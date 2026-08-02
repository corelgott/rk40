//! rk40 — single-user web server to drive a K40 laser cutter (M2 Nano).
//!
//! Usage: rk40 [--port 8040] [--data-dir <dir>]
//!
//! Works fully offline: SVGs can be prepared and stored without the laser
//! attached; connecting the device is only needed to actually print.

mod api;
mod device;
mod egv;
mod jobs;
mod laser_speed;
mod planner;

use std::path::PathBuf;
use std::sync::Arc;

fn default_data_dir() -> PathBuf {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".rk40")
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut port: u16 = 8040;
    let mut data_dir = default_data_dir();

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--port" | "-p" => {
                port = args
                    .next()
                    .and_then(|v| v.parse().ok())
                    .ok_or_else(|| anyhow::anyhow!("--port braucht eine Zahl"))?;
            }
            "--data-dir" | "-d" => {
                data_dir = args
                    .next()
                    .map(PathBuf::from)
                    .ok_or_else(|| anyhow::anyhow!("--data-dir braucht einen Pfad"))?;
            }
            "--help" | "-h" => {
                println!("rk40 [--port 8040] [--data-dir ~/.rk40]");
                return Ok(());
            }
            other => anyhow::bail!("Unbekanntes Argument: {other}"),
        }
    }

    std::fs::create_dir_all(&data_dir)?;
    let jobs = jobs::JobStore::new(&data_dir)?;
    let device = device::spawn_device_thread();

    // try to connect on startup, but don't require it (offline mode)
    let _ = device.send(device::Command::Connect);

    let device_for_shutdown = device.clone();
    let state = Arc::new(api::AppState { device, jobs });
    let app = api::router(state);

    let addr = std::net::SocketAddr::from(([0, 0, 0, 0], port));
    let listener = tokio::net::TcpListener::bind(addr).await?;
    println!("rk40 läuft auf http://localhost:{port}  (Daten: {})", data_dir.display());

    // Ctrl+C beendet sofort. Kein graceful shutdown: die offenen SSE-Streams
    // der Browser-Tabs schließen nie, damit würde das Warten endlos hängen.
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        eprintln!("\nrk40 wird beendet…");
        if device_for_shutdown.status_snapshot().busy {
            eprintln!("Laufender Job wird gestoppt (Not-Stopp an den Laser)…");
            device_for_shutdown.request_stop();
            tokio::time::sleep(std::time::Duration::from_millis(800)).await;
        }
        std::process::exit(0);
    });

    axum::serve(listener, app).await?;
    Ok(())
}
