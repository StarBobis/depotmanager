//! Steam authentication service flows (QR code + credentials) used to
//! obtain refresh tokens for account logon.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context};

use super::connection::CmConnection;
use super::crypto;
use super::msgs::eresult;
use super::proto_gen::*;

pub mod guard_type {
    pub const NONE: i32 = 1;
    pub const EMAIL_CODE: i32 = 2;
    pub const DEVICE_CODE: i32 = 3;
    pub const DEVICE_CONFIRMATION: i32 = 4;
    pub const EMAIL_CONFIRMATION: i32 = 5;
    pub const MACHINE_TOKEN: i32 = 6;

    pub fn describe(code: i32) -> &'static str {
        match code {
            NONE => "无需验证",
            EMAIL_CODE => "邮箱验证码",
            DEVICE_CODE => "手机令牌验证码",
            DEVICE_CONFIRMATION => "手机端确认",
            EMAIL_CONFIRMATION => "邮箱确认",
            MACHINE_TOKEN => "机器令牌",
            _ => "未知验证",
        }
    }
}

const PLATFORM_STEAM_CLIENT: i32 = 1;
const PERSISTENCE_PERSISTENT: i32 = 1;

#[derive(Debug, Clone)]
pub struct AuthSessionState {
    pub client_id: u64,
    pub request_id: Vec<u8>,
    pub steamid: u64,
    pub interval: f32,
    pub allowed_confirmations: Vec<(i32, String)>,
    pub challenge_url: Option<String>,
    pub weak_token: Option<String>,
}

#[derive(Debug, Clone)]
pub struct AuthPollResult {
    pub account_name: String,
    pub refresh_token: String,
    pub access_token: String,
    pub new_guard_data: Option<String>,
}

pub struct AuthService {
    conn: Arc<CmConnection>,
    device_name: String,
}

impl AuthService {
    pub fn new(conn: Arc<CmConnection>) -> Self {
        AuthService {
            conn,
            device_name: format!("DepotManager ({})", whoami_hostname()),
        }
    }

    fn device_details(&self) -> CAuthenticationDeviceDetails {
        CAuthenticationDeviceDetails {
            device_friendly_name: Some(self.device_name.clone()),
            platform_type: Some(PLATFORM_STEAM_CLIENT),
            os_type: Some(20),
            gaming_device_type: None,
            client_count: None,
            machine_id: None,
            app_type: None,
        }
    }

    /// Starts a QR code auth session. The challenge URL should be rendered
    /// as a QR code for the Steam mobile app to scan.
    pub async fn begin_qr(&self) -> anyhow::Result<AuthSessionState> {
        let req = CAuthenticationBeginAuthSessionViaQrRequest {
            device_friendly_name: Some(self.device_name.clone()),
            platform_type: Some(PLATFORM_STEAM_CLIENT),
            device_details: Some(self.device_details()),
            website_id: Some("Client".into()),
        };
        let (result, pkt) = self
            .conn
            .unified_call("Authentication.BeginAuthSessionViaQR#1", &req, Duration::from_secs(20))
            .await?;
        if result != eresult::OK {
            bail!("创建二维码登录会话失败: {} ({})", eresult::describe(result), result);
        }
        let resp: CAuthenticationBeginAuthSessionViaQrResponse = pkt.decode_body()?;
        Ok(AuthSessionState {
            client_id: resp.client_id.unwrap_or(0),
            request_id: resp.request_id.unwrap_or_default(),
            steamid: 0,
            interval: resp.interval.unwrap_or(5.0).max(1.0),
            allowed_confirmations: resp
                .allowed_confirmations
                .into_iter()
                .map(|c| (c.confirmation_type.unwrap_or(0), c.associated_message.unwrap_or_default()))
                .collect(),
            challenge_url: resp.challenge_url.filter(|s| !s.is_empty()),
            weak_token: None,
        })
    }

