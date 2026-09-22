//! Steam CDN (Steampipe) downloads: content server discovery, depot keys,
//! manifest & chunk downloads with decryption / decompression / verification.

use std::io::{Cursor, Read, Write};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context};
use dashmap::DashMap;
use parking_lot::RwLock;

use super::connection::CmConnection;
use super::crypto;
use super::manifest::{ChunkData, DepotManifest};
use super::msgs::{emsg, eresult};
use super::proto_gen::*;

#[derive(Debug, Clone)]
pub struct CdnServer {
    pub host: String,
    pub vhost: String,
    pub https: bool,
    pub server_type: String,
    pub cell_id: i32,
    pub weighted_load: f32,
    pub num_entries: i32,
    pub allowed_app_ids: Vec<u32>,
    pub use_as_proxy: bool,
    pub steam_china_only: bool,
}

impl CdnServer {
    pub fn port(&self) -> u16 {
        if self.https { 443 } else { 80 }
    }

    fn base_url(&self) -> String {
        format!(
            "{}://{}:{}",
            if self.https { "https" } else { "http" },
            self.vhost,
            self.port()
        )
    }
}

pub struct CdnClient {
    conn: Arc<CmConnection>,
    http: reqwest::Client,
    servers: RwLock<Vec<CdnServer>>,
    auth_tokens: DashMap<(u32, String), (String, u64)>,
    failures: DashMap<String, u32>,
    next_server: AtomicUsize,
}

impl CdnClient {
    pub fn new(conn: Arc<CmConnection>) -> anyhow::Result<Self> {
        Ok(CdnClient {
            conn,
            http: super::http_client()?,
            servers: RwLock::new(Vec::new()),
            auth_tokens: DashMap::new(),
            failures: DashMap::new(),
            next_server: AtomicUsize::new(0),
        })
    }

