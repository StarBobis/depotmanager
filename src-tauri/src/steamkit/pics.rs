//! PICS (Product Info) queries: access tokens + app/package info with
//! caching, and text-VDF / binary-KV decoding.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context};
use dashmap::DashMap;

use super::connection::CmConnection;
use super::kv::{self, KeyValue};
use super::msgs::emsg;
use super::proto_gen::*;

/// Parsed product info for one app.
pub struct AppInfo {
    pub app_id: u32,
    pub change_number: u32,
    pub missing_token: bool,
    /// Root KeyValue whose children are the top-level sections
    /// ("common", "depots", "config", "extended", ...).
    pub kv: KeyValue,
    pub only_public: bool,
}

pub struct PackageInfo {
    pub package_id: u32,
    pub change_number: u32,
    pub missing_token: bool,
    pub kv: KeyValue,
}

pub struct ProductInfoService {
    conn: Arc<CmConnection>,
    pub app_tokens: DashMap<u32, u64>,
    pub package_tokens: DashMap<u32, u64>,
    app_cache: DashMap<u32, Option<Arc<AppInfo>>>,
    package_cache: DashMap<u32, Option<Arc<PackageInfo>>>,
}

impl ProductInfoService {
    pub fn new(conn: Arc<CmConnection>) -> Self {
        ProductInfoService {
            conn,
            app_tokens: DashMap::new(),
            package_tokens: DashMap::new(),
            app_cache: DashMap::new(),
            package_cache: DashMap::new(),
        }
    }

    /// Requests PICS access tokens for the given apps/packages and caches them.
    pub async fn request_access_tokens(
        &self,
        app_ids: &[u32],
        package_ids: &[u32],
    ) -> anyhow::Result<()> {
        if app_ids.is_empty() && package_ids.is_empty() {
            return Ok(());
        }
        let req = CMsgClientPicsAccessTokenRequest {
            appids: app_ids.to_vec(),
            packageids: package_ids.to_vec(),
        };
        let pkt = self
            .conn
            .call(emsg::CLIENT_PICS_ACCESS_TOKEN_REQUEST, &req, Duration::from_secs(20))
            .await
            .context("PICS access token request failed")?;
        let resp: CMsgClientPicsAccessTokenResponse = pkt.decode_body()?;
        for t in resp.app_access_tokens {
            if let (Some(app), Some(tok)) = (t.appid, t.access_token) {
                self.app_tokens.insert(app, tok);
            }
        }
        for t in resp.package_access_tokens {
            if let (Some(pkg), Some(tok)) = (t.packageid, t.access_token) {
                self.package_tokens.insert(pkg, tok);
            }
        }
        Ok(())
    }

    /// Fetches app info for a single app (with caching). Returns Ok(None) if
    /// the app is unknown / not visible to this account.
    pub async fn app_info(&self, app_id: u32) -> anyhow::Result<Option<Arc<AppInfo>>> {
        self.app_info_force(app_id, false).await
    }

    /// Requests a FreeOnDemand license for a free-to-play app.
    /// Returns (granted_appids, granted_packageids).
    ///
    /// Note: the CM silently ignores this message from anonymous sessions,
    /// so only call it for logged-in accounts.
    pub async fn request_free_license(&self, app_id: u32) -> anyhow::Result<(Vec<u32>, Vec<u32>)> {
        let req = CMsgClientRequestFreeLicense { appids: vec![app_id] };
        let pkt = self
            .conn
            .call(emsg::CLIENT_REQUEST_FREE_LICENSE, &req, Duration::from_secs(15))
            .await
            .context("free license request failed")?;
        let resp: CMsgClientRequestFreeLicenseResponse = pkt.decode_body()?;
        Ok((resp.granted_appids, resp.granted_packageids))
    }