    /// Starts a credentials (account name + password) auth session.
    pub async fn begin_credentials(
        &self,
        account_name: &str,
        password: &str,
        remember: bool,
        guard_data: Option<&str>,
    ) -> anyhow::Result<AuthSessionState> {
        // 1. fetch RSA public key for the account
        let key_req = CAuthenticationGetPasswordRsaPublicKeyRequest {
            account_name: Some(account_name.to_string()),
        };
        let (result, pkt) = self
            .conn
            .unified_call(
                "Authentication.GetPasswordRSAPublicKey#1",
                &key_req,
                Duration::from_secs(20),
            )
            .await?;
        if result != eresult::OK {
            bail!(
                "获取账号公钥失败: {} ({})（账号是否存在?）",
                eresult::describe(result),
                result
            );
        }
        let key_resp: CAuthenticationGetPasswordRsaPublicKeyResponse = pkt.decode_body()?;
        let modulus = key_resp.publickey_mod.context("服务器未返回公钥模数")?;
        let exponent = key_resp.publickey_exp.context("服务器未返回公钥指数")?;
        let timestamp = key_resp.timestamp.unwrap_or(0);

        let encrypted_password = crypto::rsa_encrypt_password(&modulus, &exponent, password)?;

        // 2. begin auth session
        let req = CAuthenticationBeginAuthSessionViaCredentialsRequest {
            device_friendly_name: Some(self.device_name.clone()),
            account_name: Some(account_name.to_string()),
            encrypted_password: Some(encrypted_password),
            encryption_timestamp: Some(timestamp),
            remember_login: Some(remember),
            platform_type: Some(PLATFORM_STEAM_CLIENT),
            persistence: Some(PERSISTENCE_PERSISTENT),
            website_id: Some("Client".into()),
            device_details: Some(self.device_details()),
            guard_data: guard_data.map(String::from),
            language: Some(6), // schinese? use 0? keep 6 = tchinese? actually 6 = german; use None
            qos_level: None,
        };
        // language: send nothing (Steam picks account language)
        let req = CAuthenticationBeginAuthSessionViaCredentialsRequest {
            language: None,
            ..req
        };

        let (result, pkt) = self
            .conn
            .unified_call(
                "Authentication.BeginAuthSessionViaCredentials#1",
                &req,
                Duration::from_secs(20),
            )
            .await?;
        let resp: CAuthenticationBeginAuthSessionViaCredentialsResponse = pkt.decode_body()?;
        if result != eresult::OK {
            let ext = resp.extended_error_message.unwrap_or_default();
            bail!(
                "登录失败: {} ({}) {}",
                eresult::describe(result),
                result,
                ext
            );
        }
        Ok(AuthSessionState {
            client_id: resp.client_id.unwrap_or(0),
            request_id: resp.request_id.unwrap_or_default(),
            steamid: resp.steamid.unwrap_or(0),
            interval: resp.interval.unwrap_or(5.0).max(1.0),
            allowed_confirmations: resp
                .allowed_confirmations
                .into_iter()
                .map(|c| (c.confirmation_type.unwrap_or(0), c.associated_message.unwrap_or_default()))
                .collect(),
            challenge_url: None,
            weak_token: resp.weak_token.filter(|s| !s.is_empty()),
        })
    }

    /// Submits a Steam Guard code (email code or device 2FA code).
    pub async fn submit_guard_code(
        &self,
        state: &AuthSessionState,
        code: &str,
        code_type: i32,
    ) -> anyhow::Result<()> {
        let req = CAuthenticationUpdateAuthSessionWithSteamGuardCodeRequest {
            client_id: Some(state.client_id),
            steamid: Some(state.steamid),
            code: Some(code.to_string()),
            code_type: Some(code_type),
        };
        let (result, _pkt) = self
            .conn
            .unified_call(
                "Authentication.UpdateAuthSessionWithSteamGuardCode#1",
                &req,
                Duration::from_secs(20),
            )
            .await?;
        if result != eresult::OK {
            bail!(
                "提交验证码失败: {} ({})",
                eresult::describe(result),
                result
            );
        }
        Ok(())
    }

    /// Polls once for auth session completion.
    pub async fn poll_once(
        &self,
        state: &AuthSessionState,
    ) -> anyhow::Result<Option<AuthPollResult>> {
        let req = CAuthenticationPollAuthSessionStatusRequest {
            client_id: Some(state.client_id),
            request_id: Some(state.request_id.clone()),
            token_to_revoke: None,
        };
        let (result, pkt) = self
            .conn
            .unified_call(
                "Authentication.PollAuthSessionStatus#1",
                &req,
                Duration::from_secs(20),
            )
            .await?;
        if result != eresult::OK {
            bail!(
                "轮询登录状态失败: {} ({})",
                eresult::describe(result),
                result
            );
        }
        let resp: CAuthenticationPollAuthSessionStatusResponse = pkt.decode_body()?;
        match (resp.refresh_token, resp.access_token) {
            (Some(refresh), Some(access)) if !refresh.is_empty() => Ok(Some(AuthPollResult {
                account_name: resp.account_name.unwrap_or_default(),
                refresh_token: refresh,
                access_token: access,
                new_guard_data: resp.new_guard_data.filter(|s| !s.is_empty()),
            })),
            _ => Ok(None),
        }
    }
}

fn whoami_hostname() -> String {
    hostname::get()
        .ok()
        .and_then(|h| h.into_string().ok())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "PC".into())
}

// minimal hostname shim (avoid extra dependency)
mod hostname {
    pub fn get() -> std::io::Result<std::ffi::OsString> {
        Ok(std::env::var_os("COMPUTERNAME")
            .or_else(|| std::env::var_os("HOSTNAME"))
            .unwrap_or_else(|| std::ffi::OsString::from("PC")))
    }
}
