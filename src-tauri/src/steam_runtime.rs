//! Steam runtime: session lifecycle, auth flows, event bridging to the UI.

use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::engine::Engine;
use crate::state::{SavedAccount, Settings};
use crate::steamkit::auth::{AuthService, AuthSessionState};
use crate::steamkit::cdn::CdnClient;
use crate::steamkit::connection::{CmConnection, ConnectionEvent};
use crate::steamkit::pics::ProductInfoService;
use crate::steamkit::session::{jwt_account_id, SteamSession};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "phase")]
pub enum LoginState {
    Offline { message: String },
    Connecting,
    Authenticating { detail: String },
    LoggingOn,
    Online {
        account_name: String,
        steamid: u64,
        anonymous: bool,
    },
}

impl Default for LoginState {
    fn default() -> Self {
        LoginState::Offline {
            message: "未登录".into(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct AuthChallenge {
    /// "none" | "email_code" | "device_code" | "confirmation" | "qr"
    pub kind: String,
    pub message: String,
}

pub struct SteamRuntime {
    pub session: Arc<SteamSession>,
    pub conn: Arc<CmConnection>,
    pub pics: Arc<ProductInfoService>,
    pub cdn: Arc<CdnClient>,
}

pub enum PendingAuth {
    Qr {
        service: AuthService,
        state: AuthSessionState,
    },
    Credentials {
        service: AuthService,
        state: AuthSessionState,
        account_name: String,
        remember: bool,
    },
}

pub struct AppContext {
    pub settings: parking_lot::RwLock<Settings>,
    pub login_state: parking_lot::RwLock<LoginState>,
    pub runtime: Mutex<Option<Arc<SteamRuntime>>>,
    pub engine: parking_lot::RwLock<Option<Arc<Engine>>>,
    pub pending_auth: Mutex<Option<PendingAuth>>,
    pub app_handle: parking_lot::RwLock<Option<tauri::AppHandle>>,
    last_login: Mutex<Option<LastLogin>>,
}

#[derive(Clone)]
enum LastLogin {
    Anonymous,
    Account { name: String, token: String },
}

impl AppContext {
    pub fn new() -> Self {
        AppContext {
            settings: parking_lot::RwLock::new(Settings::load()),
            login_state: parking_lot::RwLock::new(LoginState::default()),
            runtime: Mutex::new(None),
            engine: parking_lot::RwLock::new(None),
            pending_auth: Mutex::new(None),
            app_handle: parking_lot::RwLock::new(None),
            last_login: Mutex::new(None),
        }
    }

    pub fn set_login_state(&self, state: LoginState) {
        *self.login_state.write() = state.clone();
        if let Some(h) = self.app_handle.read().as_ref() {
            use tauri::Emitter;
            let _ = h.emit("steam://state", &state);
        }
    }

    pub fn emit_log(&self, line: impl Into<String>) {
        if let Some(h) = self.app_handle.read().as_ref() {
            use tauri::Emitter;
            let _ = h.emit("steam://log", serde_json::json!({ "line": line.into() }));
        }
    }

    pub fn current_engine(&self) -> Option<Arc<Engine>> {
        self.engine.read().clone()
    }

    /// Connects to a CM and returns a fresh runtime (not yet logged on).
    pub async fn connect(self: &Arc<Self>) -> anyhow::Result<Arc<SteamRuntime>> {        self.set_login_state(LoginState::Connecting);
        let (settings, machine_id, cell_id, endpoints) = {
            let s = self.settings.read();
            (
                s.clone(),
                s.machine_id(),
                s.cell_id,
                if s.cm_endpoints.is_empty() {
                    None
                } else {
                    Some(s.cm_endpoints.clone())
                }
            )
        };
        let _ = settings;

        let (events, mut rx) = tokio::sync::broadcast::channel::<ConnectionEvent>(128);

        // forward connection logs to UI
        let app_handle = self.app_handle.read().clone();
        tokio::spawn(async move {
            while let Ok(ev) = rx.recv().await {
                if let ConnectionEvent::Log(line) = ev {
                    if let Some(h) = &app_handle {
                        use tauri::Emitter;
                        let _ = h.emit("steam://log", serde_json::json!({ "line": line }));
                    }
                }
            }
        });

        let session = SteamSession::connect(cell_id, machine_id, endpoints, events).await?;

        // cache the working endpoint immediately; refresh the full list in the
        // background (the directory API can be slow/unreachable on some networks)
        {
            let used = session.conn.endpoint_host();
            let mut s = self.settings.write();
            if !s.cm_endpoints.first().map(|e| e == &used).unwrap_or(false) {
                s.cm_endpoints.insert(0, used.clone());
                s.cm_endpoints.truncate(50);
                s.save();
            }
        }
        {
            let me = self.clone();
            tokio::spawn(async move {
                if let Ok(fresh) = CmConnection::fetch_cm_list(0).await {
                    let mut s = me.settings.write();
                    for e in fresh {
                        if !s.cm_endpoints.contains(&e) {
                            s.cm_endpoints.push(e);
                        }
                    }
                    s.cm_endpoints.truncate(50);
                    s.save();
                }
            });
        }

        let conn = session.conn.clone();
        let pics = Arc::new(ProductInfoService::new(conn.clone()));
        let cdn = Arc::new(CdnClient::new(conn.clone())?);
        let runtime = Arc::new(SteamRuntime {
            session,
            conn,
            pics,
            cdn,
        });
        *self.runtime.lock().await = Some(runtime.clone());

        // hook engine events -> UI
        let engine = Engine::new(
            runtime.conn.clone(),
            runtime.pics.clone(),
            runtime.cdn.clone(),
            tokio::sync::broadcast::channel(1024).0,
        );
        {
            let mut rx = engine.events.subscribe();
            let app_handle = self.app_handle.read().clone();
            tokio::spawn(async move {
                while let Ok(ev) = rx.recv().await {
                    if let Some(h) = &app_handle {
                        use tauri::Emitter;
                        match ev {
                            crate::engine::EngineEvent::TaskSnapshot(snap) => {
                                let _ = h.emit("task://snapshot", &snap);
                            }
                            crate::engine::EngineEvent::Log { task_id, line } => {
                                let _ = h.emit(
                                    "steam://log",
                                    serde_json::json!({ "line": format!("[{}] {}", &task_id[..6.min(task_id.len())], line) }),
                                );
                            }
                        }
                    }
                }
            });
        }
        *self.engine.write() = Some(engine);

        Ok(runtime)
    }

    /// Disconnects and clears runtime.
    pub async fn disconnect(&self) {
        if let Some(rt) = self.runtime.lock().await.take() {
            rt.session.disconnect();
        }
        *self.engine.write() = None;
    }

    /// Full anonymous login: reconnect + logon + state updates.
    pub async fn try_login_anonymous(self: &Arc<Self>) -> anyhow::Result<LoginState> {
        self.disconnect().await;
        let rt = self.connect().await?;
        self.set_login_state(LoginState::LoggingOn);
        rt.session.logon_anonymous().await?;
        let steamid = rt.conn.state().steamid;
        let state = LoginState::Online {
            account_name: "匿名用户".into(),
            steamid,
            anonymous: true,
        };
        self.set_login_state(state.clone());
        *self.last_login.lock().await = Some(LastLogin::Anonymous);
        self.spawn_reconnect_watcher(rt.conn.clone());
        Ok(state)
    }

    /// Full account login with a refresh token.
    pub async fn try_login_account(self: &Arc<Self>, name: &str, token: &str) -> anyhow::Result<LoginState> {
        self.disconnect().await;
        let rt = self.connect().await?;
        self.set_login_state(LoginState::LoggingOn);
        rt.session.logon_account(name, token, true).await?;
        let steamid = rt.conn.state().steamid;
        let state = LoginState::Online {
            account_name: name.to_string(),
            steamid,
            anonymous: false,
        };
        self.set_login_state(state.clone());
        *self.last_login.lock().await = Some(LastLogin::Account {
            name: name.to_string(),
            token: token.to_string(),
        });
        self.spawn_reconnect_watcher(rt.conn.clone());
        Ok(state)
    }

    /// Watches for unexpected disconnects and reconnects with the last login.
    fn spawn_reconnect_watcher(self: &Arc<Self>, conn: Arc<CmConnection>) {
        let ctx = self.clone();
        tokio::spawn(async move {
            let mut rx = conn.events.subscribe();
            while let Ok(ev) = rx.recv().await {
                let ConnectionEvent::Disconnected { reason } = ev else { continue };
                // stale connection? (a newer runtime replaced this one)
                let current = ctx.runtime.lock().await.clone();
                match current {
                    Some(rt) if Arc::ptr_eq(&rt.conn, &conn) => {}
                    _ => return,
                }
                if reason.contains("主动断开") {
                    return; // user-initiated
                }
                let login = ctx.last_login.lock().await.clone();
                let Some(login) = login else { return };

                ctx.set_login_state(LoginState::Offline {
                    message: format!("连接断开（{}），正在重连...", reason),
                });
                for attempt in 1..=3u32 {
                    tokio::time::sleep(Duration::from_secs(2 + attempt as u64 * 3)).await;
                    let res = match &login {
                        LastLogin::Anonymous => ctx.try_login_anonymous().await,
                        LastLogin::Account { name, token } => {
                            ctx.try_login_account(name, token).await
                        }
                    };
                    if res.is_ok() {
                        return; // new watcher was spawned by try_login_*
                    }
                }
                ctx.set_login_state(LoginState::Offline {
                    message: "自动重连失败，请重新登录".into(),
                });
                return;
            }
        });
    }

    /// Startup auto-login: last saved account, else anonymous.
    pub async fn auto_login(self: &Arc<Self>) {
        let creds = {
            let s = self.settings.read();
            s.last_account
                .as_ref()
                .and_then(|name| s.find_account(name))
                .map(|a| (a.account_name.clone(), a.refresh_token.clone()))
        };
        if let Some((name, token)) = creds {
            if self.try_login_account(&name, &token).await.is_ok() {
                return;
            }
        }
        let _ = self.try_login_anonymous().await;
    }

    /// Polls the pending auth session; on success saves the account and logs on.
    pub async fn poll_auth_once(&self) -> anyhow::Result<Option<(String, String, Option<String>)>> {
        let mut guard = self.pending_auth.lock().await;
        let Some(pending) = guard.as_mut() else {
            anyhow::bail!("没有进行中的登录会话");
        };
        let (service, state) = match pending {
            PendingAuth::Qr { service, state } => (service, state),
            PendingAuth::Credentials { service, state, .. } => (service, state),
        };
        match service.poll_once(state).await? {
            Some(result) => {
                let account_name = result.account_name.clone();
                let refresh = result.refresh_token.clone();
                let guard_data = result.new_guard_data.clone();

                // save account
                {
                    let steamid = jwt_account_id(&refresh)
                        .map(|id| (1u64 << 56) | (1u64 << 52) | id as u64)
                        .unwrap_or(0);
                    let mut s = self.settings.write();
                    s.upsert_account(SavedAccount {
                        account_name: account_name.clone(),
                        steamid,
                        refresh_token: refresh.clone(),
                        guard_data: guard_data.clone(),
                    });
                    s.last_account = Some(account_name.clone());
                    s.save();
                }
                *guard = None;
                Ok(Some((account_name, refresh, guard_data)))
            }
            None => Ok(None),
        }
    }

    pub async fn pending_auth_interval(&self) -> f32 {
        self.pending_auth
            .lock()
            .await
            .as_ref()
            .map(|p| match p {
                PendingAuth::Qr { state, .. } => state.interval,
                PendingAuth::Credentials { state, .. } => state.interval,
            })
            .unwrap_or(5.0)
    }

    /// Submits a guard code to the pending credentials auth session.
    pub async fn submit_guard_code(&self, code: &str, code_type: i32) -> anyhow::Result<()> {
        let guard = self.pending_auth.lock().await;
        let Some(pending) = guard.as_ref() else {
            anyhow::bail!("没有进行中的登录会话");
        };
        match pending {
            PendingAuth::Credentials { service, state, .. } => {
                service.submit_guard_code(state, code, code_type).await
            }
            PendingAuth::Qr { .. } => anyhow::bail!("二维码登录无需验证码"),
        }
    }

    pub async fn begin_qr_auth(&self) -> anyhow::Result<String> {
        let rt = self.runtime.lock().await.clone().ok_or_else(|| anyhow::anyhow!("未连接"))?;
        let service = AuthService::new(rt.conn.clone());
        let state = service.begin_qr().await?;
        let url = state.challenge_url.clone().unwrap_or_default();
        *self.pending_auth.lock().await = Some(PendingAuth::Qr { service, state });
        Ok(url)
    }

    pub async fn begin_credentials_auth(
        &self,
        account_name: &str,
        password: &str,
        remember: bool,
    ) -> anyhow::Result<AuthChallenge> {
        let rt = self.runtime.lock().await.clone().ok_or_else(|| anyhow::anyhow!("未连接"))?;
        let service = AuthService::new(rt.conn.clone());
        let state = service
            .begin_credentials(account_name, password, remember, None)
            .await?;

        let challenge = challenge_from_state(&state);
        *self.pending_auth.lock().await = Some(PendingAuth::Credentials {
            service,
            state,
            account_name: account_name.to_string(),
            remember,
        });
        Ok(challenge)
    }

    pub fn challenge_for_pending(&self) -> AuthChallenge {
        // best-effort; used by UI after submit_code to re-render
        AuthChallenge {
            kind: "confirmation".into(),
            message: "等待确认...".into(),
        }
    }
}

pub fn challenge_from_state(state: &AuthSessionState) -> AuthChallenge {
    use crate::steamkit::auth::guard_type;
    // prefer device code, then email code, then any confirmation
    let mut kind = "none";
    let mut message = String::new();
    for (ty, msg) in &state.allowed_confirmations {
        match *ty {
            guard_type::DEVICE_CODE => {
                kind = "device_code";
                message = if msg.is_empty() {
                    "请输入 Steam 手机令牌验证码".into()
                } else {
                    msg.clone()
                };
                break;
            }
            guard_type::EMAIL_CODE => {
                kind = "email_code";
                message = if msg.is_empty() {
                    "请输入发送到邮箱的验证码".into()
                } else {
                    msg.clone()
                };
            }
            guard_type::DEVICE_CONFIRMATION | guard_type::EMAIL_CONFIRMATION => {
                if kind == "none" {
                    kind = "confirmation";
                    message = if msg.is_empty() {
                        "请在 Steam 手机应用上确认登录".into()
                    } else {
                        msg.clone()
                    };
                }
            }
            _ => {}
        }
    }
    if kind == "none" {
        message = "无需额外验证".into();
    }
    AuthChallenge {
        kind: kind.to_string(),
        message,
    }
}

/// Human-friendly account id extraction for display.
pub fn steamid_from_token(token: &str) -> u64 {
    jwt_account_id(token)
        .map(|id| (1u64 << 56) | (1u64 << 52) | id as u64)
        .unwrap_or(0)
}

pub async fn wait_logged_off(conn: &Arc<CmConnection>, timeout: Duration) {
    let mut rx = conn.events.subscribe();
    let _ = tokio::time::timeout(timeout, async {
        while let Ok(ev) = rx.recv().await {
            if matches!(ev, ConnectionEvent::Disconnected { .. }) {
                break;
            }
        }
    })
    .await;
}