    /// Refreshes the content server list, filtered for the given app.
    pub async fn update_servers(&self, cell_id: u32, app_id: u32) -> anyhow::Result<usize> {
        let req = CContentServerDirectoryGetServersForSteamPipeRequest {
            cell_id: Some(cell_id),
            max_servers: Some(40),
            ip_override: None,
            launcher_type: None,
        };
        let (result, pkt) = self
            .conn
            .unified_call(
                "ContentServerDirectory.GetServersForSteamPipe#1",
                &req,
                Duration::from_secs(20),
            )
            .await?;
        if result != eresult::OK {
            bail!("获取 CDN 服务器列表失败: {} ({})", eresult::describe(result), result);
        }
        let resp: CContentServerDirectoryGetServersForSteamPipeResponse = pkt.decode_body()?;

        let mut servers: Vec<CdnServer> = resp
            .servers
            .into_iter()
            .filter(|s| {
                let ty = s.r#type.clone().unwrap_or_default();
                (ty == "SteamCache" || ty == "CDN")
                    && (s.allowed_app_ids.is_empty() || s.allowed_app_ids.contains(&app_id))
            })
            .map(|s| {
                let https = s.https_support.as_deref() == Some("mandatory");
                let host = s.host.unwrap_or_default();
                let vhost = s.vhost.unwrap_or_else(|| host.clone());
                CdnServer {
                    host,
                    vhost,
                    https,
                    server_type: s.r#type.unwrap_or_default(),
                    cell_id: s.cell_id.unwrap_or(0),
                    weighted_load: s.weighted_load.unwrap_or(0.0),
                    num_entries: s.num_entries_in_client_list.unwrap_or(1).max(1),
                    allowed_app_ids: s.allowed_app_ids,
                    use_as_proxy: s.use_as_proxy.unwrap_or(false),
                    steam_china_only: s.steam_china_only.unwrap_or(false),
                }
            })
            .collect();

        if servers.is_empty() {
            bail!("没有可用的 CDN 服务器");
        }
        // Prefer servers closer (lower weighted load first) but keep some variety.
        servers.sort_by(|a, b| {
            a.weighted_load
                .partial_cmp(&b.weighted_load)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        let n = servers.len();
        *self.servers.write() = servers;
        Ok(n)
    }

    /// Requests the manifest request code needed to download a (possibly old)
    /// manifest. May return 0 when the code is not granted.
    pub async fn get_manifest_request_code(
        &self,
        depot_id: u32,
        app_id: u32,
        manifest_id: u64,
        branch: &str,
    ) -> anyhow::Result<u64> {
        let branch = if branch.eq_ignore_ascii_case("public") {
            None
        } else {
            Some(branch.to_string())
        };
        let req = CContentServerDirectoryGetManifestRequestCodeRequest {
            app_id: Some(app_id),
            depot_id: Some(depot_id),
            manifest_id: Some(manifest_id),
            app_branch: branch,
            branch_password_hash: None,
        };
        let (result, pkt) = self
            .conn
            .unified_call(
                "ContentServerDirectory.GetManifestRequestCode#1",
                &req,
                Duration::from_secs(20),
            )
            .await?;
        if result != eresult::OK {
            // Non-OK here usually means old-manifest downloads are blocked;
            // surface as code 0 so the caller can fall back.
            return Ok(0);
        }
        let resp: CContentServerDirectoryGetManifestRequestCodeResponse = pkt.decode_body()?;
        Ok(resp.manifest_request_code.unwrap_or(0))
    }

    /// Requests a CDN auth token for a (depot, host) pair (cached with expiry).
    pub async fn cdn_auth_token(&self, app_id: u32, depot_id: u32, host: &str) -> anyhow::Result<String> {
        let key = (depot_id, host.to_string());
        if let Some(entry) = self.auth_tokens.get(&key) {
            let (token, expiry) = entry.value();
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            if *expiry == 0 || *expiry > now + 30 {
                return Ok(token.clone());
            }
        }
        let req = CContentServerDirectoryGetCdnAuthTokenRequest {
            depot_id: Some(depot_id),
            host_name: Some(host.to_string()),
            app_id: Some(app_id),
        };
        let (result, pkt) = self
            .conn
            .unified_call(
                "ContentServerDirectory.GetCDNAuthToken#1",
                &req,
                Duration::from_secs(20),
            )
            .await?;
        if result != eresult::OK {
            bail!(
                "获取 CDN 授权失败({}): {} ({})",
                host,
                eresult::describe(result),
                result
            );
        }
        let resp: CContentServerDirectoryGetCdnAuthTokenResponse = pkt.decode_body()?;
        let token = resp.token.unwrap_or_default();
        self.auth_tokens
            .insert(key, (token.clone(), resp.expiration_time.unwrap_or(0) as u64));
        Ok(token)
    }

    fn pick_server(&self) -> Option<CdnServer> {
        let servers = self.servers.read();
        if servers.is_empty() {
            return None;
        }
        let idx = self.next_server.fetch_add(1, Ordering::Relaxed);
        Some(servers[idx % servers.len()].clone())
    }

    fn note_failure(&self, server: &CdnServer) {
        *self.failures.entry(server.host.clone()).or_insert(0) += 1;
        self.next_server.fetch_add(1, Ordering::Relaxed);
    }

    fn note_success(&self, server: &CdnServer) {
        self.failures.remove(&server.host);
    }

    /// Downloads and parses a depot manifest (decrypting filenames if needed).
    pub async fn download_manifest(
        &self,
        app_id: u32,
        depot_id: u32,
        manifest_id: u64,
        request_code: u64,
        depot_key: &[u8],
    ) -> anyhow::Result<DepotManifest> {
        let mut last_err = anyhow!("no server");
        for attempt in 0..6 {
            let server = self.pick_server().context("CDN 服务器列表为空")?;
            let mut url = format!(
                "{}/depot/{}/manifest/{}/5/{}",
                server.base_url(),
                depot_id,
                manifest_id,
                request_code
            );

            let mut authed = false;
            match self.get_bytes(&server, &url, depot_id, app_id, &mut authed, 60).await {
                Ok(bytes) => {
                    self.note_success(&server);
                    let plain = unzip_single(&bytes).context("manifest zip 解压失败")?;
                    let mut manifest = DepotManifest::parse(&plain).context("manifest 解析失败")?;
                    if manifest.filenames_encrypted {
                        manifest.decrypt_filenames(depot_key)?;
                    }
                    return Ok(manifest);
                }
                Err(e) => {
                    if is_auth_error(&e) && !authed {
                        // fetch CDN auth token and retry same server once
                        match self.cdn_auth_token(app_id, depot_id, &server.host).await {
                            Ok(token) => {
                                if !token.is_empty() {
                                    url = append_token(&url, &token);
                                    authed = true;
                                    match self.get_bytes(&server, &url, depot_id, app_id, &mut authed, 60).await {
                                        Ok(bytes) => {
                                            let plain = unzip_single(&bytes).context("manifest zip 解压失败")?;
                                            let mut manifest = DepotManifest::parse(&plain)?;
                                            if manifest.filenames_encrypted {
                                                manifest.decrypt_filenames(depot_key)?;
                                            }
                                            return Ok(manifest);
                                        }
                                        Err(e2) => last_err = e2,
                                    }
                                }
                            }
                            Err(e2) => last_err = e2,
                        }
                    } else {
                        last_err = e;
                    }
                    self.note_failure(&server);
                    let _ = attempt;
                }
            }
        }
        Err(last_err.context("下载 manifest 失败"))
    }

    /// Downloads a single depot chunk: decrypts, decompresses and verifies it.
    /// Returns the uncompressed chunk bytes.
    pub async fn download_chunk(
        &self,
        app_id: u32,
        depot_id: u32,
        chunk: &ChunkData,
        depot_key: &[u8],
    ) -> anyhow::Result<Vec<u8>> {
        let chunk_id = chunk.id_hex();
        let mut last_err = anyhow!("no server");
        let attempts = 8usize;

        for _ in 0..attempts {
            let server = self.pick_server().context("CDN 服务器列表为空")?;
            let mut url = format!(
                "{}/depot/{}/chunk/{}",
                server.base_url(),
                depot_id,
                chunk_id
            );

            let mut authed = false;
            let mut try_again_same_server = false;
            match self.get_bytes(&server, &url, depot_id, app_id, &mut authed, 90).await {
                Ok(bytes) => match process_chunk(chunk, &bytes, depot_key) {
                    Ok(data) => {
                        self.note_success(&server);
                        return Ok(data);
                    }
                    Err(e) => {
                        last_err = e.context(format!("chunk {} 内容处理失败", chunk_id));
                    }
                },
                Err(e) => {
                    if is_auth_error(&e) && !authed {
                        match self.cdn_auth_token(app_id, depot_id, &server.host).await {
                            Ok(token) if !token.is_empty() => {
                                url = append_token(&url, &token);
                                try_again_same_server = true;
                            }
                            _ => {}
                        }
                    }
                    last_err = e;
                }
            }

            if try_again_same_server {
                match self.get_bytes(&server, &url, depot_id, app_id, &mut authed, 90).await {
                    Ok(bytes) => match process_chunk(chunk, &bytes, depot_key) {
                        Ok(data) => {
                            self.note_success(&server);
                            return Ok(data);
                        }
                        Err(e) => {
                            last_err = e.context(format!("chunk {} 内容处理失败", chunk_id));
                        }
                    },
                    Err(e) => {
                        last_err = e;
                    }
                }
            }

            self.note_failure(&server);
        }

        Err(last_err.context(format!("下载 chunk {} 失败", chunk_id)))
    }

    /// Raw GET with auth-error classification.
    async fn get_bytes(
        &self,
        server: &CdnServer,
        url: &str,
        _depot_id: u32,
        _app_id: u32,
        _authed: &mut bool,
        timeout_secs: u64,
    ) -> anyhow::Result<Vec<u8>> {
        let started = Instant::now();
        let resp = self
            .http
            .get(url)
            .timeout(Duration::from_secs(timeout_secs))
            .send()
            .await
            .with_context(|| format!("请求 {} 失败", server.host))?;

        let status = resp.status();
        if status == reqwest::StatusCode::FORBIDDEN || status == reqwest::StatusCode::UNAUTHORIZED {
            return Err(CdnError::Auth(status.as_u16()).into());
        }
        if status == reqwest::StatusCode::NOT_FOUND {
            return Err(CdnError::NotFound.into());
        }
        if !status.is_success() {
            return Err(CdnError::Http(status.as_u16()).into());
        }

        let bytes = resp
            .bytes()
            .await
            .with_context(|| format!("读取 {} 响应失败 ({}ms)", server.host, started.elapsed().as_millis()))?;
        Ok(bytes.to_vec())
    }

    /// Current number of known servers.
    pub fn server_count(&self) -> usize {
        self.servers.read().len()
    }

    /// Debug helper: prints all known servers.
    pub fn debug_print_servers(&self) {
        for s in self.servers.read().iter() {
            println!(
                "    server {:<40} vhost {:<40} type {:<10} https {} load {:.1} entries {} proxy {}",
                s.host, s.vhost, s.server_type, s.https, s.weighted_load, s.num_entries, s.use_as_proxy
            );
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CdnError {
    #[error("CDN 鉴权失败 (HTTP {0})")]
    Auth(u16),
    #[error("CDN 内容不存在 (404)")]
    NotFound,
    #[error("CDN HTTP 错误 {0}")]
    Http(u16),
}

fn is_auth_error(e: &anyhow::Error) -> bool {
    e.downcast_ref::<CdnError>()
        .map(|c| matches!(c, CdnError::Auth(_)))
        .unwrap_or(false)
}

/// Appends a CDN auth token to a URL. Steam returns tokens that already
/// start with `?` (e.g. `?auth_key=...`); handle bare forms defensively.
fn append_token(url: &str, token: &str) -> String {
    if token.starts_with('?') {
        format!("{}{}", url, token)
    } else if url.contains('?') {
        format!("{}&{}", url, token)
    } else {
        format!("{}?{}", url, token)
    }
}

/// Decrypts, decompresses and verifies a downloaded chunk payload.
pub fn process_chunk(chunk: &ChunkData, data: &[u8], depot_key: &[u8]) -> anyhow::Result<Vec<u8>> {
    if data.len() < 16 || data.len() % 16 != 0 {
        bail!("chunk 数据长度无效: {}", data.len());
    }
    let iv = crypto::aes256_ecb_decrypt_block(depot_key, data[..16].try_into().unwrap());
    let decrypted = crypto::aes256_cbc_decrypt(depot_key, &iv, &data[16..])?;
    if decrypted.len() < 4 {
        bail!("chunk 解密后数据过短");
    }

    let out = if decrypted[0..4] == *b"VSZa" {
        vzstd_decompress(&decrypted)?
    } else if decrypted[0..3] == *b"VZa" {
        vzip_decompress(&decrypted)?
    } else if decrypted[0..4] == [0x50, 0x4B, 0x03, 0x04] {
        zip_decompress(&decrypted)?
    } else {
        bail!("未知的 chunk 压缩格式: {:02x?}", &decrypted[0..4]);
    };

    if out.len() != chunk.uncompressed_len as usize {
        bail!(
            "chunk 解压长度不符: 期望 {} 实际 {}",
            chunk.uncompressed_len,
            out.len()
        );
    }
    let crc = crypto::adler32(&out);
    if crc != chunk.checksum {
        bail!(
            "chunk adler32 校验失败: 期望 {:08x} 实际 {:08x}",
            chunk.checksum,
            crc
        );
    }
    Ok(out)
}

/// vZstd: magic(4) + crc(4) + zstd frame + footer(crc(4) + size(4) + "zsv"(3) + ?(2))
fn vzstd_decompress(data: &[u8]) -> anyhow::Result<Vec<u8>> {
    if data.len() < 8 + 15 {
        bail!("vZstd 数据过短");
    }
    if u32::from_le_bytes(data[0..4].try_into().unwrap()) != 0x615A5356 {
        bail!("vZstd magic 不匹配");
    }
    if &data[data.len() - 3..] != b"zsv" {
        bail!("vZstd footer 不匹配");
    }
    let size = u32::from_le_bytes(data[data.len() - 11..data.len() - 7].try_into().unwrap()) as usize;
    let frame = &data[8..data.len() - 15];
    let out = zstd::stream::decode_all(frame).context("zstd 解压失败")?;
    if out.len() != size {
        bail!("vZstd 解压长度不符");
    }
    Ok(out)
}

/// VZip (LZMA): 'VZ' + 'a' + u32 + lzma-props(5) + stream + crc(4) + size(4) + 'zv'
fn vzip_decompress(data: &[u8]) -> anyhow::Result<Vec<u8>> {
    if data.len() < 12 + 10 {
        bail!("VZip 数据过短");
    }
    if u16::from_le_bytes(data[0..2].try_into().unwrap()) != 0x5A56 || data[2] != b'a' {
        bail!("VZip magic 不匹配");
    }
    if u16::from_le_bytes(data[data.len() - 2..].try_into().unwrap()) != 0x767A {
        bail!("VZip footer 不匹配");
    }
    let size = u32::from_le_bytes(data[data.len() - 6..data.len() - 2].try_into().unwrap()) as usize;
    let props = &data[7..12];
    let stream = &data[12..data.len() - 10];

    // Re-pack into the .lzma alone format: props(5) + uncompressed size(8 LE) + stream
    let mut packed = Vec::with_capacity(13 + stream.len());
    packed.extend_from_slice(props);
    packed.extend_from_slice(&(size as u64).to_le_bytes());
    packed.extend_from_slice(stream);

    let mut out = Vec::with_capacity(size);
    let mut cursor = Cursor::new(&packed);
    lzma_rs::lzma_decompress(&mut cursor, &mut out).context("LZMA 解压失败")?;
    if out.len() != size {
        bail!("VZip 解压长度不符: 期望 {} 实际 {}", size, out.len());
    }
    Ok(out)
}

/// Plain ZIP with a single entry (used for newer chunk payloads & manifests).
fn zip_decompress(data: &[u8]) -> anyhow::Result<Vec<u8>> {
    let mut archive = zip::ZipArchive::new(Cursor::new(data)).context("zip 打开失败")?;
    if archive.len() != 1 {
        bail!("zip 条目数不为 1");
    }
    let mut entry = archive.by_index(0).context("zip 条目读取失败")?;
    let mut out = Vec::with_capacity(entry.size() as usize);
    entry.read_to_end(&mut out).context("zip 解压失败")?;
    Ok(out)
}

/// Manifest payload ZIP (single entry).
pub fn unzip_single(data: &[u8]) -> anyhow::Result<Vec<u8>> {
    zip_decompress(data)
}

/// Requests the depot decryption key via the legacy CM message.
pub async fn get_depot_key(
    conn: &Arc<CmConnection>,
    app_id: u32,
    depot_id: u32,
) -> anyhow::Result<Vec<u8>> {
    let req = CMsgClientGetDepotDecryptionKey {
        depot_id: Some(depot_id),
        app_id: Some(app_id),
    };
    let pkt = conn
        .call(emsg::CLIENT_GET_DEPOT_DECRYPTION_KEY, &req, Duration::from_secs(20))
        .await?;
    let resp: CMsgClientGetDepotDecryptionKeyResponse = pkt.decode_body()?;
    let result = resp.eresult.unwrap_or(eresult::FAIL);
    if result != eresult::OK {
        bail!(
            "获取 depot {} 解密密钥失败: {} ({})",
            depot_id,
            eresult::describe(result),
            result
        );
    }
    resp.depot_encryption_key
        .filter(|k| k.len() == 32)
        .context("depot 密钥缺失或长度错误")
}

/// Writes bytes into a Vec sink (used by lzma_rs).
pub struct VecSink<'a>(pub &'a mut Vec<u8>);
impl<'a> Write for VecSink<'a> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
