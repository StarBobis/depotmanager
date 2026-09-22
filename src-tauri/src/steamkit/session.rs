//! High-level Steam session: connection bootstrap, logon flows, heartbeat.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context};
use parking_lot::Mutex;
use tokio::task::JoinHandle;

use super::connection::{CmConnection, ConnectionEvent};
use super::msgs::{emsg, eresult, steamid};
use super::proto_gen::*;

pub struct SteamSession {
    pub conn: Arc<CmConnection>,
    machine_id: Vec<u8>,
    cell_id: u32,
    heartbeat: Mutex<Option<JoinHandle<()>>>,
}

impl SteamSession {
    /// Wraps an existing connection.
    pub fn new(conn: Arc<CmConnection>, machine_id: Vec<u8>, cell_id: u32) -> Arc<Self> {
        Arc::new(SteamSession {
            conn,
            machine_id,
            cell_id,
            heartbeat: Mutex::new(None),
        })
    }

    /// Establishes a new connection (fetching CM endpoints first).
    pub async fn connect(
        cell_id: u32,
        machine_id: Vec<u8>,
        cached_endpoints: Option<Vec<String>>,
        events: tokio::sync::broadcast::Sender<ConnectionEvent>,
    ) -> anyhow::Result<Arc<Self>> {
        let endpoints = match cached_endpoints.filter(|e| !e.is_empty()) {
            Some(eps) => eps,
            None => match CmConnection::fetch_cm_list(cell_id).await {
                Ok(eps) => eps,
                Err(e) => {
                    let _ = events.send(ConnectionEvent::Log(format!(
                        "CM 目录获取失败（{}），使用内置节点",
                        e
                    )));
                    super::connection::FALLBACK_CM_ENDPOINTS
                        .iter()
                        .map(|s| s.to_string())
                        .collect()
                }
            },
        };
        let conn = CmConnection::connect(&endpoints, events).await?;
        Ok(Self::new(conn, machine_id, cell_id))
    }

    /// Logs on anonymously. Resolves with the assigned steamid.
    pub async fn logon_anonymous(&self) -> anyhow::Result<u64> {
        let mut rx = self.conn.events.subscribe();
        self.conn.state.lock().steamid = steamid::anonymous();

        let body = CMsgClientLogon {
            protocol_version: Some(super::msgs::CURRENT_PROTOCOL),
            client_package_version: Some(super::msgs::CLIENT_PACKAGE_VERSION),
            client_os_type: Some(20), // Windows 11
            client_language: Some("english".into()),
            cell_id: Some(self.cell_id),
            machine_id: Some(self.machine_id.clone()),
            supports_rate_limit_response: Some(true),
            ..Default::default()
        };

        self.conn
            .send_message(emsg::CLIENT_LOGON, &body)
            .context("发送匿名登录请求失败")?;
        let steamid = wait_logon(&mut rx, Duration::from_secs(20)).await?;
        self.start_heartbeat();
        Ok(steamid)
    }

    /// Logs on with an account name + refresh token (obtained from a previous
    /// auth session) or raw password (legacy path, requires Steam Guard codes).
    pub async fn logon_account(
        &self,
        account_name: &str,
        access_token: &str,
        should_remember: bool,
    ) -> anyhow::Result<u64> {
        let mut rx = self.conn.events.subscribe();
        let account_id = jwt_account_id(access_token).unwrap_or(0);
        self.conn.state.lock().steamid = steamid::individual(account_id);

        let body = CMsgClientLogon {
            protocol_version: Some(super::msgs::CURRENT_PROTOCOL),
            client_package_version: Some(super::msgs::CLIENT_PACKAGE_VERSION),
            client_os_type: Some(20),
            client_language: Some("english".into()),
            cell_id: Some(self.cell_id),
            machine_id: Some(self.machine_id.clone()),
            supports_rate_limit_response: Some(true),
            account_name: Some(account_name.to_string()),
            access_token: Some(access_token.to_string()),
            should_remember_password: Some(should_remember),
            ..Default::default()
        };

        self.conn
            .send_message(emsg::CLIENT_LOGON, &body)
            .context("发送账号登录请求失败")?;
        let steamid = wait_logon(&mut rx, Duration::from_secs(20)).await?;
        self.start_heartbeat();
        Ok(steamid)
    }

    fn start_heartbeat(&self) {
        self.stop_heartbeat();
        let conn = self.conn.clone();
        let handle = tokio::spawn(async move {
            loop {
                let secs = {
                    let s = conn.state.lock();
                    if !s.logged_on {
                        return;
                    }
                    s.heartbeat_seconds.max(5)
                };
                tokio::time::sleep(Duration::from_secs(secs as u64)).await;
                if !conn.is_logged_on() {
                    return;
                }
                if conn
                    .send_message(emsg::CLIENT_HEART_BEAT, &CMsgClientHeartBeat { send_reply: Some(false) })
                    .is_err()
                {
                    return;
                }
            }
        });
        *self.heartbeat.lock() = Some(handle);
    }

    pub fn stop_heartbeat(&self) {
        if let Some(h) = self.heartbeat.lock().take() {
            h.abort();
        }
    }

    pub fn is_logged_on(&self) -> bool {
        self.conn.is_logged_on()
    }

    pub fn disconnect(&self) {
        self.stop_heartbeat();
        self.conn.disconnect();
    }
}

/// Extracts the account id (lower 32 bits of steamid `sub`) from a Steam JWT.
pub fn jwt_account_id(token: &str) -> Option<u32> {
    let mut parts = token.split('.');
    parts.next()?;
    let payload = parts.next()?;
    use base64::Engine;
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload).ok()?;
    let json: serde_json::Value = serde_json::from_slice(&decoded).ok()?;
    let sub = json.get("sub")?.as_str()?.parse::<u64>().ok()?;
    Some(steamid::account_id(sub))
}

/// Extracts expiry (unix seconds) from a Steam JWT, if present.
pub fn jwt_expiry(token: &str) -> Option<u64> {
    let mut parts = token.split('.');
    parts.next()?;
    let payload = parts.next()?;
    use base64::Engine;
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload).ok()?;
    let json: serde_json::Value = serde_json::from_slice(&decoded).ok()?;
    json.get("exp")?.as_u64()
}

/// Waits for the logon result on the connection event stream.
async fn wait_logon(
    rx: &mut tokio::sync::broadcast::Receiver<super::connection::ConnectionEvent>,
    timeout: Duration,
) -> anyhow::Result<u64> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            bail!("登录超时");
        }
        let ev = tokio::time::timeout(remaining, rx.recv())
            .await
            .map_err(|_| anyhow!("登录超时"))?
            .map_err(|_| anyhow!("连接已关闭"))?;
        match ev {
            super::connection::ConnectionEvent::LoggedOn { steamid, .. } => return Ok(steamid),
            super::connection::ConnectionEvent::LogonFailed { eresult } => {
                bail!("登录失败: {} ({})", eresult::describe(eresult), eresult)
            }
            super::connection::ConnectionEvent::Disconnected { reason } => {
                bail!("连接断开: {}", reason)
            }
            _ => {}
        }
    }
}
