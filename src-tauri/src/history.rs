//! Depot manifest history: SteamDB scraping (best effort) merged with the
//! branch manifests from PICS (always available while logged on).

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::steamkit::kv::KeyValue;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestVersion {
    pub manifest_id: u64,
    /// ISO date string when known.
    pub date: Option<String>,
    /// "steamdb" | "pics"
    pub source: String,
    /// Branch this manifest currently belongs to, if any.
    pub branch: Option<String>,
    pub build_id: Option<u64>,
    pub description: Option<String>,
}

/// Manifests currently attached to each branch (from PICS appinfo).
pub fn branch_manifests(app_kv: &KeyValue, depot_id: u32) -> Vec<ManifestVersion> {
    let mut out = Vec::new();
    let Some(depots) = app_kv.get("depots") else { return out };
    let Some(depot) = depots.get(&depot_id.to_string()) else { return out };
    let Some(manifests) = depot.get("manifests") else { return out };

    let branches = app_kv
        .get("depots")
        .and_then(|d| d.get("branches"));

    for branch_node in &manifests.children {
        let branch = branch_node.name.clone();
        let gid = branch_node
            .get("gid")
            .and_then(|g| g.as_u64())
            .unwrap_or(0);
        if gid == 0 {
            continue;
        }
        let (build_id, description) = branches
            .and_then(|b| b.get(&branch))
            .map(|b| {
                (
                    b.get("buildid").and_then(|v| v.as_u64()),
                    b.get("description").and_then(|v| v.value_str()).map(|s| s.to_string()),
                )
            })
            .unwrap_or((None, None));
        out.push(ManifestVersion {
            manifest_id: gid,
            date: None,
            source: "pics".into(),
            branch: Some(branch),
            build_id,
            description,
        });
    }
    out
}

/// Branch list of an app (name, build id, description).
pub fn branch_list(app_kv: &KeyValue) -> Vec<(String, u64, Option<String>)> {
    let mut out = Vec::new();
    if let Some(branches) = app_kv.get("depots").and_then(|d| d.get("branches")) {
        for b in &branches.children {
            out.push((
                b.name.clone(),
                b.get("buildid").and_then(|v| v.as_u64()).unwrap_or(0),
                b.get("description").and_then(|v| v.value_str()).map(|s| s.to_string()),
            ));
        }
    }
    out
}

/// Scrapes SteamDB for the manifest history of a depot. Returns an empty vec
/// when unreachable (SteamDB is often behind Cloudflare).
pub async fn steamdb_history(depot_id: u32) -> Vec<ManifestVersion> {
    let http = match crate::steamkit::http_client() {
        Ok(h) => h,
        Err(_) => return Vec::new(),
    };
    let url = format!("https://steamdb.info/depot/{}/history/", depot_id);
    let body = match http
        .get(&url)
        .timeout(Duration::from_secs(15))
        .header(reqwest::header::ACCEPT, "text/html")
        .send()
        .await
    {
        Ok(r) if r.status().is_success() => match r.text().await {
            Ok(t) => t,
            Err(_) => return Vec::new(),
        },
        _ => return Vec::new(),
    };
    parse_steamdb_history(&body)
}

/// Parses manifest ids + dates out of the SteamDB depot history HTML.
fn parse_steamdb_history(html: &str) -> Vec<ManifestVersion> {
    let mut out: Vec<ManifestVersion> = Vec::new();
    // Rows look like: <td>8967932709921207793</td> ... <td ... data-time="1739999999" ...>
    let row_re = regex::Regex::new(r"(?s)<tr[^>]*>(.*?)</tr>").unwrap();
    let gid_re = regex::Regex::new(r"<td[^>]*>\s*(\d{15,20})\s*</td>").unwrap();
    let time_re = regex::Regex::new(r#"data-time="(\d+)""#).unwrap();
    let date_re = regex::Regex::new(r"(\d{4}-\d{2}-\d{2})").unwrap();

    for row in row_re.captures_iter(html) {
        let row = row.get(1).unwrap().as_str();
        let Some(gid_cap) = gid_re.captures(row) else { continue };
        let Ok(gid) = gid_cap.get(1).unwrap().as_str().parse::<u64>() else { continue };
        let date = time_re
            .captures(row)
            .and_then(|c| c.get(1).unwrap().as_str().parse::<i64>().ok())
            .map(|ts| {
                time::OffsetDateTime::from_unix_timestamp(ts)
                    .map(|t| t.date().to_string())
                    .unwrap_or_default()
            })
            .or_else(|| date_re.captures(row).map(|c| c.get(1).unwrap().as_str().to_string()));
        if out.iter().any(|m| m.manifest_id == gid) {
            continue;
        }
        out.push(ManifestVersion {
            manifest_id: gid,
            date,
            source: "steamdb".into(),
            branch: None,
            build_id: None,
            description: None,
        });
    }
    out.sort_by(|a, b| b.manifest_id.cmp(&a.manifest_id));
    out
}

/// Merges SteamDB history with current branch manifests (dedup by gid).
pub fn merged_history(mut steamdb: Vec<ManifestVersion>, pics: Vec<ManifestVersion>) -> Vec<ManifestVersion> {
    for p in pics {
        if let Some(existing) = steamdb.iter_mut().find(|m| m.manifest_id == p.manifest_id) {
            existing.branch = p.branch.clone().or(existing.branch.take());
            existing.build_id = p.build_id.or(existing.build_id);
            existing.description = p.description.clone().or(existing.description.take());
        } else {
            steamdb.push(p);
        }
    }
    steamdb
}
