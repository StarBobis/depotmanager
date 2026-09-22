//! CM (content manager) WebSocket connection actor.
//!
//! Handles framing, job-based request/response routing, Multi message
//! unpacking and connection lifecycle events.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context};
use dashmap::DashMap;
use flate2::read::GzDecoder;
use futures_util::{SinkExt, StreamExt};
use parking_lot::Mutex;
use prost::Message;
use tokio::sync::{broadcast, mpsc};
use tokio_tungstenite::tungstenite::Message as WsMessage;

use super::msgs::{self, emsg, eresult, make_header, parse_packet, PacketMsg};
use super::proto_gen::*;

const CM_LIST_URL: &str =
    "https://api.steampowered.com/ISteamDirectory/GetCMListForConnect/v1/?cellid={cell}";

/// Bootstrap websocket CM endpoints used when the directory API is unreachable
/// (observed 2026-02; the API is consulted first whenever possible).
pub const FALLBACK_CM_ENDPOINTS: &[&str] = &[
    "cmp1-hkg1.steamserver.net:27024",
    "cmp3-hkg1.steamserver.net:27025",
    "cmp2-hkg1.steamserver.net:27025",
    "cmp1-hkg1.steamserver.net:27025",
    "cmp2-hkg1.steamserver.net:27018",
    "cmp3-hkg1.steamserver.net:27024",
    "cmp1-hkg1.steamserver.net:27023",
    "cmp3-hkg1.steamserver.net:27021",
    "cmp2-hkg1.steamserver.net:27019",
    "cmp3-hkg1.steamserver.net:27022",
    "cmp2-hkg1.steamserver.net:27021",
    "cmp3-hkg1.steamserver.net:27020",
    "cmp2-hkg1.steamserver.net:27023",
    "cmp1-hkg1.steamserver.net:27019",
    "cmp2-hkg1.steamserver.net:27022",
];

/// Events broadcast by the connection / session layers.
#[derive(Debug, Clone)]
pub enum ConnectionEvent {
    Connected,
    /// Logon succeeded.
    LoggedOn { steamid: u64, cell_id: u32, ip_country: String },
    /// Logon failed with an EResult.
    LogonFailed { eresult: i32 },
    /// Received the account license list.
    Licenses { count: usize },
    /// Server told us we got logged off.
    LoggedOff { eresult: i32 },
    /// The connection was lost (or shut down).
    Disconnected { reason: String },
    /// Free-form log line for the UI.
    Log(String),
}

struct JobEntry {
    /// EMsg of the expected response (request emsg + 1); used for fallback
    /// routing when the server does not echo job ids (e.g. logon, free license).
    expected_emsg: u32,
    tx: mpsc::UnboundedSender<PacketMsg>,
}

enum Cmd {
    Send(Vec<u8>),
    Shutdown,
}

#[derive(Default)]
pub struct ConnectionState {
    pub steamid: u64,
    pub session_id: i32,
    pub cell_id: u32,
    pub logged_on: bool,
    pub heartbeat_seconds: u32,
    pub licenses: Vec<(u32, u64)>, // (package_id, access_token)
}

pub struct CmConnection {
    cmd_tx: mpsc::UnboundedSender<Cmd>,
    jobs: Arc<DashMap<u64, JobEntry>>,
    next_job: Arc<AtomicU64>,
    pub events: broadcast::Sender<ConnectionEvent>,
    pub state: Arc<Mutex<ConnectionState>>,
    endpoint: String,
}

impl CmConnection {
    pub fn state(&self) -> ConnectionStateSnapshot {
        let s = self.state.lock();
        ConnectionStateSnapshot {
            steamid: s.steamid,
            session_id: s.session_id,
            cell_id: s.cell_id,
            logged_on: s.logged_on,
            licenses: s.licenses.clone(),
        }
    }

    pub fn is_logged_on(&self) -> bool {
        self.state.lock().logged_on
    }

    pub fn set_logged_off(&self) {
        let mut s = self.state.lock();
        s.logged_on = false;
        s.session_id = 0;
    }

