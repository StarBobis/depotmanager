//! Full download-engine smoke test: resolves depots, downloads manifests,
//! validates files, downloads chunks in parallel with progress output.
//!
//! Run: cargo run --bin smoke3 -- [appid] [depotid] [filter_regex]
//! Example: cargo run --bin smoke3 -- 730 2347770 "\.txt$"

use std::sync::Arc;

use depotmanager_lib::steamkit::cdn::CdnClient;
use depotmanager_lib::steamkit::connection::{CmConnection, ConnectionEvent};
use depotmanager_lib::steamkit::pics::ProductInfoService;
use depotmanager_lib::steamkit::{machineid, session::SteamSession};
use depotmanager_lib::engine::{DownloadRequest, Engine, EngineEvent, TaskState};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let app_id: u32 = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(730);
    let depot_id: u32 = std::env::args().nth(2).and_then(|s| s.parse().ok()).unwrap_or(2347770);
    let filter = std::env::args().nth(3).unwrap_or_else(|| r"\.txt$".to_string());
    let manifest_id: Option<u64> = std::env::args().nth(4).and_then(|s| s.parse().ok()).filter(|v| *v > 0);

    let (events, mut rx) = tokio::sync::broadcast::channel::<ConnectionEvent>(64);
    tokio::spawn(async move {
        while let Ok(ev) = rx.recv().await {
            match ev {
                ConnectionEvent::LoggedOn { .. } => println!("[conn] {:?}", ev),
                ConnectionEvent::Log(l) => println!("[conn] {}", l),
                _ => {}
            }
        }
    });

    let endpoints = CmConnection::fetch_cm_list(0).await.unwrap_or_else(|_| {
        depotmanager_lib::steamkit::connection::FALLBACK_CM_ENDPOINTS
            .iter()
            .map(|s| s.to_string())
            .collect()
    });
    let conn = CmConnection::connect(&endpoints, events.clone()).await?;
    let session = SteamSession::new(conn.clone(), machineid::build_machine_id(b"smoke-test-3"), 0);
    session.logon_anonymous().await?;
    println!("==> logged on (anonymous)");

    let pics = Arc::new(ProductInfoService::new(conn.clone()));
    let cdn = Arc::new(CdnClient::new(conn.clone())?);

    let (engine_events, mut engine_rx) = tokio::sync::broadcast::channel::<EngineEvent>(1024);
    tokio::spawn(async move {
        while let Ok(ev) = engine_rx.recv().await {
            if let EngineEvent::Log { line, .. } = ev {
                println!("[task] {}", line);
            }
        }
    });

    let engine = Engine::new(conn.clone(), pics, cdn, engine_events);

    let install_dir = std::path::PathBuf::from(format!(
        "D:\\Dev\\depotmanager\\test-download\\{}-{}",
        app_id, depot_id
    ));
    let _ = std::fs::create_dir_all(&install_dir);

    let request = DownloadRequest {
        app_id,
        depot_ids: vec![depot_id],
        branch: "public".into(),
        manifest_id,
        install_dir: install_dir.clone(),
        os: "windows".into(),
        arch: "64".into(),
        language: "english".into(),
        all_languages: false,
        file_filters: vec![filter.clone()],
        verify_only: false,
        max_connections: 24,
    };

    println!("==> starting download: app {} depot {} filter /{}/", app_id, depot_id, filter);
    let started = std::time::Instant::now();
    let task_id = engine.start_download(request);

    let mut last_print = std::time::Instant::now();
    loop {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let Some(task) = engine.task(&task_id) else { break };
        let snap = task.snapshot();
        if last_print.elapsed().as_secs() >= 2 {
            last_print = std::time::Instant::now();
            println!(
                "    [{:?}] {}/{} bytes ({:.1} MB/s) files {}/{}",
                snap.state,
                snap.progress.downloaded_bytes,
                snap.progress.total_bytes,
                snap.progress.speed_bps / 1e6,
                snap.progress.done_files,
                snap.progress.total_files,
            );
        }
        match snap.state {
            TaskState::Completed => {
                println!("==> COMPLETED in {:?}", started.elapsed());
                break;
            }
            TaskState::Failed => {
                println!("==> FAILED: {:?}", snap.error);
                std::process::exit(1);
            }
            TaskState::Canceled => {
                println!("==> CANCELED");
                std::process::exit(1);
            }
            _ => {}
        }
    }

    // list downloaded files
    for entry in walkdir(&install_dir) {
        let meta = std::fs::metadata(&entry).ok();
        println!("    file: {} ({} bytes)", entry.display(), meta.map(|m| m.len()).unwrap_or(0));
    }

    session.disconnect();
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    Ok(())
}

fn walkdir(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                if p.file_name().map(|n| n != ".depotmanager").unwrap_or(false) {
                    out.extend(walkdir(&p));
                }
            } else {
                out.push(p);
            }
        }
    }
    out
}
