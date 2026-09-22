//! Store search with multiple sources and graceful degradation:
//! 1. Steam store search API (rich results with images)
//! 2. Local app list cache (downloaded from the WebAPI, substring matching)

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use anyhow::Context;
use serde::{Deserialize, Serialize};

use crate::state::config_dir;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult {
    pub app_id: u32,
    pub name: String,
    #[serde(default)]
    pub image: Option<String>,
    #[serde(default)]
    pub source: String,
}

pub struct StoreSearch {
    http: reqwest::Client,
    applist: parking_lot::RwLock<Option<Arc<Vec<(u32, String)>>>>,
}

impl StoreSearch {
    pub fn new() -> anyhow::Result<Self> {
        Ok(StoreSearch {
            http: crate::steamkit::http_client()?,
            applist: parking_lot::RwLock::new(None),
        })
    }

    pub async fn search(&self, term: &str) -> Vec<SearchResult> {
        let term = term.trim();
        if term.is_empty() {
            return Vec::new();
        }

        // 1. store search API (best results, with images)
        for (cc, l) in [("us", "english"), ("cn", "schinese")] {
            match self.search_store_api(term, cc, l).await {
                Ok(results) if !results.is_empty() => return results,
                Ok(_) => {}
                Err(_) => {}
            }
        }

        // 2. local app list fallback
        if let Ok(list) = self.ensure_applist().await {
            let needle = term.to_lowercase();
            let mut matches: Vec<(u32, String)> = Vec::new();
            // prefer prefix matches, then substring
            for phase in 0..2 {
                for (appid, name) in list.iter() {
                    if matches.len() >= 50 {
                        break;
                    }
                    let hay = name.to_lowercase();
                    let hit = if phase == 0 {
                        hay.starts_with(&needle)
                    } else {
                        hay.contains(&needle)
                    };
                    if hit && !matches.iter().any(|(id, _)| id == appid) {
                        matches.push((*appid, name.clone()));
                    }
                }
            }
            return matches
                .into_iter()
                .map(|(app_id, name)| SearchResult {
                    app_id,
                    name,
                    image: Some(header_image_url(app_id)),
                    source: "applist".into(),
                })
                .collect();
        }

        Vec::new()
    }

    async fn search_store_api(&self, term: &str, cc: &str, l: &str) -> anyhow::Result<Vec<SearchResult>> {
        let url = format!(
            "https://store.steampowered.com/api/storesearch/?term={}&cc={}&l={}",
            urlencoding(term),
            cc,
            l
        );
        let resp: serde_json::Value = self
            .http
            .get(&url)
            .timeout(Duration::from_secs(10))
            .send()
            .await?
            .json()
            .await?;

        let mut out = Vec::new();
        if let Some(items) = resp.get("items").and_then(|i| i.as_array()) {
            for item in items.iter().take(50) {
                let app_id = item.get("id").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                let name = item.get("name").and_then(|v| v.as_str()).unwrap_or("");
                if app_id == 0 || name.is_empty() {
                    continue;
                }
                // only real games/apps have type "app" (dlc/videos excluded here)
                let ty = item.get("type").and_then(|v| v.as_str()).unwrap_or("app");
                if ty != "app" {
                    continue;
                }
                let image = item
                    .get("tiny_image")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
                    .or_else(|| Some(header_image_url(app_id)));
                out.push(SearchResult {
                    app_id,
                    name: name.to_string(),
                    image,
                    source: "store".into(),
                });
            }
        }
        Ok(out)
    }

    /// Ensures the app list is loaded (from disk cache or the WebAPI).
    async fn ensure_applist(&self) -> anyhow::Result<Arc<Vec<(u32, String)>>> {
        if let Some(list) = self.applist.read().as_ref() {
            return Ok(list.clone());
        }

        let cache_path = applist_cache_path();

        // fresh disk cache (7 days)
        let fresh = std::fs::metadata(&cache_path)
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| SystemTime::now().duration_since(t).ok())
            .map(|d| d < Duration::from_secs(7 * 24 * 3600))
            .unwrap_or(false);