    /// Fetches the list of websocket CM endpoints from the Steam directory.
    pub async fn fetch_cm_list(cell_id: u32) -> anyhow::Result<Vec<String>> {
        let url = CM_LIST_URL.replace("{cell}", &cell_id.to_string());
        let client = super::http_client()?;
        let mut last_err = anyhow!("request failed");
        for _ in 0..3 {
            match client.get(&url).timeout(Duration::from_secs(20)).send().await {
                Ok(resp) => {
                    let resp: serde_json::Value = resp.json().await.context("CM list parse failed")?;
                    let mut endpoints = Vec::new();
                    if let Some(list) = resp
                        .get("response")
                        .and_then(|r| r.get("serverlist"))
                        .and_then(|s| s.as_array())
                    {
                        for entry in list {
                            let ty = entry.get("type").and_then(|t| t.as_str()).unwrap_or("");
                            if ty == "websockets" {
                                if let Some(ep) = entry.get("endpoint").and_then(|e| e.as_str()) {
                                    endpoints.push(ep.to_string());
                                }
                            }
                        }
                    }
                    if endpoints.is_empty() {
                        bail!("Steam directory returned no websocket CM servers");
                    }
                    return Ok(endpoints);
                }
                Err(e) => last_err = e.into(),
            }
            tokio::time::sleep(Duration::from_millis(800)).await;
        }
        Err(last_err.context("CM list request failed"))
    }

    /// Connects to a CM server, trying each endpoint in order.
    pub async fn connect(
        endpoints: &[String],
        events: broadcast::Sender<ConnectionEvent>,
    ) -> anyhow::Result<Arc<Self>> {
        let mut last_err = anyhow!("no endpoints");
        for endpoint in endpoints.iter().take(6) {
            let url = format!("wss://{}/cmsocket/", endpoint);
            match Self::connect_one(&url, endpoint, events.clone()).await {
                Ok(conn) => return Ok(conn),
                Err(e) => {
                    let _ = events.send(ConnectionEvent::Log(format!(
                        "连接 CM {} 失败: {}",
                        endpoint, e
                    )));
                    last_err = e;
                }
            }
        }
        Err(last_err.context("所有 CM 服务器均连接失败"))
    }

    async fn connect_one(
        url: &str,
        endpoint: &str,
        events: broadcast::Sender<ConnectionEvent>,
    ) -> anyhow::Result<Arc<Self>> {
        let (ws, _resp) = tokio::time::timeout(
            Duration::from_secs(15),
            tokio_tungstenite::connect_async(url),
        )
        .await
        .context("websocket connect timeout")??;

        let (mut sink, mut stream) = ws.split();
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<Cmd>();
        let jobs: Arc<DashMap<u64, JobEntry>> = Arc::new(DashMap::new());
        let state = Arc::new(Mutex::new(ConnectionState::default()));
        let events2 = events.clone();
        let jobs2 = jobs.clone();
        let state2 = state.clone();

        // Send ClientHello first thing, as required by the WS transport.
        let hello = msgs::encode_packet(
            emsg::CLIENT_HELLO,
            &make_header(0, 0, msgs::JOBID_NONE, msgs::JOBID_NONE, None),
            &CMsgClientHello { protocol_version: Some(msgs::CURRENT_PROTOCOL) },
        );
        cmd_tx
            .send(Cmd::Send(hello))
            .map_err(|_| anyhow!("failed to queue ClientHello"))?;

        tokio::spawn(async move {
            let shutdown_reason: String;
            loop {
                tokio::select! {
                    cmd = cmd_rx.recv() => {
                        match cmd {
                            Some(Cmd::Send(bytes)) => {
                                if let Err(e) = sink.send(WsMessage::Binary(bytes.into())).await {
                                    shutdown_reason = format!("发送失败: {e}");
                                    break;
                                }
                            }
                            Some(Cmd::Shutdown) | None => {
                                shutdown_reason = "客户端主动断开".into();
                                break;
                            }
                        }
                    }
                    frame = stream.next() => {
                        match frame {
                            Some(Ok(WsMessage::Binary(data))) => {
                                if let Err(e) = dispatch_frame(&data, &jobs2, &state2, &events2) {
                                    let _ = events2.send(ConnectionEvent::Log(format!("处理消息出错: {e}")));
                                }
                            }
                            Some(Ok(WsMessage::Close(_))) => {
                                shutdown_reason = "服务器关闭了连接".into();
                                break;
                            }
                            Some(Ok(_)) => { /* ping/pong/text ignored */ }
                            Some(Err(e)) => {
                                shutdown_reason = format!("网络错误: {e}");
                                break;
                            }
                            None => {
                                shutdown_reason = "连接被关闭".into();
                                break;
                            }
                        }
                    }
                }
            }

            // connection closed: mark logged off, fail all pending jobs
            state2.lock().logged_on = false;
            jobs2.clear();
            let _ = events2.send(ConnectionEvent::Disconnected { reason: shutdown_reason });
        });

        let _ = events.send(ConnectionEvent::Connected);
        Ok(Arc::new(CmConnection {
            cmd_tx,
            jobs,
            next_job: Arc::new(AtomicU64::new(1)),
            events,
            state,
            endpoint: endpoint.to_string(),
        }))
    }

