//! Tauri command surface: login, search, app details, downloads, settings.

use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tauri::State;

use crate::engine::{DownloadRequest, Engine, TaskSnapshot};
use crate::history::{self, ManifestVersion};
use crate::state::Settings;
use crate::steam_runtime::{AppContext, AuthChallenge, LoginState};
use crate::store::{SearchResult, StoreSearch};
pub type CmdResult<T> = Result<T, String>;

fn err(e: anyhow::Error) -> String {
    format!("{:#}", e)
}

// ============================================================
// login
// ============================================================

#[tauri::command]
pub fn get_login_state(ctx: State<'_, Arc<AppContext>>) -> LoginState {
    ctx.login_state.read().clone()
}

#[tauri::command]
pub async fn login_anonymous(ctx: State<'_, Arc<AppContext>>) -> CmdResult<LoginState> {
    ctx.try_login_anonymous().await.map_err(err)
}

#[tauri::command]
pub async fn login_saved(ctx: State<'_, Arc<AppContext>>, account_name: String) -> CmdResult<LoginState> {
    let token = {
        let s = ctx.settings.read();
        s.find_account(&account_name)
            .map(|a| a.refresh_token.clone())
    }
    .ok_or_else(|| "未找到保存的账号".to_string())?;
    ctx.try_login_account(&account_name, &token).await.map_err(err)
}

#[tauri::command]
pub async fn login_password(
    ctx: State<'_, Arc<AppContext>>,
    account_name: String,
    password: String,
    remember: bool,
) -> CmdResult<AuthChallenge> {
    ctx.disconnect().await;
    ctx.connect().await.map_err(err)?;
    ctx.set_login_state(LoginState::Authenticating {
        detail: format!("正在验证账号 {}...", account_name),
    });
    ctx.begin_credentials_auth(&account_name, &password, remember)
        .await
        .map_err(err)
}

#[tauri::command]
pub async fn login_qr(ctx: State<'_, Arc<AppContext>>) -> CmdResult<String> {
    ctx.disconnect().await;
    ctx.connect().await.map_err(err)?;
    ctx.set_login_state(LoginState::Authenticating {
        detail: "请使用 Steam 手机应用扫描二维码".into(),
    });
    ctx.begin_qr_auth().await.map_err(err)
}

#[tauri::command]
pub async fn login_submit_code(ctx: State<'_, Arc<AppContext>>, code: String, code_type: i32) -> CmdResult<()> {
    ctx.submit_guard_code(&code, code_type).await.map_err(err)
}

/// Polls the pending auth session once; returns Some(state) when logged in.
#[tauri::command]
pub async fn auth_poll(ctx: State<'_, Arc<AppContext>>) -> CmdResult<Option<LoginState>> {
    let has_pending = ctx.pending_auth.lock().await.is_some();
    if !has_pending {
        return Ok(Some(ctx.login_state.read().clone()));
    }
    match ctx.poll_auth_once().await {
        Ok(Some((account_name, refresh, _guard))) => {
            let state = ctx
                .try_login_account(&account_name, &refresh)
                .await
                .map_err(err)?;
            Ok(Some(state))
        }
        Ok(None) => Ok(None),
        Err(e) => Err(err(e)),
    }
}

#[tauri::command]
pub async fn auth_cancel(ctx: State<'_, Arc<AppContext>>) -> CmdResult<()> {
    *ctx.pending_auth.lock().await = None;
    Ok(())
}

#[tauri::command]
pub async fn logout(ctx: State<'_, Arc<AppContext>>) -> CmdResult<()> {
    *ctx.pending_auth.lock().await = None;
    ctx.disconnect().await;
    ctx.set_login_state(LoginState::Offline {
        message: "已退出登录".into(),
    });
    Ok(())
}

// ============================================================
// store search
// ============================================================

#[tauri::command]
pub async fn search_games(
    ctx: State<'_, Arc<AppContext>>,
    store: State<'_, Arc<StoreSearch>>,
    query: String,
) -> CmdResult<Vec<SearchResult>> {
    let _ = ctx;
    Ok(store.search(&query).await)
}

#[tauri::command]
pub async fn popular_games(store: State<'_, Arc<StoreSearch>>) -> CmdResult<Vec<SearchResult>> {
    Ok(store.popular().await)
}

// ============================================================
// app details / history
// ============================================================

