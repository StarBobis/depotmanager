//! CDN smoke test: depot keys, manifest request codes, manifest download,
//! chunk download/decrypt/verify — the full Steampipe read path.
//!
//! Run: cargo run --bin smoke2 -- [appid] [depotid]

use depotmanager_lib::steamkit::cdn::{self, CdnClient};
use depotmanager_lib::steamkit::connection::{CmConnection, ConnectionEvent};
use depotmanager_lib::steamkit::pics::ProductInfoService;
use depotmanager_lib::steamkit::{machineid, session::SteamSession};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let app_id: u32 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(730);
    let forced_depot: Option<u32> = std::env::args().nth(2).and_then(|s| s.parse().ok());

    let (events, mut rx) = tokio::sync::broadcast::channel::<ConnectionEvent>(64);
    tokio::spawn(async move {
        while let Ok(ev) = rx.recv().await {
            match &ev {
                ConnectionEvent::Licenses { count } => println!("[event] Licenses: {}", count),
                ConnectionEvent::LoggedOn { .. } | ConnectionEvent::Disconnected { .. } => println!("[event] {:?}", ev),
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
    let session = SteamSession::new(conn.clone(), machineid::build_machine_id(b"smoke-test-2"), 0);
    let steamid = session.logon_anonymous().await?;
    println!("==> logged on, steamid = {steamid}");

    let pics = ProductInfoService::new(conn.clone());
    let info = pics
        .app_info(app_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("no app info"))?;
    println!("==> app: {:?} change {}", info.name(), info.change_number);

    // List depots with public manifests.
    let mut depot_ids: Vec<u32> = Vec::new();
    if let Some(depots) = info.section("depots") {
        for child in &depots.children {
            if !child.name.chars().all(|c| c.is_ascii_digit()) {
                continue;
            }
            let id: u32 = child.name.parse().unwrap_or(0);
            let gid = child
                .path("manifests/public/gid")
                .and_then(|g| g.as_u64());
            let oslist = child.path("config/oslist").and_then(|v| v.value_str());
            let size = child.path("manifests/public/size").and_then(|v| v.as_u64());
            if let Some(gid) = gid {
                depot_ids.push(id);
                println!(
                    "    depot {:<10} gid {:<22} size {:<14} os {:?}",
                    id, gid, size.unwrap_or(0), oslist
                );
            }
        }
    }

    let depot_id = forced_depot.or_else(|| depot_ids.first().copied()).expect("no depot");
    println!("==> using depot {}", depot_id);

    let cdn = CdnClient::new(conn.clone())?;
    let cell_id = conn.state().cell_id;
    let n = cdn.update_servers(cell_id, app_id).await?;
    println!("==> {} CDN servers", n);
    cdn.debug_print_servers();

    // find a depot whose key is granted to this (anonymous) session
    let mut depot_key = Vec::new();
    let mut depot_id = depot_id;
    let candidates: Vec<u32> = if forced_depot.is_some() {
        vec![depot_id]
    } else {
        depot_ids.clone()
    };
    for cand in &candidates {
        match cdn::get_depot_key(&conn, app_id, *cand).await {
            Ok(key) => {
                println!("==> depot {} key OK: {}", cand, hex::encode(&key));
                depot_key = key;
                depot_id = *cand;
                break;
            }
            Err(e) => println!("    depot {} key denied: {}", cand, e),
        }
    }
    if depot_key.is_empty() {
        anyhow::bail!("没有任何 depot 密钥可用（匿名无权访问该游戏?）");
    }

    // manifest gid for the public branch
    let depots = info.section("depots").unwrap();
    let depot_kv = depots.get(&depot_id.to_string()).expect("depot kv");
    let gid = depot_kv.path("manifests/public/gid").and_then(|g| g.as_u64()).expect("gid");

    println!("==> requesting manifest request code for gid {}...", gid);
    let code = cdn.get_manifest_request_code(depot_id, app_id, gid, "public").await?;
    println!("==> request code: {}", code);

    // debug: CDN auth token
    for host in ["xz.pphimalayanrt.com", "st.dl.eccdnx.com", "dl.steam.clngaa.com"] {
        match cdn.cdn_auth_token(app_id, depot_id, host).await {
            Ok(t) => println!("==> cdn token for {}: {} (len {})", host, &t[..t.len().min(60)], t.len()),
            Err(e) => println!("==> cdn token for {} FAILED: {}", host, e),
        }
    }

    println!("==> downloading manifest...");
    let t1 = std::time::Instant::now();
    let manifest = cdn
        .download_manifest(app_id, depot_id, gid, code, &depot_key)
        .await?;
    println!(
        "==> manifest ok: {} files, {} bytes total ({}ms)",
        manifest.files.len(),
        manifest.total_uncompressed,
        t1.elapsed().as_millis()
    );

    // find the smallest file with at least one chunk
    let file = manifest
        .files
        .iter()
        .filter(|f| !f.chunks.is_empty() && f.size > 0)
        .min_by_key(|f| f.size)
        .expect("no file");
    println!(
        "==> smallest file: {} ({} bytes, {} chunks)",
        file.name,
        file.size,
        file.chunks.len()
    );

    let chunk = &file.chunks[0];
    println!(
        "==> downloading chunk {} ({} -> {} bytes)...",
        chunk.id_hex(),
        chunk.compressed_len,
        chunk.uncompressed_len
    );
    let t2 = std::time::Instant::now();
    let data = cdn.download_chunk(app_id, depot_id, chunk, &depot_key).await?;
    println!(
        "==> chunk ok: {} bytes in {}ms ({:.2} MB/s)",
        data.len(),
        t2.elapsed().as_millis(),
        data.len() as f64 / 1e6 / t2.elapsed().as_secs_f64()
    );

    session.disconnect();
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    println!("==> done");
    Ok(())
}
