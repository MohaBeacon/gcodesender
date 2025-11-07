use crate::conn::Connection;
use slint::{invoke_from_event_loop, ModelRc, SharedString, StandardListViewItem, VecModel, Weak};
use std::sync::Arc;
use tokio::fs;
use tokio::sync::{mpsc, Mutex, Notify};
use tokio::task::AbortHandle;
use tokio::time::{sleep, Duration, Instant};
use regex::Regex;

use crate::CNC_Dashboard;

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
    Start, StartPaused, Pause, Resume, Stop,
    LoadFile(String), SetZOffset(f32), CancelZOffset,
    SendImmediate(String), SendNextLine,
    SetNextIndex(usize), // New command to set the starting line
}

pub struct JobController {
    conn: Arc<Connection>,
    ui_handle: CncDashboardWeak,
    gcode_lines: Arc<Mutex<Vec<String>>>,
    job_status: Arc<Mutex<JobStatus>>,
    current_job: Arc<Mutex<Option<AbortHandle>>>,
    cmd_rx: Mutex<Option<mpsc::UnboundedReceiver<JobCommand>>>,
    next_line_index: Arc<Mutex<usize>>,
    z_offset: Arc<Mutex<f32>>,
    waiting_for_offset: Arc<Mutex<bool>>,
    z_offset_notify: Arc<Notify>,
    initial_file: Option<String>,
    next_line_notify: Arc<Notify>,
}

impl JobController {
    pub fn new(
        conn: Arc<Connection>,
        ui_handle: CncDashboardWeak,
        cmd_rx: mpsc::UnboundedReceiver<JobCommand>,
        initial_file: Option<String>,
    ) -> Arc<Self> {
        Arc::new(Self {
            conn, ui_handle, cmd_rx: Mutex::new(Some(cmd_rx)),
            gcode_lines: Arc::new(Mutex::new(vec![])),
            job_status: Arc::new(Mutex::new(JobStatus::Idle)),
            current_job: Arc::new(Mutex::new(None)),
            next_line_index: Arc::new(Mutex::new(0)),
            z_offset: Arc::new(Mutex::new(0.0)),
            waiting_for_offset: Arc::new(Mutex::new(false)),
            z_offset_notify: Arc::new(Notify::new()),
            initial_file,
            next_line_notify: Arc::new(Notify::new()),
        })
    }

