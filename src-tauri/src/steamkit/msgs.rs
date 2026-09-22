//! Steam message framing: EMsg constants, EResult, SteamID helpers,
//! MsgHdrProtoBuf encode/decode.

use bytes::{Buf, BufMut, BytesMut};
use prost::Message;
use super::proto_gen::CMsgProtoBufHeader;

pub const PROTO_MASK: u32 = 0x8000_0000;
pub const EMSG_MASK: u32 = 0x3FFF_FFFF;

/// Job ID meaning "no job".
pub const JOBID_NONE: u64 = u64::MAX;

// ---- EMsg values we care about ----------------------------------------
pub mod emsg {
    pub const MULTI: u32 = 1;
    pub const SERVICE_METHOD_RESPONSE: u32 = 147;
    pub const SERVICE_METHOD_CALL_FROM_CLIENT: u32 = 151;
    pub const SERVICE_METHOD_CALL_FROM_CLIENT_NONAUTHED: u32 = 9804;

    pub const CLIENT_HEART_BEAT: u32 = 703;
    pub const CLIENT_LOG_OFF: u32 = 706;
    pub const CLIENT_LOG_ON_RESPONSE: u32 = 751;
    pub const CLIENT_LOGGED_OFF: u32 = 757;
    pub const CLIENT_LICENSE_LIST: u32 = 780;

    pub const CLIENT_GET_DEPOT_DECRYPTION_KEY: u32 = 5438;
    pub const CLIENT_GET_DEPOT_DECRYPTION_KEY_RESPONSE: u32 = 5439;
    pub const CLIENT_LOGON: u32 = 5514;
    pub const CLIENT_REQUEST_FREE_LICENSE: u32 = 5572;
    pub const CLIENT_REQUEST_FREE_LICENSE_RESPONSE: u32 = 5573;

    pub const CLIENT_PICS_PRODUCT_INFO_REQUEST: u32 = 8903;
    pub const CLIENT_PICS_PRODUCT_INFO_RESPONSE: u32 = 8904;
    pub const CLIENT_PICS_ACCESS_TOKEN_REQUEST: u32 = 8905;
    pub const CLIENT_PICS_ACCESS_TOKEN_RESPONSE: u32 = 8906;

    pub const CLIENT_SERVER_UNAVAILABLE: u32 = 5500;
    pub const CLIENT_HELLO: u32 = 9805;
}

/// Protocol version sent in ClientHello / ClientLogon (from SteamKit).
pub const CURRENT_PROTOCOL: u32 = 65581;
/// Client package version sent in ClientLogon.
pub const CLIENT_PACKAGE_VERSION: u32 = 1771;

// ---- EResult ------------------------------------------------------------
pub mod eresult {
    pub const OK: i32 = 1;
    pub const FAIL: i32 = 2;
    pub const NO_CONNECTION: i32 = 3;
    pub const INVALID_PASSWORD: i32 = 5;
    pub const INVALID_PARAM: i32 = 8;
    pub const FILE_NOT_FOUND: i32 = 9;
    pub const ACCESS_DENIED: i32 = 15;
    pub const ACCOUNT_NOT_FOUND: i32 = 18;
    pub const SERVICE_UNAVAILABLE: i32 = 20;
    pub const PENDING: i32 = 22;
    pub const REVOKED: i32 = 26;
    pub const EXPIRED: i32 = 27;
    pub const BLOCKED: i32 = 40;
    pub const ACCOUNT_LOCKED: i32 = 44;
    pub const TRY_ANOTHER_CM: i32 = 48;
    pub const ACCOUNT_LOGON_DENIED: i32 = 63;
    pub const INVALID_LOGIN_AUTH_CODE: i32 = 65;
    pub const RATE_LIMIT_EXCEEDED: i32 = 84;
    pub const ACCOUNT_LOGIN_DENIED_NEED_TWO_FACTOR: i32 = 85;
    pub const TWO_FACTOR_CODE_MISMATCH: i32 = 88;
    pub const LIMITED_USER_ACCOUNT: i32 = 112;

    pub fn describe(code: i32) -> &'static str {
        match code {
            OK => "OK",
            FAIL => "Fail",
            NO_CONNECTION => "NoConnection",
            INVALID_PASSWORD => "InvalidPassword",
            INVALID_PARAM => "InvalidParam",
            FILE_NOT_FOUND => "FileNotFound",
            ACCESS_DENIED => "AccessDenied",
            ACCOUNT_NOT_FOUND => "AccountNotFound",
            SERVICE_UNAVAILABLE => "ServiceUnavailable",
            PENDING => "Pending",
            REVOKED => "Revoked",
            EXPIRED => "Expired",
            BLOCKED => "Blocked",
            ACCOUNT_LOCKED => "AccountLocked",
            TRY_ANOTHER_CM => "TryAnotherCM",
            ACCOUNT_LOGON_DENIED => "AccountLogonDenied",
            INVALID_LOGIN_AUTH_CODE => "InvalidLoginAuthCode",
            RATE_LIMIT_EXCEEDED => "RateLimitExceeded",
            ACCOUNT_LOGIN_DENIED_NEED_TWO_FACTOR => "AccountLoginDeniedNeedTwoFactor",
            TWO_FACTOR_CODE_MISMATCH => "TwoFactorCodeMismatch",
            LIMITED_USER_ACCOUNT => "LimitedUserAccount",
            _ => "Unknown",
        }
    }
}