#[derive(Debug, Clone, Serialize)]
pub struct BranchInfo {
    pub name: String,
    pub build_id: u64,
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DepotManifestRef {
    pub branch: String,
    pub manifest_id: u64,
    pub size: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct DepotInfo {
    pub depot_id: u32,
    pub owner_app: u32,
    pub os_list: String,
    pub os_arch: String,
    pub language: String,
    pub shared_install: bool,
    pub low_violence: bool,
    pub manifests: Vec<DepotManifestRef>,
    /// null = unknown (not checked), true/false = access for current session
    pub accessible: Option<bool>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AppDetail {
    pub app_id: u32,
    pub name: String,
    pub free_to_download: bool,
    pub os_list: Vec<String>,
    pub branches: Vec<BranchInfo>,
    pub depots: Vec<DepotInfo>,
    pub header_image: String,
    /// true when the server withheld the depots section because the current
    /// session holds no license for this app.
    pub depots_restricted: bool,
}

#[tauri::command]
pub async fn get_app_detail(ctx: State<'_, Arc<AppContext>>, app_id: u32) -> CmdResult<AppDetail> {
    let rt = ctx.runtime.lock().await.clone().ok_or_else(|| "未连接 Steam，请先登录".to_string())?;

    let is_anonymous = rt.conn.state().steamid >> 52 & 0xF == 10; // account type AnonUser
    let info = rt
        .pics
        .app_info_licensed(app_id, !is_anonymous)
        .await
        .map_err(err)?
        .ok_or_else(|| format!("无法获取 App {} 的信息（不存在或无权访问）", app_id))?;

    let kv = &info.kv;
    let os_list = kv
        .path("common/oslist")
        .and_then(|v| v.value_str())
        .unwrap_or_default()
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>();

    let branches = history::branch_list(kv)
        .into_iter()
        .map(|(name, build_id, description)| BranchInfo {
            name,
            build_id,
            description,
        })
        .collect();

    let mut depots = Vec::new();
    if let Some(depots_kv) = kv.get("depots") {
        for child in &depots_kv.children {
            if !child.name.chars().all(|c| c.is_ascii_digit()) {
                continue;
            }
            let depot_id: u32 = match child.name.parse() {
                Ok(v) => v,
                Err(_) => continue,
            };
            let config = child.get("config");
            let manifests = child
                .get("manifests")
                .map(|m| {
                    m.children
                        .iter()
                        .filter_map(|b| {
                            let gid = b.get("gid").and_then(|g| g.as_u64())?;
                            if gid == 0 {
                                return None;
                            }
                            Some(DepotManifestRef {
                                branch: b.name.clone(),
                                manifest_id: gid,
                                size: b.get("size").and_then(|v| v.as_u64()).unwrap_or(0),
                            })
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            depots.push(DepotInfo {
                depot_id,
                owner_app: child
                    .get("depotfromapp")
                    .and_then(|v| v.as_u32())
                    .unwrap_or(app_id),
                os_list: config
                    .and_then(|c| c.get("oslist"))
                    .and_then(|v| v.value_str())
                    .map(|s| s.to_string())
                    .unwrap_or_default(),
                os_arch: config
                    .and_then(|c| c.get("osarch"))
                    .and_then(|v| v.value_str())
                    .map(|s| s.to_string())
                    .unwrap_or_default(),
                language: config
                    .and_then(|c| c.get("language"))
                    .and_then(|v| v.value_str())
                    .map(|s| s.to_string())
                    .unwrap_or_default(),
                shared_install: child.get("sharedinstall").map(|v| v.as_bool()).unwrap_or(false)
                    || config.and_then(|c| c.get("sharedinstall")).map(|v| v.as_bool()).unwrap_or(false),
                low_violence: config
                    .and_then(|c| c.get("lowviolence"))
                    .map(|v| v.as_bool())
                    .unwrap_or(false),
                manifests,
                accessible: None,
            });
        }
    }
    depots.sort_by_key(|d| d.depot_id);

    let depots_restricted = depots.is_empty() && (info.missing_token || info.only_public);

    Ok(AppDetail {
        app_id,
        name: info.name().unwrap_or_else(|| format!("App {}", app_id)),
        free_to_download: info.free_to_download(),
        os_list,
        branches,
        depots,
        header_image: crate::store::header_image_url(app_id),
        depots_restricted,
    })
}

/// Checks depot accessibility for the current session (updates in place).
#[tauri::command]
pub async fn check_depot_access(ctx: State<'_, Arc<AppContext>>, app_id: u32, depot_ids: Vec<u32>) -> CmdResult<Vec<(u32, bool)>> {
    let engine = ctx
        .current_engine()
        .ok_or_else(|| "未连接 Steam，请先登录".to_string())?;
    let mut out = Vec::new();
    for depot_id in depot_ids {
        out.push((depot_id, engine.check_access(app_id, depot_id).await));
    }
    Ok(out)
}

#[tauri::command]
pub async fn get_manifest_history(
    ctx: State<'_, Arc<AppContext>>,
    app_id: u32,
    depot_id: u32,
) -> CmdResult<Vec<ManifestVersion>> {
    let rt = ctx.runtime.lock().await.clone().ok_or_else(|| "未连接 Steam，请先登录".to_string())?;
    let info = rt
        .pics
        .app_info(app_id)
        .await
        .map_err(err)?
        .ok_or_else(|| format!("无法获取 App {} 的信息", app_id))?;

    let pics_list = history::branch_manifests(&info.kv, depot_id);
    let db_list = history::steamdb_history(depot_id).await;
    let merged = history::merged_history(db_list, pics_list);
    Ok(merged)
}

// ============================================================
// downloads
// ============================================================

#[derive(Debug, Clone, Deserialize)]
pub struct StartDownloadArgs {
    pub app_id: u32,
    #[serde(default)]
    pub depot_ids: Vec<u32>,
    #[serde(default = "default_branch")]
    pub branch: String,
    pub manifest_id: Option<u64>,
    pub install_dir: String,
    pub os: String,
    pub arch: String,
    pub language: String,
    #[serde(default)]
    pub all_languages: bool,
    #[serde(default)]
    pub file_filters: Vec<String>,
    #[serde(default)]
    pub verify_only: bool,
    #[serde(default = "default_max_conn")]
    pub max_connections: usize,
}

fn default_branch() -> String {
    "public".into()
}
fn default_max_conn() -> usize {
    16
}

#[tauri::command]
pub async fn start_download(ctx: State<'_, Arc<AppContext>>, args: StartDownloadArgs) -> CmdResult<String> {
    let engine: Arc<Engine> = ctx
        .current_engine()
        .ok_or_else(|| "未连接 Steam，请先登录".to_string())?;

    if args.install_dir.is_empty() {
        return Err("请选择安装目录".into());
    }
    let install_dir = PathBuf::from(&args.install_dir);
    if !args.verify_only {
        std::fs::create_dir_all(&install_dir).map_err(|e| format!("无法创建安装目录: {}", e))?;
    }

    let request = DownloadRequest {
        app_id: args.app_id,
        depot_ids: args.depot_ids,
        branch: args.branch,
        manifest_id: args.manifest_id,
        install_dir,
        os: args.os,
        arch: args.arch,
        language: args.language,
        all_languages: args.all_languages,
        file_filters: args.file_filters,
        verify_only: args.verify_only,
        max_connections: args.max_connections.clamp(1, 64),
    };
    Ok(engine.start_download(request))
}

#[tauri::command]
pub fn task_list(ctx: State<'_, Arc<AppContext>>) -> Vec<TaskSnapshot> {
    ctx.current_engine().map(|e| e.tasks()).unwrap_or_default()
}

#[tauri::command]
pub fn pause_task(ctx: State<'_, Arc<AppContext>>, task_id: String) -> CmdResult<()> {
    let engine = ctx.current_engine().ok_or_else(|| "引擎未就绪".to_string())?;
    let task = engine.task(&task_id).ok_or_else(|| "任务不存在".to_string())?;
    task.pause();
    Ok(())
}

#[tauri::command]
pub fn resume_task(ctx: State<'_, Arc<AppContext>>, task_id: String) -> CmdResult<()> {
    let engine = ctx.current_engine().ok_or_else(|| "引擎未就绪".to_string())?;
    let task = engine.task(&task_id).ok_or_else(|| "任务不存在".to_string())?;
    task.resume();
    Ok(())
}

#[tauri::command]
pub fn cancel_task(ctx: State<'_, Arc<AppContext>>, task_id: String) -> CmdResult<()> {
    let engine = ctx.current_engine().ok_or_else(|| "引擎未就绪".to_string())?;
    let task = engine.task(&task_id).ok_or_else(|| "任务不存在".to_string())?;
    task.cancel();
    Ok(())
}

#[tauri::command]
pub fn remove_task(ctx: State<'_, Arc<AppContext>>, task_id: String) -> CmdResult<()> {
    let engine = ctx.current_engine().ok_or_else(|| "引擎未就绪".to_string())?;
    engine.remove_task(&task_id);
    Ok(())
}

// ============================================================
// settings
// ============================================================

#[tauri::command]
pub fn get_settings(ctx: State<'_, Arc<AppContext>>) -> Settings {
    ctx.settings.read().clone()
}

#[tauri::command]
pub fn update_settings(ctx: State<'_, Arc<AppContext>>, settings: Settings) -> CmdResult<()> {
    *ctx.settings.write() = settings;
    ctx.settings.read().save();
    Ok(())
}

#[tauri::command]
pub fn delete_saved_account(ctx: State<'_, Arc<AppContext>>, account_name: String) -> CmdResult<()> {
    let mut s = ctx.settings.write();
    s.remove_account(&account_name);
    s.save();
    Ok(())
}

/// App name hint via applist (works offline of PICS).
#[tauri::command]
pub async fn app_name_hint(store: State<'_, Arc<StoreSearch>>, app_id: u32) -> CmdResult<Option<String>> {
    Ok(store.app_name_hint(app_id).await)
}
