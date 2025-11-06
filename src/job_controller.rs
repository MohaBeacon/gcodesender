// src/job_controller.rs
use crate::conn::Connection;
use slint::{SharedString, Weak, invoke_from_event_loop};
use std::sync::Arc;
use tokio::fs;
use tokio::sync::{mpsc, Mutex, Notify};
use tokio::task::AbortHandle;
use tokio::time::{sleep, Duration, Instant};
use regex::Regex;

#[derive(Debug, Clone, PartialEq)]
pub enum JobStatus {
    Idle, Running, Paused, Stopping, Complete, Error(String), WaitingForZOffset,
}

impl JobStatus {
    pub fn to_slint_string(&self) -> SharedString {
        match self {
            JobStatus::Idle => "Idle".into(),
            JobStatus::Running => "Running".into(),
            JobStatus::Paused => "Paused".into(),
            JobStatus::Stopping => "Stopping".into(),
            JobStatus::Complete => "Complete".into(),
            JobStatus::Error(e) => format!("Error: {e}").into(),
            JobStatus::WaitingForZOffset => "Waiting for Z-Offset".into(),
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
    ui_handle: Weak<crate::slint_generated::CNC_Dashboard>,
    gcode_lines: Arc<Mutex<Vec<String>>>,
    job_status: Arc<Mutex<JobStatus>>,
    current_job: Arc<Mutex<Option<AbortHandle>>>,
    cmd_rx: Mutex<Option<mpsc::UnboundedReceiver<JobCommand>>>,

    z_offset: Arc<Mutex<f32>>,
    waiting_for_offset: Arc<Mutex<bool>>,
    z_offset_notify: Arc<Notify>,
    // NEW FIELD
    initial_file: Option<String>,
}

impl JobController {
    pub fn new(
        conn: Arc<Connection>,
        ui_handle: Weak<crate::slint_generated::CNC_Dashboard>,
        cmd_rx: mpsc::UnboundedReceiver<JobCommand>,
        // NEW ARGUMENT
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
            // INITIALIZE NEW FIELD
            initial_file,
        })
    }

    pub async fn run_command_listener(self: Arc<Self>) {
        let mut rx = self.cmd_rx.lock().await.take().unwrap();
        self.update_status(JobStatus::Idle).await;

        // FIX: Load the initial file immediately after the controller is ready.
        if let Some(path) = self.initial_file.clone() {
             self.load_file(&path).await;
        }

        while let Some(cmd) = rx.recv().await {
            let this = self.clone();
            match cmd {
                JobCommand::Start => this.handle_start().await,
                JobCommand::Pause => *this.job_status.lock().await = JobStatus::Paused,
                JobCommand::Resume => *this.job_status.lock().await = JobStatus::Running,
                JobCommand::Stop => *this.job_status.lock().await = JobStatus::Stopping,
                JobCommand::LoadFile(path) => this.load_file(&path).await,
                JobCommand::SetZOffset(v) => this.set_z_offset(v).await,
                JobCommand::CancelZOffset => this.cancel_z_offset().await,
                JobCommand::SendImmediate(cmd) => {
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
            Err(_) => return self.update_status(JobStatus::Error("File not found".into())).await,
        };

        let lines: Vec<String> = content
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with(';') && !l.starts_with('('))
            .map(|l| l.to_uppercase())
            .collect();

        *self.gcode_lines.lock().await = lines;
        self.update_status(JobStatus::Idle).await;
    }

    fn apply_offset_to_line(line: &str, offset: f32) -> String {
        if offset.abs() < 1e-6 { return line.to_string(); }
        let re = Regex::new(r"(Z[+-]?\d*\.?\d*)").unwrap();
        re.replace_all(line, |caps: &regex::Captures| {
            let old: f32 = caps[1][1..].parse().unwrap_or(0.0);
            format!("Z{:.4}", old + offset)
        }).to_string()
    }

    async fn handle_start(&self) {
        let lines = self.gcode_lines.lock().await.clone();
        if lines.is_empty() { return; }

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
            for (i, line) in lines.iter().enumerate() {
                while *status.lock().await == JobStatus::Paused {
                    sleep(Duration::from_millis(100)).await;
                }
                if matches!(*status.lock().await, JobStatus::Stopping | JobStatus::Error(_)) {
                    break;
                }

                if line.contains("G92") {
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
                }

                let cmd = Self::apply_offset_to_line(line, *z_offset.lock().await);
                if conn.send_command(&cmd).await.is_err() {
                    *status.lock().await = JobStatus::Error("Send failed".into());
                    break;
                }

                let deadline = Instant::now() + Duration::from_secs(30);
                while Instant::now() < deadline {
                    if conn.send_command("?").await.map(|r| r.contains("<Idle")).unwrap_or(false) {
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

            let job_final = if *status.lock().await == JobStatus::Stopping { JobStatus::Idle } else { JobStatus::Complete };
            *status.lock().await = job_final.clone();
            let _ = invoke_from_event_loop({
                let ui = ui.clone();
                move || {
                    let ui = ui.upgrade().unwrap();
                    ui.set_current_line(-1);
                    ui.set_progress(if job_final == JobStatus::Idle { 0.0 } else { 100.0 });
                    ui.set_job_status(job_final.to_slint_string());
                }
            });
        });

        *job_store.lock().await = Some(handle.abort_handle());
    }

    pub async fn set_z_offset(&self, offset: f32) {
        *self.z_offset.lock().await = offset;
        *self.waiting_for_offset.lock().await = false;
        self.z_offset_notify.notify_one();
        self.update_status(JobStatus::Running).await;
    }

    pub async fn cancel_z_offset(&self) {
        *self.waiting_for_offset.lock().await = false;
        self.z_offset_notify.notify_one();
        self.update_status(JobStatus::Paused).await;
    }
}