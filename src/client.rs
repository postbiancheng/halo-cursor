//! Cursor API helpers: headers, JSON Connect unary, token expiry.

use super::config;
use agent2api_wasm_sdk::{self as sdk, http, json, Error, Result, Value};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use sha2::{Digest, Sha256};

pub const API: &str = "https://api2.cursor.sh";
pub const WEBSITE: &str = "https://cursor.com";
pub const OAUTH_FALLBACK_TTL_MS: i64 = 60 * 24 * 3_600_000;
const API_KEY_FALLBACK_TTL_MS: i64 = 30 * 24 * 3_600_000;
#[allow(dead_code)]
pub const AGENT_FALLBACK: &str = "https://agentn.global.api5.cursor.sh";

pub fn random_bytes(len: usize) -> Result<Vec<u8>> {
    let raw = sdk::host("random.bytes", json!({ "length": len }))?;
    let encoded = raw
        .as_str()
        .ok_or_else(|| Error::http(502, "随机数宿主接口返回无效"))?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|_| Error::http(502, "随机数解码失败"))?;
    if bytes.len() != len {
        return Err(Error::http(502, "随机数宿主接口返回长度无效"));
    }
    Ok(bytes)
}

pub fn format_uuid(bytes: &[u8]) -> String {
    let h = hex::encode(bytes);
    format!(
        "{}-{}-{}-{}-{}",
        &h[0..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..32]
    )
}

pub fn uuid_v4() -> Result<String> {
    let mut bytes = random_bytes(16)?;
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Ok(format_uuid(&bytes))
}

pub fn base64url(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

pub fn sha256_b64url(input: &[u8]) -> String {
    base64url(&Sha256::digest(input))
}

pub fn jwt_exp_ms(token: &str) -> Option<i64> {
    let payload = token.split('.').nth(1)?;
    let decoded = URL_SAFE_NO_PAD.decode(payload).ok()?;
    let claims: Value = serde_json::from_slice(&decoded).ok()?;
    let exp = claims["exp"].as_i64()?;
    Some(exp.saturating_mul(1000))
}

pub fn headers(token: Option<&str>) -> Result<Value> {
    let version = config::client_version()?;
    let mut map = json!({
        "Content-Type": "application/json",
        "Connect-Protocol-Version": "1",
        "x-cursor-client-version": version,
        "x-cursor-client-type": "cli",
        "x-ghost-mode": "true",
        "Accept": "application/json",
    });
    if let Some(tok) = token.filter(|t| !t.is_empty()) {
        map["Authorization"] = json!(format!("Bearer {tok}"));
    }
    if let Some(team) = config::team_id()? {
        map["x-cursor-team-id"] = json!(team);
    }
    Ok(map)
}

pub fn unary(path: &str, token: &str, body: Value) -> Result<Value> {
    unary_timeout(path, token, body, 15_000)
}

pub fn unary_timeout(path: &str, token: &str, body: Value, timeout_ms: u64) -> Result<Value> {
    let url = format!("{API}/{path}");
    let response =
        http::open_timeout("POST", &url, headers(Some(token))?, Some(&body), timeout_ms)?;
    let status = response.status;
    if !(200..300).contains(&status) {
        let text = String::from_utf8_lossy(&response.bytes(1 << 20)?).into_owned();
        return Err(Error::http(
            status,
            format!(
                "Cursor {path} 失败：{}",
                text.chars().take(240).collect::<String>()
            ),
        ));
    }
    response.json()
}

pub fn get_me(token: &str) -> Result<Value> {
    unary("aiserver.v1.DashboardService/GetMe", token, json!({}))
}

pub fn get_plan(token: &str) -> Option<Value> {
    unary("aiserver.v1.DashboardService/GetPlanInfo", token, json!({})).ok()
}

/// Soft-fail unary: None on transport/HTTP/parse errors (usage extras).
pub fn unary_optional(path: &str, token: &str, body: Value) -> Option<Value> {
    unary(path, token, body).ok()
}

fn encode_uri_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'-'
            | b'_'
            | b'.'
            | b'!'
            | b'~'
            | b'*'
            | b'\''
            | b'('
            | b')' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// WorkOS dashboard user id from a CLI JWT (`sub` last `|` segment).
pub fn session_user_id(token: &str) -> Option<String> {
    let payload = token.split('.').nth(1)?;
    let decoded = URL_SAFE_NO_PAD.decode(payload).ok()?;
    let claims: Value = serde_json::from_slice(&decoded).ok()?;
    let sub = claims.get("sub")?.as_str()?;
    let user = sub.split('|').rfind(|s| !s.is_empty())?;
    if user.is_empty()
        || !user
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    {
        return None;
    }
    Some(user.to_string())
}

/// Grok Bot / Sand allowance (website dashboard cookie session). Soft-fail.
pub fn sand_usage_status(token: &str) -> Option<Value> {
    let user = session_user_id(token)?;
    let cookie = format!(
        "WorkosCursorSessionToken={}",
        encode_uri_component(&format!("{user}::{token}"))
    );
    let url = format!("{WEBSITE}/api/dashboard/get-sand-usage-status");
    let response = http::open_timeout(
        "POST",
        &url,
        json!({
            "Content-Type": "application/json",
            "Accept": "application/json",
            "Origin": WEBSITE,
            "Cookie": cookie,
        }),
        Some(&json!({})),
        5_000,
    )
    .ok()?;
    if !(200..300).contains(&response.status) {
        return None;
    }
    response.json().ok()
}

#[allow(dead_code)]
pub fn agent_url(token: &str) -> String {
    match unary_timeout(
        "aiserver.v1.ServerConfigService/GetServerConfig",
        token,
        json!({}),
        10_000,
    ) {
        Ok(cfg) => {
            for key in ["/agentUrlConfig/agentUrl", "/agentUrlConfig/agentnUrl"] {
                if let Some(raw) = cfg.pointer(key).and_then(Value::as_str) {
                    let trimmed = raw.trim_end_matches('/');
                    if trimmed.starts_with("https://") || trimmed.starts_with("http://") {
                        return trimmed.to_string();
                    }
                }
            }
            AGENT_FALLBACK.into()
        }
        Err(_) => AGENT_FALLBACK.into(),
    }
}

pub fn exchange_api_key(api_key: &str) -> Result<(String, i64)> {
    let url = format!("{API}/auth/exchange_user_api_key");
    let version = config::client_version()?;
    let response = http::open_timeout(
        "POST",
        &url,
        json!({
            "Content-Type": "application/json",
            "Authorization": format!("Bearer {api_key}"),
            "x-cursor-client-version": version,
            "x-cursor-client-type": "cli",
            "Accept": "application/json",
        }),
        Some(&json!({})),
        15_000,
    )?;
    if !(200..300).contains(&response.status) {
        return Err(Error::http(
            if response.status >= 500 {
                502
            } else {
                response.status
            },
            "Cursor 未接受此 API Key",
        ));
    }
    let body = response.json()?;
    let access = body["accessToken"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| Error::http(401, "Cursor 未返回 access token"))?
        .to_string();
    let expires =
        jwt_exp_ms(&access).unwrap_or_else(|| sdk::now().saturating_add(API_KEY_FALLBACK_TTL_MS));
    Ok((access, expires))
}
