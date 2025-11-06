mod conn;
mod job_controller;
mod slint_generated {
    slint::include_modules!();
}

use crate::conn::Connection;
use crate::job_controller::{JobCommand, JobController};
use crate::slint_generated::CNC_Dashboard;
use anyhow::Result;
use clap::Parser;
use regex::Regex;
use slint::SharedString;
use slint::ComponentHandle;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::time::{sleep, Duration};

#[derive(Parser, Debug)]
struct Args {
    #[arg(short, long, default_value = "COM3")] conn: String,
    #[arg(long)] file: Option<String>,
}

fn parse_status(status: &str) -> (SharedString, f32, f32, f32, f32) {
    let state_re = Regex::new(r"<(\w+)|^\[?([A-Za-z]+)").unwrap();
    // Two capture groups: Group 1 is X, Group 2 is Z
    let pos_re = Regex::new(r"[MW]Pos:([-\d.]+),[-\d.]+,([-\d.]+)").unwrap();
    let fs_re = Regex::new(r"FS?:([-\d.]+),([-\d.]+)").unwrap();

    let state = state_re.captures(status).and_then(|c| c.get(1).or(c.get(2))).map(|m| m.as_str()).unwrap_or("Disconnected");
    // FIX: Accessing c[2] (Z) instead of the non-existent c[3]
    let (x, z) = pos_re.captures(status).and_then(|c| Some((c[1].parse().ok()?, c[2].parse().ok()?))).unwrap_or((0.0, 0.0));
    let (f, s) = fs_re.captures(status).and_then(|c| Some((c[1].parse().ok()?, c[2].parse().ok()?))).unwrap_or((0.0, 0.0));

    (state.into(), x, z, f, s)
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let ui = CNC_Dashboard::new()?;
    let ui_weak = ui.as_weak();

    let conn = Arc::new(Connection::open(&args.conn).await?);
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (status_tx, mut status_rx) = mpsc::unbounded_channel();

    if conn.send_command("?").await.as_ref().map_or(false, |s| s.contains("ALARM")) {
        let _ = conn.send_command("$X").await;
    }

    // Capture the optional file argument
    let initial_file = args.file;
    
    // Pass the file path to JobController::new
    let controller = JobController::new(conn.clone(), ui_weak.clone(), cmd_rx, initial_file);
    let listener_controller = controller.clone();
tokio::spawn(async move {
    listener_controller.run_command_listener().await;
});

    // REMOVED: Initial cmd_tx.send(JobCommand::LoadFile(p)) is now handled inside the JobController.

    tokio::spawn({
        let conn = conn.clone();
        async move {
            loop {
                let s = conn.send_command("?").await.unwrap_or("<Disconnected>".into());
                let _ = status_tx.send(s);
                sleep(Duration::from_millis(150)).await;
            }
        }
    });

    tokio::spawn({
        let ui_weak = ui_weak.clone();
        async move {
            while let Some(raw) = status_rx.recv().await {
                let parsed = tokio::task::spawn_blocking(move || parse_status(&raw)).await.unwrap();
                let ui_w = ui_weak.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_w.upgrade() {
                        let (state, x, z, f, s) = parsed;
                        ui.set_state(state); ui.set_pos_x(x); ui.set_pos_z(z);
                        ui.set_feedrate(f); ui.set_spindle(s);
                    }
                });
            }
        }
    });

    ui.on_load_file({
        let tx = cmd_tx.clone();
        move || {
            if let Some(p) = rfd::FileDialog::new().pick_file() {
                let _ = tx.send(JobCommand::LoadFile(p.display().to_string()));
            }
        }
    });

    ui.on_start_job({
        let tx = cmd_tx.clone();
        move || { let _ = tx.send(JobCommand::Start); }
    });
    ui.on_pause_job({
        let tx = cmd_tx.clone();
        move || { let _ = tx.send(JobCommand::Pause); }
    });
    ui.on_resume_job({
        let tx = cmd_tx.clone();
        move || { let _ = tx.send(JobCommand::Resume); }
    });
    ui.on_stop_job({
        let tx = cmd_tx.clone();
        move || { let _ = tx.send(JobCommand::Stop); }
    });

    ui.on_jog({
        let tx = cmd_tx.clone();
        move |dx, dz| {
            if dx.abs() > 0.001 || dz.abs() > 0.001 {
                let _ = tx.send(JobCommand::SendImmediate(format!("$J=G91 X{dx:.3} Z{dz:.3} F1800")));
            }
        }
    });

    ui.on_emergency_stop({
        let tx = cmd_tx.clone();
        move || {
            let _ = tx.send(JobCommand::SendImmediate("\x18".into()));
            let _ = tx.send(JobCommand::SendImmediate("$X".into()));
        }
    });

    let controller_weak = Arc::downgrade(&controller);
    let ui_weak_c = ui_weak.clone();
    ui.on_z_offset_submit(move |val| {
        let c = controller_weak.clone();
        let ui = ui_weak_c.clone();
        tokio::spawn(async move {
            if let Some(ctrl) = c.upgrade() {
                ctrl.set_z_offset(val).await;
                let _ = slint::invoke_from_event_loop(move || {
                    ui.upgrade().unwrap().set_show_z_offset_popup(false);
                });
            }
        });
    });

    let controller_weak = Arc::downgrade(&controller);
    let ui_weak_c = ui_weak.clone();
    ui.on_z_offset_cancel(move || {
        let c = controller_weak.clone();
        let ui = ui_weak_c.clone();
        tokio::spawn(async move {
            if let Some(ctrl) = c.upgrade() {
                ctrl.cancel_z_offset().await;
                let _ = slint::invoke_from_event_loop(move || {
                    ui.upgrade().unwrap().set_show_z_offset_popup(false);
                });
            }
        });
    });

    ui.on_clear_all(move || {
        if let Some(ui) = ui_weak.upgrade() {
            ui.set_state("Disconnected".into());
            ui.set_pos_x(0.0); ui.set_pos_z(0.0);
            ui.set_progress(0.0); ui.set_current_line(-1);
            ui.set_job_status("IDLE".into());
        }
    });

    ui.run().unwrap();
    Ok(())
}