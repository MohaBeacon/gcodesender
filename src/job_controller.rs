use crate::conn::Connection;
// FIX: Removed unused import 'SharedVector'
use slint::{SharedString, Weak, invoke_from_event_loop, ModelRc, VecModel}; 
use std::sync::Arc;
use tokio::fs;
use tokio::sync::{mpsc, Mutex, Notify};
use tokio::task::AbortHandle;
use tokio::time::{self, sleep, Duration, Instant}; 
use regex::Regex;

// FIX: Access the generated component type directly from the crate root
use crate::CNC_Dashboard;

// FIX: Renamed type alias to follow standard Rust conventions
type CncDashboardWeak = Weak<CNC_Dashboard>; 

#[derive(Debug, Clone, PartialEq)]
pub enum JobStatus {
    Idle, Running, Paused, Stopping, Complete, Error(String), WaitingForZOffset,
}

impl JobStatus {
    pub fn to_slint_string(&self) -> SharedString {
        match self {
            JobStatus::Idle => "IDLE".into(),
            JobStatus::Running => "RUNNING".into(),
            JobStatus::Paused => "PAUSED".into(),
            JobStatus::Stopping => "STOPPING".into(),
            JobStatus::Complete => "COMPLETE".into(),
            JobStatus::Error(e) => format!("ERROR: {e}").into(),
            JobStatus::WaitingForZOffset => "WAITING FOR Z OFFSET".into(),
        }
    }
}

#[derive(Debug, Clone)]
pub enum JobCommand {
    Start, Pause, Resume, Stop,
    LoadFile(String),
    SetZOffset(f32),
    CancelZOffset,
    SendImmediate(String),
}

pub struct JobController {
    conn: Arc<Connection>,
    // FIX: Use new type alias name
    ui_handle: CncDashboardWeak, 
    gcode_lines: Arc<Mutex<Vec<String>>>,
    job_status: Arc<Mutex<JobStatus>>,
    current_job: Arc<Mutex<Option<AbortHandle>>>,
    cmd_rx: Mutex<Option<mpsc::UnboundedReceiver<JobCommand>>>, 

    z_offset: Arc<Mutex<f32>>,
    waiting_for_offset: Arc<Mutex<bool>>,
    z_offset_notify: Arc<Notify>,
    initial_file: Option<String>,
}

impl JobController {
    pub fn new(
        conn: Arc<Connection>,
        // FIX: Use new type alias name
        ui_handle: CncDashboardWeak,
        cmd_rx: mpsc::UnboundedReceiver<JobCommand>,
        initial_file: Option<String>, 
    ) -> Arc<Self> {
        Arc::new(Self {
            conn,
            ui_handle,
            gcode_lines: Arc::new(Mutex::new(vec![])),
            job_status: Arc::new(Mutex::new(JobStatus::Idle)),
            current_job: Arc::new(Mutex::new(None)),
            cmd_rx: Mutex::new(Some(cmd_rx)),
            z_offset: Arc::new(Mutex::new(0.0)),
            waiting_for_offset: Arc::new(Mutex::new(false)),
            z_offset_notify: Arc::new(Notify::new()),
            initial_file,
        })
    }

    pub async fn run_command_listener(self: Arc<Self>) {
        let mut rx = self.cmd_rx.lock().await.take().unwrap();
        self.update_status(JobStatus::Idle).await;

        if let Some(path) = self.initial_file.clone() {
             self.load_file(&path).await;
        }

        while let Some(cmd) = rx.recv().await {
            let this = self.clone(); 
            match cmd {
                JobCommand::Start => this.handle_start().await,
                JobCommand::Pause => this.handle_pause_job().await,
                JobCommand::Resume => this.handle_resume_job().await,
                JobCommand::Stop => this.handle_stop_job().await,
                JobCommand::LoadFile(path) => this.load_file(&path).await,
                JobCommand::SetZOffset(v) => this.set_z_offset(v).await,
                JobCommand::CancelZOffset => this.cancel_z_offset().await,
                JobCommand::SendImmediate(cmd) => {
                    // ADDED: Print immediate commands sent to the machine
                    println!("Sending Immediate Command: \"{}\"", cmd.trim()); 
                    let conn = this.conn.clone();
                    tokio::spawn(async move {
                        let _ = conn.send_command(&cmd).await; 
                    });
                }
            }
        }
    }

