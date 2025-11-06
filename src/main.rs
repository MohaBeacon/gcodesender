mod conn;
mod job_controller;

use crate::conn::Connection;
use crate::job_controller::{JobCommand, JobController};
use anyhow::Result;
use clap::Parser;
use regex::Regex;
use slint::SharedString;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::time;

slint::include_modules!();

#[derive(Parser, Debug, Clone)]
struct Args {
    #[arg(short, long, help = "Connection string (e.g., COM3:115200)")]
    conn: String,
    #[arg(long, help = "Optional path to a G-code file to load on startup")]
    file: Option<String>,
}

/// Parses the full GRBL status report string into required UI values.
fn parse_status(status: &str) -> (SharedString, f32, f32, f32, f32) {
    let state_re = Regex::new(r"<(\w+)").unwrap();
    let pos_re = Regex::new(r"MPos:([-\d.]+),[-\d.]+,([-\d.]+)").unwrap(); 
    let fs_re = Regex::new(r"FS:([-\d.]+),([-\d.]+)").unwrap(); 

    let state = state_re.captures(status)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str())
        .unwrap_or("Disconnected");

    let (x, z) = pos_re.captures(status)
        .and_then(|c| Some((c[1].parse().ok()?, c[2].parse().ok()?)))
        .unwrap_or((0.0, 0.0));

    let (f, s) = fs_re.captures(status)
        .and_then(|c| Some((c[1].parse().ok()?, c[2].parse().ok()?)))
        .unwrap_or((0.0, 0.0));

    (state.into(), x, z, f, s)
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let ui = CNC_Dashboard::new()?;
    let ui_weak = ui.as_weak();

    let (status_tx, mut status_rx) = mpsc::unbounded_channel::<String>();
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<JobCommand>();

    let conn = Arc::new(Connection::open(&args.conn).await?);
    
    // Pass the initial file from clap arguments into the JobController
    let job = JobController::new(conn.clone(), ui_weak.clone(), cmd_rx, args.file.clone());
    // Use Arc::clone() to share the controller for the spawned task
    tokio::spawn(job.clone().run_command_listener());

    if conn.send_command("?").await.as_ref().map(|s| s.contains("Alarm")).unwrap_or(false) {
        let _ = conn.send_command("$X").await;
    }

    // Status polling
    tokio::spawn({
        let conn = conn.clone();
        async move {
            loop {
                match conn.send_command("?").await {
                    Ok(s) => { let _ = status_tx.send(s); }
                    Err(_) => { let _ = status_tx.send("<Disconnected>".into()); }
                }
                time::sleep(tokio::time::Duration::from_millis(200)).await;
            }
        }
    });

    // UI updater (parsing off-thread)
    tokio::spawn({
        let ui_weak = ui_weak.clone();
        async move {
            while let Some(status) = status_rx.recv().await {
                let parsed = tokio::task::spawn_blocking(move || parse_status(&status))
                    .await
                    .unwrap_or(("Error".into(), 0.0, 0.0, 0.0, 0.0));

                let ui_w = ui_weak.clone();
                slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_w.upgrade() {
                        let (state, x, z, f, s) = parsed;
                        ui.set_state(state);
                        ui.set_pos_x(x);
                        ui.set_pos_z(z);
                        ui.set_feedrate(f);
                        ui.set_spindle(s);
                    }
                }).unwrap_or_default();
            }
        }
    });

    // --- UI Callbacks ---

    ui.on_jog({
        let tx = cmd_tx.clone();
        move |dx, dz| {
            if dx.abs() < 0.001 && dz.abs() < 0.001 { return; }
            let cmd = format!("$J=G91 X{dx:.3} Z{dz:.3} F1200");
            let _ = tx.send(JobCommand::SendImmediate(cmd));
        }
    });

    ui.on_emergency_stop({
        let tx = cmd_tx.clone();
        move || {
            let _ = tx.send(JobCommand::SendImmediate("\x18".into())); 
            let _ = tx.send(JobCommand::Stop); 
        }
    });

    ui.on_load_file({
        let tx = cmd_tx.clone();
        move || {
            if let Some(p) = rfd::FileDialog::new()
                .add_filter("G-code", &["gcode", "nc", "txt"])
                .pick_file()
            {
                let _ = tx.send(JobCommand::LoadFile(p.display().to_string()));
            }
        }
    });

    ui.on_clear_all({
        let ui_w = ui_weak.clone();
        move || {
            if let Some(ui) = ui_w.upgrade() {
                // Reset G-code viewer state
                ui.set_gcode_lines(Default::default());
                ui.set_current_line(-1); 
                ui.set_progress(0.0);
                ui.set_job_status("IDLE".into());
                // The file_path property is likely bound to the controller, so we skip setting it here.
                // We rely on status polling to update machine position/state.
            }
        }
    });
    
    ui.on_start_job({ let tx = cmd_tx.clone(); move || { let _ = tx.send(JobCommand::Start); } });
    ui.on_pause_job({ let tx = cmd_tx.clone(); move || { let _ = tx.send(JobCommand::Pause); } });
    ui.on_resume_job({ let tx = cmd_tx.clone(); move || { let _ = tx.send(JobCommand::Resume); } });
    ui.on_stop_job({ let tx = cmd_tx.clone(); move || { let _ = tx.send(JobCommand::Stop); } });

    // Z-Offset callbacks
    let job_controller_c = job.clone();
    ui.on_z_offset_submit(move |value| {
        let controller_c = job_controller_c.clone();
        tokio::task::spawn(async move {
            controller_c.set_z_offset(value).await;
        });
    });

    let job_controller_c = job.clone();
    ui.on_z_offset_cancel(move || {
        let controller_c = job_controller_c.clone();
        tokio::task::spawn(async move {
            controller_c.cancel_z_offset().await;
        });
    });

    // FIX: Changed non-existent method to standard run()
    ui.run()?;
    Ok(())
}