    /// The endpoint host this connection is using.
    pub fn endpoint_host(&self) -> String {
        self.endpoint.clone()
    }

    fn next_job_id(&self) -> u64 {
        self.next_job.fetch_add(1, Ordering::Relaxed)
    }

    /// Sends a fire-and-forget protobuf message with session fields filled in.
    pub fn send_message<M: Message>(&self, emsg: u32, body: &M) -> anyhow::Result<()> {
        self.send_with_job(emsg, body, None).map(|_| ())
    }

    /// Sends a message; returns the job id assigned when `with_job` is true.
    pub fn send_with_job<M: Message>(
        &self,
        emsg: u32,
        body: &M,
        with_job: Option<u64>,
    ) -> anyhow::Result<u64> {
        let (steamid, session_id) = {
            let s = self.state.lock();
            (s.steamid, s.session_id)
        };
        let job = with_job.unwrap_or(msgs::JOBID_NONE);
        let header = make_header(steamid, session_id, job, msgs::JOBID_NONE, None);
        let bytes = msgs::encode_packet(emsg, &header, body);
        self.cmd_tx
            .send(Cmd::Send(bytes))
            .map_err(|_| anyhow!("连接已关闭，无法发送"))?;
        Ok(job)
    }

    /// Performs a job-based request that expects exactly one response packet.
    pub async fn call<M: Message>(
        &self,
        emsg: u32,
        body: &M,
        timeout: Duration,
    ) -> anyhow::Result<PacketMsg> {
        let (job, mut rx) = self.register_job(emsg + 1);
        self.send_with_job(emsg, body, Some(job))?;
        let res = tokio::time::timeout(timeout, rx.recv()).await;
        self.jobs.remove(&job);
        match res {
            Ok(Some(pkt)) => Ok(pkt),
            Ok(None) => bail!("连接已关闭"),
            Err(_) => bail!("请求超时"),
        }
    }

