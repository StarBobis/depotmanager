//! Smoke test for the Steam protocol stack (no Tauri GUI):
//! connects to a CM, logs on anonymously, fetches app info.
//!
//! Run: cargo run --bin smoke -- [appid]

use depotmanager_lib::steamkit::connection::{CmConnection, ConnectionEvent};
use depotmanager_lib::steamkit::pics::ProductInfoService;
use depotmanager_lib::steamkit::{machineid, session::SteamSession};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let app_id: u32 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(570);

    let (events, mut rx) = tokio::sync::broadcast::channel::<ConnectionEvent>(64);

    tokio::spawn(async move {
        while let Ok(ev) = rx.recv().await {
            println!("[event] {:?}", ev);
        }
    });

    println!("==> fetching CM list...");
    let endpoints = CmConnection::fetch_cm_list(0)
        .await
        .unwrap_or_else(|_| {
            depotmanager_lib::steamkit::connection::FALLBACK_CM_ENDPOINTS
                .iter()
                .map(|s| s.to_string())
                .collect()
        });
    println!("==> {} endpoints, connecting...", endpoints.len());

    let conn = CmConnection::connect(&endpoints, events.clone()).await?;
    println!("==> connected");

    let session = SteamSession::new(conn.clone(), machineid::build_machine_id(b"smoke-test"), 0);

    println!("==> anonymous logon...");
    let steamid = session.logon_anonymous().await?;
    println!("==> logged on, steamid = {steamid}");

    let pics = ProductInfoService::new(conn.clone());
    println!("==> requesting app info for {app_id}...");
    match pics.app_info(app_id).await? {
        Some(info) => {
            println!("==> app name: {:?}", info.name());
            println!("==> change number: {}", info.change_number);
            println!("==> missing_token: {} only_public: {}", info.missing_token, info.only_public);
            println!("==> token cached: {:?}", pics.app_tokens.get(&app_id).map(|v| *v));
            println!("==> free to download: {}", info.free_to_download());
            println!("==> branches:");
            for (name, buildid, desc) in info.branches() {
                println!("    {:<20} build {:<12} {:?}", name, buildid, desc);
            }
            if let Some(depots) = info.section("depots") {
                let count = depots
                    .children
                    .iter()
                    .filter(|c| c.name.chars().all(|ch| ch.is_ascii_digit()))
                    .count();
                println!("==> depots: {}", count);
                println!("==> depots children: {:?}", depots.children.iter().take(8).map(|c| c.name.clone()).collect::<Vec<_>>());
                println!("==> depots all non-numeric: {:?}", depots.children.iter().filter(|c| !c.name.chars().all(|ch| ch.is_ascii_digit())).map(|c| c.name.clone()).collect::<Vec<_>>());
                if let Some(branches) = depots.get("branches") {
                    println!("==> branches node found: {:?}", branches.children.iter().map(|c| c.name.clone()).collect::<Vec<_>>());
                }
            }
            if let Some(common) = info.section("common") {
                println!("==> common keys: {:?}", common.children.iter().take(30).map(|c| c.name.clone()).collect::<Vec<_>>());
            }
        }
        None => {
            println!("==> app info not available (unknown app or no access)");
        }
    }

    session.disconnect();
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    println!("==> done");
    Ok(())
}
