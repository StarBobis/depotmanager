//! SteamKit-style protocol layer, implemented from scratch in Rust.
//!
//! Talks to Steam CM servers over WebSocket (plaintext protobuf over TLS,
//! no channel-encryption handshake required), handles logon (anonymous /
//! account), PICS product info, unified service messages and CDN downloads.

pub mod proto_gen;
pub mod msgs;
pub mod crypto;
pub mod machineid;
pub mod kv;
pub mod connection;
pub mod session;
pub mod pics;
pub mod cdn;
pub mod manifest;
pub mod auth;

/// Shared reqwest client builder with sane defaults for Steam endpoints.
pub fn http_client() -> anyhow::Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36 DepotManager/0.1")
        .connect_timeout(std::time::Duration::from_secs(10))
        .pool_max_idle_per_host(16)
        .build()?)
}