    /// Performs a job-based request that expects multiple response packets,
    /// collected until `done` returns true for the latest batch.
    pub async fn call_multi<M: Message>(
        &self,
        emsg: u32,
        body: &M,
        timeout: Duration,
        done: impl Fn(&[PacketMsg]) -> bool,
    ) -> anyhow::Result<Vec<PacketMsg>> {
        let (job, mut rx) = self.register_job(emsg + 1);
        self.send_with_job(emsg, body, Some(job))?;
        let mut out = Vec::new();
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                self.jobs.remove(&job);
                bail!("请求超时");
            }
            match tokio::time::timeout(remaining, rx.recv()).await {
                Ok(Some(pkt)) => {
                    out.push(pkt);
                    if done(&out) {
                        break;
                    }
                }
                Ok(None) => {
                    self.jobs.remove(&job);
                    bail!("连接已关闭");
                }
                Err(_) => {
                    self.jobs.remove(&job);
                    bail!("请求超时");
                }
            }
        }
        self.jobs.remove(&job);
        Ok(out)
    }

    /// Calls a unified service method, e.g. `ContentServerDirectory.GetServersForSteamPipe#1`.
    /// Returns (eresult, response body bytes).
    pub async fn unified_call<Req: Message>(
        &self,
        service_method: &str,
        body: &Req,
        timeout: Duration,
    ) -> anyhow::Result<(i32, PacketMsg)> {
        let (job, mut rx) = self.register_job(emsg::SERVICE_METHOD_RESPONSE);
        let (steamid, session_id, logged_on) = {
            let s = self.state.lock();
            (s.steamid, s.session_id, s.logged_on)
        };
        let emsg = if logged_on {
            emsg::SERVICE_METHOD_CALL_FROM_CLIENT
        } else {
            emsg::SERVICE_METHOD_CALL_FROM_CLIENT_NONAUTHED
        };
        let header = make_header(
            steamid,
            session_id,
            job,
            msgs::JOBID_NONE,
            Some(service_method.to_string()),
        );
        let bytes = msgs::encode_packet(emsg, &header, body);
        self.cmd_tx
            .send(Cmd::Send(bytes))
            .map_err(|_| anyhow!("连接已关闭，无法发送"))?;

        let res = tokio::time::timeout(timeout, rx.recv()).await;
        self.jobs.remove(&job);
        match res {
            Ok(Some(pkt)) => Ok((pkt.header.eresult.unwrap_or(eresult::FAIL), pkt)),
            Ok(None) => bail!("连接已关闭"),
            Err(_) => bail!("请求超时: {}", service_method),
        }
    }

    fn register_job(&self, expected_emsg: u32) -> (u64, mpsc::UnboundedReceiver<PacketMsg>) {
        let job = self.next_job_id();
        let (tx, rx) = mpsc::unbounded_channel();
        self.jobs.insert(job, JobEntry { expected_emsg, tx });
        (job, rx)
    }

    /// Gracefully shuts the connection down (sends ClientLogOff if logged on).
    pub fn disconnect(&self) {
        if self.is_logged_on() {
            let _ = self.send_message(emsg::CLIENT_LOG_OFF, &CMsgClientLogOff {});
        }
        let _ = self.cmd_tx.send(Cmd::Shutdown);
    }
}

impl Drop for CmConnection {
    fn drop(&mut self) {
        if self.is_logged_on() {
            let _ = self.send_message(emsg::CLIENT_LOG_OFF, &CMsgClientLogOff {});
        }
    }
}

#[derive(Clone, Default)]
pub struct ConnectionStateSnapshot {
    pub steamid: u64,
    pub session_id: i32,
    pub cell_id: u32,
    pub logged_on: bool,
    pub licenses: Vec<(u32, u64)>,
}

/// Routes one received WS frame.
fn dispatch_frame(
    data: &[u8],
    jobs: &Arc<DashMap<u64, JobEntry>>,
    state: &Arc<Mutex<ConnectionState>>,
    events: &broadcast::Sender<ConnectionEvent>,
) -> anyhow::Result<()> {
    let pkt = match parse_packet(data)? {
        Some(p) => p,
        None => return Ok(()),
    };
    dispatch_packet(pkt, jobs, state, events)
}