// ---- SteamID ------------------------------------------------------------
pub mod steamid {
    pub const UNIVERSE_PUBLIC: u64 = 1;
    pub const TYPE_INDIVIDUAL: u64 = 1;
    pub const TYPE_ANON_USER: u64 = 10;
    pub const INSTANCE_DESKTOP: u64 = 1;

    pub fn make(universe: u64, account_type: u64, instance: u64, account_id: u32) -> u64 {
        (universe << 56) | (account_type << 52) | (instance << 32) | account_id as u64
    }

    pub fn anonymous() -> u64 {
        make(UNIVERSE_PUBLIC, TYPE_ANON_USER, 0, 0)
    }

    pub fn individual(account_id: u32) -> u64 {
        make(UNIVERSE_PUBLIC, TYPE_INDIVIDUAL, INSTANCE_DESKTOP, account_id)
    }

    pub fn account_id(steamid: u64) -> u32 {
        (steamid & 0xFFFF_FFFF) as u32
    }

    /// Renders "STEAM_1:0:1234" style ID32 text.
    pub fn to_id32(steamid: u64) -> String {
        let account = account_id(steamid);
        format!("{}:{}", account & 1, account >> 1)
    }
}

// ---- Packet framing ------------------------------------------------------

/// A fully-parsed incoming packet.
pub struct PacketMsg {
    pub emsg: u32,
    pub header: CMsgProtoBufHeader,
    /// Remaining body bytes after the protobuf header.
    pub body: Vec<u8>,
}

impl PacketMsg {
    pub fn decode_body<M: Message + Default>(&self) -> Result<M, prost::DecodeError> {
        M::decode(&self.body[..])
    }

    pub fn jobid_target(&self) -> u64 {
        self.header.jobid_target.unwrap_or(JOBID_NONE)
    }

    pub fn jobid_source(&self) -> u64 {
        self.header.jobid_source.unwrap_or(JOBID_NONE)
    }
}

/// Parses a raw WS binary frame into a PacketMsg.
/// Over WebSocket all traffic is plaintext MsgHdrProtoBuf-framed protobuf.
pub fn parse_packet(data: &[u8]) -> anyhow::Result<Option<PacketMsg>> {
    if data.len() < 8 {
        anyhow::bail!("packet too small: {} bytes", data.len());
    }
    let mut cur = &data[..];
    let raw_emsg = cur.get_u32_le();
    let emsg = raw_emsg & EMSG_MASK;

    if raw_emsg & PROTO_MASK == 0 {
        // Non-proto messages are not expected over websockets; skip them.
        return Ok(None);
    }

    let header_len = cur.get_i32_le() as usize;
    if cur.len() < header_len {
        anyhow::bail!("packet truncated: header {} > remaining {}", header_len, cur.len());
    }
    let header = CMsgProtoBufHeader::decode(&cur[..header_len])?;
    let body = cur[header_len..].to_vec();

    Ok(Some(PacketMsg { emsg, header, body }))
}

/// Serializes a protobuf body into a full WS frame with the given header.
pub fn encode_packet<M: Message>(emsg: u32, header: &CMsgProtoBufHeader, body: &M) -> Vec<u8> {
    let hdr_bytes = header.encode_to_vec();
    let body_bytes = body.encode_to_vec();

    let mut out = BytesMut::with_capacity(8 + hdr_bytes.len() + body_bytes.len());
    out.put_u32_le(emsg | PROTO_MASK);
    out.put_i32_le(hdr_bytes.len() as i32);
    out.extend_from_slice(&hdr_bytes);
    out.extend_from_slice(&body_bytes);
    out.to_vec()
}

/// Builds a header with the current session fields pre-filled.
pub fn make_header(
    steamid: u64,
    session_id: i32,
    source_job: u64,
    target_job: u64,
    target_job_name: Option<String>,
) -> CMsgProtoBufHeader {
    CMsgProtoBufHeader {
        steamid: if steamid == 0 { None } else { Some(steamid) },
        client_sessionid: if session_id == 0 { None } else { Some(session_id) },
        routing_appid: None,
        jobid_source: Some(source_job),
        jobid_target: Some(target_job),
        target_job_name,
        seq_num: None,
        eresult: None,
        error_message: None,
        auth_account_flags: None,
        token_source: None,
        admin_spoofing_user: None,
        transport_error: None,
        messageid: None,
        publisher_group_id: None,
        sysid: None,
        webapi_key_id: None,
        is_from_external_source: None,
        forward_to_sysid: vec![],
        cm_sysid: None,
        launcher_type: None,
        realm: None,
        timeout_ms: None,
        debug_source: None,
        debug_source_string_index: None,
        token_id: None,
        session_disposition: None,
        wg_token: None,
        webui_auth_key: None,
        exclude_client_sessionids: vec![],
        admin_request_spoofing_steamid: None,
        is_valveds: None,
        trace_tag: None,
        ip: None,
        ip_v6: None,
    }
}