    /// Fetches app info, upgrading public-only info when possible: when the
    /// server reports a missing token (the account holds no license, so the
    /// depots section is withheld), logged-in sessions try to claim a
    /// FreeOnDemand license and refetch with the new token. Set
    /// `allow_free_license` to false for anonymous sessions — the CM never
    /// answers their free-license requests.
    pub async fn app_info_licensed(
        &self,
        app_id: u32,
        allow_free_license: bool,
    ) -> anyhow::Result<Option<Arc<AppInfo>>> {
        let info = self.app_info(app_id).await?;
        let Some(info) = info else { return Ok(None) };
        if !info.missing_token || !allow_free_license {
            return Ok(Some(info));
        }
        if let Ok((granted_apps, granted_pkgs)) = self.request_free_license(app_id).await {
            if granted_apps.contains(&app_id) || !granted_pkgs.is_empty() {
                // license claimed: drop stale token state and refetch appinfo
                self.app_tokens.remove(&app_id);
                let _ = self.request_access_tokens(&[app_id], &[]).await;
                if let Some(fresh) = self.app_info_force(app_id, true).await? {
                    return Ok(Some(fresh));
                }
            }
        }
        Ok(Some(info))
    }

    pub async fn app_info_force(
        &self,
        app_id: u32,
        force: bool,
    ) -> anyhow::Result<Option<Arc<AppInfo>>> {
        if !force {
            if let Some(entry) = self.app_cache.get(&app_id) {
                return Ok(entry.clone());
            }
        }

        // Ensure we have an access token (may be empty for public apps).
        if !self.app_tokens.contains_key(&app_id) {
            // Best-effort; denied tokens simply mean "use public info".
            let _ = self.request_access_tokens(&[app_id], &[]).await;
        }
        let token = self.app_tokens.get(&app_id).map(|v| *v).unwrap_or(0);

        let req = CMsgClientPicsProductInfoRequest {
            apps: vec![c_msg_client_pics_product_info_request::AppInfo {
                appid: Some(app_id),
                access_token: if token == 0 { None } else { Some(token) },
                only_public_obsolete: None,
            }],
            packages: vec![],
            meta_data_only: Some(false),
            num_prev_failed: None,
            sequence_number: None,
            single_response: None,
        };

        let pkts = self
            .conn
            .call_multi(
                emsg::CLIENT_PICS_PRODUCT_INFO_REQUEST,
                &req,
                Duration::from_secs(30),
                |batch| {
                    batch.iter().any(|p| {
                        p.decode_body::<CMsgClientPicsProductInfoResponse>()
                            .map(|r| !r.response_pending.unwrap_or(false))
                            .unwrap_or(true)
                    })
                },
            )
            .await
            .context("PICS product info request failed")?;

        let mut result: Option<Arc<AppInfo>> = None;
        let mut unknown = false;
        for pkt in &pkts {
            let resp: CMsgClientPicsProductInfoResponse = pkt.decode_body()?;
            if resp.unknown_appids.contains(&app_id) {
                unknown = true;
            }
            for app in resp.apps {
                if app.appid != Some(app_id) {
                    continue;
                }
                let kv_root = match &app.buffer {
                    Some(buf) if buf.len() > 1 => {
                        // text VDF with trailing NUL stripped
                        let parsed = kv::parse_text_vdf(&buf[..buf.len() - 1])
                            .context("解析 appinfo VDF 失败")?;
                        // Unwrap single top-level node named after the app.
                        if parsed.children.len() == 1 {
                            parsed.children.into_iter().next().unwrap()
                        } else {
                            parsed
                        }
                    }
                    _ => KeyValue::new(""),
                };
                result = Some(Arc::new(AppInfo {
                    app_id,
                    change_number: app.change_number.unwrap_or(0),
                    missing_token: app.missing_token.unwrap_or(false),
                    kv: kv_root,
                    only_public: app.only_public.unwrap_or(false),
                }));
            }
        }

        if unknown && result.is_none() {
            self.app_cache.insert(app_id, None);
            return Ok(None);
        }
        self.app_cache.insert(app_id, result.clone());
        Ok(result)
    }