        if fresh {
            if let Ok(list) = load_applist_cache(&cache_path) {
                let list = Arc::new(list);
                *self.applist.write() = Some(list.clone());
                return Ok(list);
            }
        }

        // fetch
        let resp: serde_json::Value = self
            .http
            .get("https://api.steampowered.com/ISteamApps/GetAppList/v2/")
            .timeout(Duration::from_secs(120))
            .send()
            .await
            .context("获取应用列表失败")?
            .json()
            .await
            .context("应用列表解析失败")?;

        let mut list = Vec::new();
        if let Some(apps) = resp
            .get("response")
            .and_then(|r| r.get("apps"))
            .and_then(|a| a.as_array())
        {
            for app in apps {
                let appid = app.get("appid").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                let name = app.get("name").and_then(|v| v.as_str()).unwrap_or("");
                if appid > 0 && !name.is_empty() {
                    list.push((appid, name.to_string()));
                }
            }
        }
        if list.is_empty() {
            // fall back to stale cache if we have one
            if let Ok(list) = load_applist_cache(&cache_path) {
                let list = Arc::new(list);
                *self.applist.write() = Some(list.clone());
                return Ok(list);
            }
            anyhow::bail!("应用列表为空");
        }

        let _ = save_applist_cache(&cache_path, &list);
        let list = Arc::new(list);
        *self.applist.write() = Some(list.clone());
        Ok(list)
    }

    /// Looks up a name for an app id (from applist cache, best effort).
    pub async fn app_name_hint(&self, app_id: u32) -> Option<String> {
        let list = self.ensure_applist().await.ok()?;
        list.iter()
            .find(|(id, _)| *id == app_id)
            .map(|(_, n)| n.clone())
    }

    /// Popular games right now (SteamSpy top 100, best effort).
    pub async fn popular(&self) -> Vec<SearchResult> {
        let resp: serde_json::Value = match self
            .http
            .get("https://steamspy.com/api.php?request=top100in2weeks")
            .timeout(Duration::from_secs(15))
            .send()
            .await
            .and_then(|r| Ok(r))
        {
            Ok(r) => r.json().await.unwrap_or_default(),
            Err(_) => return Vec::new(),
        };
        let mut out = Vec::new();
        if let Some(map) = resp.as_object() {
            let mut entries: Vec<(u32, String)> = map
                .values()
                .filter_map(|v| {
                    let appid = v.get("appid")?.as_u64()? as u32;
                    let name = v.get("name")?.as_str()?.to_string();
                    Some((appid, name))
                })
                .collect();
            entries.sort_by_key(|(id, _)| *id);
            for (app_id, name) in entries.into_iter().take(60) {
                out.push(SearchResult {
                    app_id,
                    name,
                    image: Some(header_image_url(app_id)),
                    source: "popular".into(),
                });
            }
        }
        out
    }
}

pub fn header_image_url(app_id: u32) -> String {
    format!(
        "https://shared.cloudflare.steamstatic.com/store_item_assets/steam/apps/{}/header.jpg",
        app_id
    )
}

pub fn capsule_image_url(app_id: u32) -> String {
    format!(
        "https://shared.cloudflare.steamstatic.com/store_item_assets/steam/apps/{}/capsule_184x69.jpg",
        app_id
    )
}

fn urlencoding(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

fn applist_cache_path() -> PathBuf {
    config_dir().join("applist.json")
}

fn load_applist_cache(path: &PathBuf) -> anyhow::Result<Vec<(u32, String)>> {
    let data = std::fs::read_to_string(path)?;
    let raw: Vec<(u32, String)> = serde_json::from_str(&data)?;
    Ok(raw)
}

fn save_applist_cache(path: &PathBuf, list: &[(u32, String)]) -> anyhow::Result<()> {
    let json = serde_json::to_string(list)?;
    std::fs::write(path, json)?;
    Ok(())
}