    async fn update_status(&self, status: JobStatus) {
        *self.job_status.lock().await = status.clone();
        let ui = self.ui_handle.clone();
        let s = status.to_slint_string();
        let _ = invoke_from_event_loop(move || {
            if let Some(ui) = ui.upgrade() {
                ui.set_job_status(s);
            }
        });
    }

    async fn load_file(&self, path: &str) {
        let content = match fs::read_to_string(path).await {
            Ok(c) => c,
            Err(e) => {
                self.update_status(JobStatus::Error(format!("Load error: {}", e))).await;
                return;
            }
        };

        // Filter and clean G-code lines
        let lines: Vec<String> = content
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with(';') && !l.starts_with('('))
            .map(|l| l.to_uppercase())
            .collect();
        
        let line_count = lines.len();

        // 1. Update internal state
        *self.gcode_lines.lock().await = lines;
        self.update_status(JobStatus::Idle).await;

        // 2. Update Slint UI properties
        let ui = self.ui_handle.clone();
        let path_ss = path.to_string().into();

        // Collect the actual data (which is Send) to be moved into the event loop
        let lines_data: Vec<SharedString> = self.gcode_lines.lock().await.iter()
            .map(|s| s.clone().into())
            .collect();
            
        // FIX: Now we move the lines_data (Vec<SharedString>) into the event loop closure.
        // The ModelRc creation happens *inside* the closure, on the UI thread,
        // resolving the Send trait error.
        let _ = invoke_from_event_loop(move || {
            let lines_model: ModelRc<SharedString> = ModelRc::new(VecModel::from(lines_data));

            if let Some(ui) = ui.upgrade() {
                ui.set_gcode_lines(lines_model);
                ui.set_file_path(path_ss); 
                ui.set_current_line(-1);
                ui.set_progress(0.0);
                ui.set_job_status("IDLE".into());
                println!("Loaded {} G-code lines into UI.", line_count);
            }
        });
    }

    fn apply_offset_to_line(line: &str, offset: f32) -> String {
        if offset.abs() < 1e-6 { return line.to_string(); }
        // Regex to find Z followed by an optional sign and floating point number
        let re = Regex::new(r"(Z[+-]?\d*\.?\d*)").unwrap();
        re.replace_all(line, |caps: &regex::Captures| {
            // Parse the existing Z value (excluding the 'Z')
            let old: f32 = caps[1][1..].parse().unwrap_or(0.0);
            // MODIFIED: Changed format to "{:.2}" to ensure only 2 decimal points are output.
            format!("Z{:.2}", old + offset)
        }).to_string()
    }

    async fn handle_start(&self) {
        let lines = self.gcode_lines.lock().await.clone();
        if lines.is_empty() { 
            self.update_status(JobStatus::Error("No G-code loaded".into())).await;
            return; 
        }

        if let Some(h) = self.current_job.lock().await.take() { h.abort(); }
        self.update_status(JobStatus::Running).await;

        let conn = self.conn.clone();
        let status = self.job_status.clone();
        let ui = self.ui_handle.clone();
        let job_store = self.current_job.clone();
        let z_offset = self.z_offset.clone();
        let waiting = self.waiting_for_offset.clone();
        let notify = self.z_offset_notify.clone();
        let total = lines.len() as f32;

        let handle = tokio::spawn(async move {
            // FIX: Only wait for Idle state to ensure the previous motion is complete.
            let idle_re = regex::Regex::new(r"<Idle").unwrap();

            for (i, line) in lines.iter().enumerate() {
                let current_status = status.lock().await.clone();
                while current_status == JobStatus::Paused || current_status == JobStatus::WaitingForZOffset {
                    sleep(Duration::from_millis(100)).await;
                    if matches!(*status.lock().await, JobStatus::Stopping | JobStatus::Error(_)) { break; }
                }
                if matches!(*status.lock().await, JobStatus::Stopping | JobStatus::Error(_)) { break; }

                // MODIFIED: Check for "G54" instead of "G92"
                if line.contains("G54") {
                    *status.lock().await = JobStatus::WaitingForZOffset;
                    let _ = invoke_from_event_loop({
                        let ui = ui.clone();
                        move || ui.upgrade().unwrap().set_show_z_offset_popup(true)
                    });

                    *waiting.lock().await = true;
                    notify.notified().await;
                    *waiting.lock().await = false;

                    let _ = invoke_from_event_loop({
                        let ui = ui.clone();
                        move || ui.upgrade().unwrap().set_show_z_offset_popup(false)
                    });
                    
                    if matches!(*status.lock().await, JobStatus::Stopping | JobStatus::Error(_)) { break; }
                }
                
                if *status.lock().await != JobStatus::Running {
                    *status.lock().await = JobStatus::Running;
                }

                // Get the command with the applied Z-offset
                let cmd = Self::apply_offset_to_line(line, *z_offset.lock().await);
                
                // ADDED: Print statement for the command being sent
                println!("Sending Line {}: Original: \"{}\", Modified: \"{}\"", i + 1, line, cmd); 

                if conn.send_command(&cmd).await.is_err() {
                    *status.lock().await = JobStatus::Error("Send failed".into());
                    break;
                }

                // Polling loop now strictly waits for the machine to return to the <Idle state
                let deadline = Instant::now() + Duration::from_secs(30);
                while Instant::now() < deadline {
                    let status_report = conn.send_command("?").await.unwrap_or_default();
                    if idle_re.is_match(&status_report) {
                        break;
                    }
                    sleep(Duration::from_millis(80)).await;
                }

                let progress = (i + 1) as f32 / total * 100.0;
                let _ = invoke_from_event_loop({
                    let ui = ui.clone();
                    move || {
                        let ui = ui.upgrade().unwrap();
                        ui.set_current_line(i as i32);
                        ui.set_progress(progress);
                    }
                });
            }

            let job_final = if *status.lock().await == JobStatus::Stopping { 
                JobStatus::Idle 
            } else if matches!(*status.lock().await, JobStatus::Error(_)) {
                status.lock().await.clone()
            } else { 
                JobStatus::Complete 
            };
            
            *status.lock().await = job_final.clone();

            let _ = invoke_from_event_loop({
                let ui = ui.clone();
                move || {
                    if let Some(ui) = ui.upgrade() {
                        let is_idle = job_final == JobStatus::Idle;
                        ui.set_current_line(-1);
                        ui.set_progress(if is_idle { 0.0 } else { 100.0 });
                        ui.set_job_status(job_final.to_slint_string());
                    }
                }
            });
        });

        *job_store.lock().await = Some(handle.abort_handle());
    }

    async fn handle_pause_job(&self) {
        if *self.job_status.lock().await == JobStatus::Running {
            self.update_status(JobStatus::Paused).await;
            let _ = self.conn.send_command("!").await; 
        }
    }

    async fn handle_resume_job(&self) {
        if *self.job_status.lock().await == JobStatus::Paused {
            self.update_status(JobStatus::Running).await;
            let _ = self.conn.send_command("~").await; 
        }
    }

    async fn handle_stop_job(&self) {
        if matches!(*self.job_status.lock().await, JobStatus::Running | JobStatus::Paused | JobStatus::Error(_) | JobStatus::WaitingForZOffset) {
            self.update_status(JobStatus::Stopping).await;
            
            if let Some(h) = self.current_job.lock().await.take() {
                h.abort();
            }
            
            let _ = self.conn.send_command("\x18").await;
            time::sleep(Duration::from_millis(150)).await;
            let _ = self.conn.send_command("$X").await;

            self.update_status(JobStatus::Idle).await;
        }
    }

    pub async fn set_z_offset(&self, offset: f32) {
        *self.z_offset.lock().await = offset;
        self.z_offset_notify.notify_one();
        self.update_status(JobStatus::Running).await;
    }

    pub async fn cancel_z_offset(&self) {
        self.z_offset_notify.notify_one();
        self.update_status(JobStatus::Paused).await; 
    }
}