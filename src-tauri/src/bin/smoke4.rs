//! Debug: dump anonymous package 17906 contents and access checks.
//! Run: cargo run --bin smoke4 -- [appid]

use depotmanager_lib::steamkit::connection::{CmConnection, ConnectionEvent};
use depotmanager_lib::steamkit::pics::{package_grants_access, ProductInfoService};
use depotmanager_lib::steamkit::{machineid, session::SteamSession};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let app_id: u32 = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(730);

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
    let session = SteamSession::new(conn.clone(), machineid::build_machine_id(b"smoke-test-4"), 0);
    session.logon_anonymous().await?;
    println!("==> logged on");

    let pics = ProductInfoService::new(conn.clone());
    println!("==> licenses: {:?}", conn.state().licenses);

    let map = pics.package_info(&[17906]).await?;
    match map.get(&17906) {
        Some(Some(pkg)) => {
            let kv = &pkg.kv;
            let appids: Vec<u32> = kv
                .path("appids")
                .map(|n| n.children.iter().filter_map(|c| c.as_u32()).collect())
                .unwrap_or_default();
            let depotids: Vec<u32> = kv
                .path("depotids")
                .map(|n| n.children.iter().filter_map(|c| c.as_u32()).collect())
                .unwrap_or_default();
            println!("==> package 17906: {} appids, {} depotids", appids.len(), depotids.len());
            println!("    contains app {}: {}", app_id, appids.contains(&app_id));
            println!("    grants app: {}", package_grants_access(pkg, app_id));
        }
        Some(None) => println!("==> package 17906 returned empty"),
        None => println!("==> package 17906 not in response"),
    }

    // Try requesting a free license for the app, then re-check PICS token.
    use depotmanager_lib::steamkit::msgs::emsg;
    use depotmanager_lib::steamkit::proto_gen::*;
    let req = CMsgClientRequestFreeLicense { appids: vec![app_id] };
    match conn.call(emsg::CLIENT_REQUEST_FREE_LICENSE, &req, std::time::Duration::from_secs(20)).await {
        Ok(pkt) => {
            let resp: CMsgClientRequestFreeLicenseResponse = pkt.decode_body()?;
            println!(
                "==> RequestFreeLicense: eresult={:?} granted_apps={:?} granted_packages={:?}",
                resp.eresult, resp.granted_appids, resp.granted_packageids
            );
        }
        Err(e) => println!("==> RequestFreeLicense failed: {}", e),
    }

    // re-request access token
    pics.request_access_tokens(&[app_id], &[]).await?;
    println!("==> token after free license: {:?}", pics.app_tokens.get(&app_id).map(|v| *v));

    // force re-fetch app info
    match pics.app_info_force(app_id, true).await? {
        Some(info) => {
            println!("==> after: missing_token={} only_public={}", info.missing_token, info.only_public);
            if let Some(depots) = info.section("depots") {
                let count = depots.children.iter().filter(|c| c.name.chars().all(|ch| ch.is_ascii_digit())).count();
                println!("==> after: depots = {}", count);
                println!("==> branches = {:?}", info.branches().iter().map(|b| b.0.clone()).collect::<Vec<_>>());
            } else {
                println!("==> after: still no depots section");
            }
        }
        None => println!("==> after: no app info"),
    }

    session.disconnect();
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    Ok(())
}