fn dispatch_packet(
    pkt: PacketMsg,
    jobs: &Arc<DashMap<u64, JobEntry>>,
    state: &Arc<Mutex<ConnectionState>>,
    events: &broadcast::Sender<ConnectionEvent>,
) -> anyhow::Result<()> {
    if std::env::var_os("DSH_DEBUG_PACKETS").is_some() {
        let _ = events.send(ConnectionEvent::Log(format!(
            "[pkt] emsg={} target_job={} len={}",
            pkt.emsg,
            pkt.jobid_target(),
            pkt.body.len()
        )));
    }
    match pkt.emsg {
        emsg::MULTI => {
            let multi: CMsgMulti = pkt.decode_body()?;
            let body = multi.message_body.unwrap_or_default();
            let plain = if multi.size_unzipped.unwrap_or(0) > 0 {
                use std::io::Read;
                let mut out = Vec::new();
                GzDecoder::new(&body[..]).read_to_end(&mut out)?;
                out
            } else {
                body
            };
            let mut cur = &plain[..];
            while cur.len() >= 4 {
                let len = u32::from_le_bytes(cur[..4].try_into().unwrap()) as usize;
                cur = &cur[4..];
                if cur.len() < len {
                    break;
                }
                let sub = &cur[..len];
                cur = &cur[len..];
                if let Some(sub_pkt) = parse_packet(sub)? {
                    dispatch_packet(sub_pkt, jobs, state, events)?;
                }
            }
            Ok(())
        }
        emsg::CLIENT_LOG_ON_RESPONSE => {
            let resp: CMsgClientLogonResponse = pkt.decode_body()?;
            let result = resp.eresult.unwrap_or(eresult::FAIL);
            if result == eresult::OK {
                {
                    let mut s = state.lock();
                    s.steamid = pkt.header.steamid.unwrap_or(0);
                    s.session_id = pkt.header.client_sessionid.unwrap_or(0);
                    s.cell_id = resp.cell_id.unwrap_or(0);
                    s.logged_on = true;
                    s.heartbeat_seconds = resp.heartbeat_seconds.unwrap_or(15).max(5) as u32;
                }
                let _ = events.send(ConnectionEvent::LoggedOn {
                    steamid: pkt.header.steamid.unwrap_or(0),
                    cell_id: resp.cell_id.unwrap_or(0),
                    ip_country: resp.ip_country_code.clone().unwrap_or_default(),
                });
            } else {
                let _ = events.send(ConnectionEvent::LogonFailed { eresult: result });
            }
            route_job(pkt, jobs, events);
            Ok(())
        }
        emsg::CLIENT_LOGGED_OFF => {
            let resp: CMsgClientLoggedOff = pkt.decode_body()?;
            state.lock().logged_on = false;
            let _ = events.send(ConnectionEvent::LoggedOff {
                eresult: resp.eresult.unwrap_or(eresult::FAIL),
            });
            Ok(())
        }
        emsg::CLIENT_LICENSE_LIST => {
            let resp: CMsgClientLicenseList = pkt.decode_body()?;
            let licenses: Vec<(u32, u64)> = resp
                .licenses
                .iter()
                .map(|l| (l.package_id.unwrap_or(0), l.access_token.unwrap_or(0)))
                .collect();
            let count = licenses.len();
            state.lock().licenses = licenses;
            let _ = events.send(ConnectionEvent::Licenses { count });
            Ok(())
        }
        emsg::CLIENT_SERVER_UNAVAILABLE => {
            let _ = events.send(ConnectionEvent::Log("CM 报告服务不可用".into()));
            Ok(())
        }
        _ => {
            route_job(pkt, jobs, events);
            Ok(())
        }
    }
}

fn route_job(
    pkt: PacketMsg,
    jobs: &Arc<DashMap<u64, JobEntry>>,
    events: &broadcast::Sender<ConnectionEvent>,
) {
    let target = pkt.jobid_target();
    if target != msgs::JOBID_NONE {
        if let Some(entry) = jobs.get(&target) {
            let _ = entry.tx.send(pkt);
        } else {
            let _ = events.send(ConnectionEvent::Log(format!(
                "未路由消息: emsg={} target_job={}（无等待任务）",
                pkt.emsg, target
            )));
        }
        return;
    }
    // Server did not echo a job id (logon, free license, ...): fall back to
    // matching by expected response EMsg.
    for entry in jobs.iter() {
        if entry.expected_emsg == pkt.emsg {
            let _ = entry.tx.send(pkt);
            return;
        }
    }
    let _ = events.send(ConnectionEvent::Log(format!(
        "未路由消息: emsg={}（无 jobid 且无匹配任务）",
        pkt.emsg
    )));
}