    pub async fn run_command_listener(self: Arc<Self>) {
        let mut rx = self.cmd_rx.lock().await.take().unwrap();
        self.update_status(JobStatus::Idle).await;

        if let Some(path) = &self.initial_file {
            self.load_file(path).await;
        }

        while let Some(cmd) = rx.recv().await {
            let this = self.clone();
            match cmd {
                JobCommand::Start => this.start_job_task(false).await,
                JobCommand::StartPaused => this.start_job_task(true).await,
                JobCommand::Pause => this.handle_pause_job().await,
                JobCommand::Resume => this.handle_resume_job().await,
                JobCommand::Stop => this.handle_stop_job().await,
                JobCommand::LoadFile(p) => this.load_file(&p).await,
                JobCommand::SetZOffset(v) => this.set_z_offset(v).await,
                JobCommand::CancelZOffset => this.cancel_z_offset().await,
                JobCommand::SendNextLine => this.handle_send_next_line().await,
                JobCommand::SendImmediate(cmd) => {
                    println!("Sending Immediate: \"{}\"", cmd.trim());
                    let conn = this.conn.clone();
                    tokio::spawn(async move { let _ = conn.send_command(&cmd).await; });
                }
                JobCommand::SetNextIndex(index) => {
                    *this.next_line_index.lock().await = index;
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

    pub fn update_gcode_ui(&self, lines: Vec<String>, path: String) {
        let path_ss: SharedString = path.into();
        let ui = self.ui_handle.clone();

        // Move the Vec<String> into the closure — it's Send + 'static
        let _ = invoke_from_event_loop(move || {
            // Build model ON THE UI THREAD
            let model = {
                let items: Vec<StandardListViewItem> = lines
                    .into_iter()
                    .map(|s| StandardListViewItem::from(SharedString::from(s)))
                    .collect();
                ModelRc::new(VecModel::from(items))
            };

            if let Some(ui) = ui.upgrade() {
                ui.set_gcode_lines(model);
                ui.set_file_path(path_ss);
            }
        });
    }

    async fn load_file(&self, path: &str) {
        let content = match fs::read_to_string(path).await {
            Ok(c) => c,
            Err(e) => {
                self.update_status(JobStatus::Error(format!("Load error: {e}"))).await;
                return;
            }
        };

        let lines: Vec<String> = content
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with(';') && !l.starts_with('('))
            .map(|l| l.to_uppercase())
            .collect();

        *self.gcode_lines.lock().await = lines.clone();
        *self.next_line_index.lock().await = 0;
        self.update_status(JobStatus::Idle).await;
        self.update_gcode_ui(lines, path.to_string());

        let _ = invoke_from_event_loop({
            let ui = self.ui_handle.clone();
            move || {
                if let Some(ui) = ui.upgrade() {
                    ui.set_current_line(-1);
                    ui.set_progress(0.0);
                }
            }
        });
    }

    async fn handle_send_next_line(&self) {
        let status = self.job_status.lock().await.clone();
        match status {
            JobStatus::Paused => {
                self.next_line_notify.notify_one();
            }
            JobStatus::Idle => {
                let lines = self.gcode_lines.lock().await.clone();
                let mut index_lock = self.next_line_index.lock().await;
                let current_index = *index_lock;

                if lines.is_empty() || current_index >= lines.len() {
                    self.update_status(JobStatus::Complete).await;
                    return;
                }

                let line = lines[current_index].clone();
                let cmd = Self::apply_offset_to_line(&line, *self.z_offset.lock().await);
                let conn = self.conn.clone();
                let ui = self.ui_handle.clone();
                let total = lines.len() as f32;

                self.update_status(JobStatus::Running).await;
                if conn.send_command(&cmd).await.is_err() {
                    self.update_status(JobStatus::Error("Send failed".into())).await;
                    return;
                }

                let idle_re = Regex::new(r"<Idle").unwrap();
                let deadline = Instant::now() + Duration::from_secs(30);
                while Instant::now() < deadline {
                    if let Ok(report) = conn.send_command("?").await {
                        if idle_re.is_match(&report) { break; }
                    }
                    sleep(Duration::from_millis(80)).await;
                }

                *index_lock += 1;
                let progress = *index_lock as f32 / total * 100.0;
                let _ = invoke_from_event_loop(move || {
                    if let Some(ui) = ui.upgrade() {
                        ui.set_current_line(current_index as i32);
                        ui.set_progress(progress);
                    }
                });

                if *index_lock >= lines.len() {
                    self.update_status(JobStatus::Complete).await;
                } else {
                    self.update_status(JobStatus::Idle).await;
                }
            }
            _ => {}
        }
    }

    fn apply_offset_to_line(line: &str, offset: f32) -> String {
        if offset.abs() < 1e-6 { return line.to_string(); }
        let re = Regex::new(r"(Z[+-]?\d*\.?\d*)").unwrap();
        re.replace_all(line, |caps: &regex::Captures| {
            let old: f32 = caps[1][1..].parse().unwrap_or(0.0);
            format!("Z{:.3}", old + offset)
        }).into()
    }

    async fn start_job_task(&self, initial_paused: bool) {
        let lines = self.gcode_lines.lock().await.clone();
        if lines.is_empty() {
            self.update_status(JobStatus::Error("No G-code loaded".into())).await;
            return;
        }

        if let Some(h) = self.current_job.lock().await.take() { h.abort(); }

        let initial_status = if initial_paused { JobStatus::Paused } else { JobStatus::Running };
        self.update_status(initial_status.clone()).await;

        if initial_paused {
            let _ = self.conn.send_command("!").await;
        }

        let conn = self.conn.clone();
        let status = self.job_status.clone();
        let ui = self.ui_handle.clone();
        let job_store = self.current_job.clone();
        let next_index = self.next_line_index.clone();
        let z_offset = self.z_offset.clone();
        let notify = self.z_offset_notify.clone();
        let next_line_notify = self.next_line_notify.clone();
        let total = lines.len() as f32;
        let mut i = *next_index.lock().await;

        let handle = tokio::spawn(async move {
            let _ = conn.send_command("$X").await;
            let idle_re = Regex::new(r"<Idle").unwrap();

            while i < lines.len() {
                let line = &lines[i];

                loop {
                    let s = status.lock().await.clone();
                    match s {
                        JobStatus::Stopping | JobStatus::Error(_) => return,
                        JobStatus::Running => break,
                        JobStatus::Paused => { drop(s); next_line_notify.notified().await; },
                        JobStatus::WaitingForZOffset => { drop(s); notify.notified().await; },
                        _ => break,
                    }
                }

                if matches!(*status.lock().await, JobStatus::Stopping | JobStatus::Error(_)) { break; }

                if line.contains("G54") {
                    *status.lock().await = JobStatus::WaitingForZOffset;
                    let _ = invoke_from_event_loop({ let ui = ui.clone(); move || ui.upgrade().unwrap().set_show_z_offset_popup(true) });
                    notify.notified().await;
                    let _ = invoke_from_event_loop({ let ui = ui.clone(); move || ui.upgrade().unwrap().set_show_z_offset_popup(false) });
                }

                let cmd = Self::apply_offset_to_line(line, *z_offset.lock().await);
                if conn.send_command(&cmd).await.is_err() {
                    *status.lock().await = JobStatus::Error("Send failed".into());
                    break;
                }

                let deadline = Instant::now() + Duration::from_secs(30);
                while Instant::now() < deadline {
                    if let Ok(r) = conn.send_command("?").await {
                        if idle_re.is_match(&r) { break; }
                    }
                    sleep(Duration::from_millis(80)).await;
                }

                let progress = (i + 1) as f32 / total * 100.0;
                *next_index.lock().await = i + 1;
                let _ = invoke_from_event_loop({
                    let ui = ui.clone();
                    let line_idx = i;
                    move || {
                        let ui = ui.upgrade().unwrap();
                        ui.set_current_line(line_idx as i32);
                        ui.set_progress(progress);
                    }
                });
                i += 1;
            }

            let final_status = if *status.lock().await == JobStatus::Stopping {
                JobStatus::Idle
            } else if matches!(*status.lock().await, JobStatus::Error(_)) {
                status.lock().await.clone()
            } else {
                JobStatus::Complete
            };

            *status.lock().await = final_status.clone();
            let _ = invoke_from_event_loop({
                let ui = ui.clone();
                move || {
                    if let Some(ui) = ui.upgrade() {
                        ui.set_current_line(-1);
                        ui.set_progress(if final_status == JobStatus::Idle { 0.0 } else { 100.0 });
                        ui.set_job_status(final_status.to_slint_string());
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
            self.next_line_notify.notify_one();
            let _ = self.conn.send_command("~").await;
        }
    }

    async fn handle_stop_job(&self) {
        if matches!(*self.job_status.lock().await, JobStatus::Running | JobStatus::Paused | JobStatus::WaitingForZOffset) {
            self.update_status(JobStatus::Stopping).await;
            if let Some(h) = self.current_job.lock().await.take() { h.abort(); }
            let _ = self.conn.send_command("\x18").await;
            sleep(Duration::from_millis(150)).await;
            let _ = self.conn.send_command("$X").await;
            *self.next_line_index.lock().await = 0;
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