    /// Fetches package info for the given packages (with caching).
    pub async fn package_info(
        &self,
        package_ids: &[u32],
    ) -> anyhow::Result<HashMap<u32, Option<Arc<PackageInfo>>>> {
        let mut missing: Vec<u32> = package_ids
            .iter()
            .copied()
            .filter(|id| !self.package_cache.contains_key(id))
            .collect();
        missing.sort_unstable();
        missing.dedup();

        if !missing.is_empty() {
            let toks_missing: Vec<u32> = missing
                .iter()
                .copied()
                .filter(|id| !self.package_tokens.contains_key(id))
                .collect();
            if !toks_missing.is_empty() {
                let _ = self.request_access_tokens(&[], &toks_missing).await;
            }

            let requests = missing
                .iter()
                .map(|id| c_msg_client_pics_product_info_request::PackageInfo {
                    packageid: Some(*id),
                    access_token: self.package_tokens.get(id).map(|v| *v).filter(|t| *t != 0),
                })
                .collect();

            let req = CMsgClientPicsProductInfoRequest {
                apps: vec![],
                packages: requests,
                meta_data_only: Some(false),
                num_prev_failed: None,
                sequence_number: None,
                single_response: None,
            };

            let pkts = self
                .conn
                .call_multi(
                    emsg::CLIENT_PICS_PRODUCT_INFO_REQUEST,
                    &req,
                    Duration::from_secs(30),
                    |batch| {
                        batch.iter().any(|p| {
                            p.decode_body::<CMsgClientPicsProductInfoResponse>()
                                .map(|r| !r.response_pending.unwrap_or(false))
                                .unwrap_or(true)
                        })
                    },
                )
                .await
                .context("PICS package info request failed")?;

            for pkt in &pkts {
                let resp: CMsgClientPicsProductInfoResponse = pkt.decode_body()?;
                for id in &resp.unknown_packageids {
                    self.package_cache.insert(*id, None);
                }
                for pkg in resp.packages {
                    let id = pkg.packageid.unwrap_or(0);
                    let kv_root = match &pkg.buffer {
                        Some(buf) if buf.len() > 4 => {
                            // u32 version + binary KV; the buffer has a single
                            // root child named after the package id.
                            let parsed = kv::parse_binary_kv(&buf[4..]).unwrap_or_default();
                            if parsed.children.len() == 1 {
                                parsed.children.into_iter().next().unwrap()
                            } else {
                                parsed
                            }
                        }
                        _ => KeyValue::new(""),
                    };
                    self.package_cache.insert(
                        id,
                        Some(Arc::new(PackageInfo {
                            package_id: id,
                            change_number: pkg.change_number.unwrap_or(0),
                            missing_token: pkg.missing_token.unwrap_or(false),
                            kv: kv_root,
                        })),
                    );
                }
            }
            // Any packages not present in the response are unknown.
            for id in &missing {
                self.package_cache.entry(*id).or_insert(None);
            }
        }

        let mut out = HashMap::new();
        for id in package_ids {
            out.insert(*id, self.package_cache.get(id).map(|v| v.clone()).unwrap_or(None));
        }
        Ok(out)
    }
}

// ---- appinfo section helpers ----------------------------------------------

impl AppInfo {
    pub fn section(&self, name: &str) -> Option<&KeyValue> {
        self.kv.get(name)
    }

    pub fn name(&self) -> Option<String> {
        self.section("common")?.get("name")?.value_str().map(|s| s.to_string())
    }

    pub fn free_to_download(&self) -> bool {
        self.section("common")
            .and_then(|c| c.get("FreeToDownload"))
            .map(|v| v.as_bool())
            .unwrap_or(false)
    }

    pub fn os_list(&self) -> Option<String> {
        self.section("common")?.get("oslist")?.value_str().map(String::from)
    }

    /// Branch list: name -> (buildid, description).
    pub fn branches(&self) -> Vec<(String, u64, Option<String>)> {
        let mut out = Vec::new();
        if let Some(depots) = self.section("depots") {
            if let Some(branches) = depots.get("branches") {
                for b in &branches.children {
                    let buildid = b.get("buildid").and_then(|v| v.as_u64()).unwrap_or(0);
                    let desc = b
                        .get("description")
                        .and_then(|v| v.value_str())
                        .map(String::from);
                    out.push((b.name.clone(), buildid, desc));
                }
            }
        }
        out
    }
}

/// Checks whether a package grants access to the given app/depot.
pub fn package_grants_access(pkg: &PackageInfo, app_or_depot_id: u32) -> bool {
    let needle = app_or_depot_id.to_string();
    for section in ["appids", "depotids"] {
        if let Some(list) = pkg.kv.get(section) {
            for child in &list.children {
                if child.value_str() == Some(needle.as_str()) || child.name == needle {
                    return true;
                }
            }
        }
    }
    false
}

/// Errors specific to PICS lookups.
pub fn missing_app_err(app_id: u32) -> anyhow::Error {
    anyhow!("无法获取 App {} 的信息（不存在或当前账号无权访问）", app_id)
}
