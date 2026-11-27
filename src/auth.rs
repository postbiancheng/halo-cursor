//! Cursor login: Deep Control OAuth + API Key exchange (Magpie-aligned, no Keychain).

use super::client;
use agent2api_wasm_sdk::{
    self as sdk, http, json, login,
    state::{self, Scope},
    AccountLock, Error, Result, Value,
};

const KEY: &str = "cursor-deep-control";
const TTL: i64 = 900_000;
const LEAD: i64 = 300_000;
const POLL_INTERVAL: i64 = 2_000;
const POLL_TIMEOUT_MS: u64 = 15_000;
const POLL_LEASE_MS: i64 = POLL_TIMEOUT_MS as i64 + 5_000;

fn put(snapshot: &state::Snapshot, value: Value, ttl: i64) -> Result<()> {
    if state::compare_exchange(Scope::Login, KEY, 1, snapshot.revision, value, ttl)? {
        Ok(())
    } else {
        Err(Error::http(409, "Cursor 授权状态已变化，请重新发起"))
    }
}

pub(crate) fn anonymous_account_id(access: &str, api_key: Option<&str>) -> String {
    let subject = client::session_user_id(access);
    let (kind, seed) = if let Some(subject) = subject.as_deref() {
        ("subject", subject)
    } else if let Some(key) = api_key {
        ("api-key", key)
    } else {
        ("token", access)
    };
    let digest = client::sha256_b64url(format!("halo-cursor/account/{kind}\0{seed}").as_bytes());
    format!("cursor:{digest}")
}

pub(crate) fn public_account_id(secret: &Value) -> Value {
    if secret["email"]
        .as_str()
        .is_some_and(|email| !email.trim().is_empty())
    {
        return secret["accountId"].clone();
    }
    if let Some(identity) = secret.get("publicAccountId").and_then(Value::as_str) {
        return json!(identity);
    }
    match secret["accessToken"].as_str() {
        Some(access) => json!(anonymous_account_id(access, secret["apiKey"].as_str())),
        None => Value::Null,
    }
}

fn secret_from_access(
    access: &str,
    refresh: Option<&str>,
    api_key: Option<&str>,
    email: &str,
    plan: &str,
    expires_at: i64,
) -> Value {
    let identity = if email.is_empty() {
        anonymous_account_id(access, api_key)
    } else {
        format!("cursor:{email}")
    };
    let mut secret = json!({
        "accessToken": access,
        "accountId": if email.is_empty() { identity.clone() } else { email.to_string() },
        "email": email,
        "plan": plan,
        "expiresAt": expires_at,
        "identity": identity,
        "authKind": if api_key.is_some() { "api" } else { "oauth" },
    });
    if let Some(refresh) = refresh.filter(|s| !s.is_empty()) {
        secret["refreshToken"] = json!(refresh);
    }
    if let Some(key) = api_key.filter(|s| !s.is_empty()) {
        secret["apiKey"] = json!(key);
    }
    secret
}

fn enrich(
    access: &str,
    refresh: Option<&str>,
    api_key: Option<&str>,
    expires_at: i64,
) -> Result<Value> {
    let me = client::get_me(access).unwrap_or(Value::Null);
    let email = me["email"].as_str().unwrap_or("").trim().to_string();
    let plan = client::get_plan(access)
        .and_then(|p| {
            p.pointer("/planInfo/planName")
                .and_then(Value::as_str)
                .map(|s| s.trim().to_string())
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_default();
    let expires = if expires_at > 0 {
        expires_at
    } else {
        client::jwt_exp_ms(access)
            .unwrap_or_else(|| sdk::now().saturating_add(client::OAUTH_FALLBACK_TTL_MS))
    };
    Ok(secret_from_access(
        access, refresh, api_key, &email, &plan, expires,
    ))
}

pub fn start(params: &Value) -> Result<Value> {
    let flow = params["flow"]
        .as_str()
        .ok_or_else(|| Error::http(400, "缺少授权流程标识"))?;
    let before = state::get(Scope::Login, KEY, 1)?;
    if !before.value.is_null() {
        return Err(Error::http(409, "Cursor 授权流程已存在"));
    }
    let verifier_bytes = client::random_bytes(32)?;
    let verifier = client::base64url(&verifier_bytes);
    let challenge = client::sha256_b64url(verifier.as_bytes());
    let uuid = client::uuid_v4()?;
    let url = format!(
        "{}/loginDeepControl?challenge={}&uuid={}&mode=login&redirectTarget=cli",
        client::WEBSITE,
        urlencoding_minimal(&challenge),
        urlencoding_minimal(&uuid)
    );
    let now = sdk::now();
    put(
        &before,
        json!({
            "uuid": uuid,
            "verifier": verifier,
            "interval": POLL_INTERVAL,
            "nextAt": now + POLL_INTERVAL,
            "expiresAt": now + TTL,
            "errors": 0,
        }),
        TTL,
    )?;
    Ok(json!({
        "flow": flow,
        "url": url,
        "poll_interval_ms": POLL_INTERVAL,
        "instructions": "在浏览器中完成 Cursor 登录；完成后本页会自动结束。",
        "external_browser": true,
        "allowed_hosts": ["cursor.com", "www.cursor.com", "api2.cursor.sh", "authenticator.cursor.sh"],
        "login_ui": {
            "browserMode": "external",
            "manualCallback": false,
            "callbackInput": "url",
            "captureCallback": false
        }
    }))
}

fn urlencoding_minimal(value: &str) -> String {
    value
        .chars()
        .map(|c| match c {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~' => c.to_string(),
            _ => format!("%{:02X}", c as u8),
        })
        .collect()
}

pub fn poll() -> Result<Value> {
    let before = state::get(Scope::Login, KEY, 1)?;
    let now = sdk::now();
    let deadline = before.value["expiresAt"].as_i64().unwrap_or(0);
    if before.value.is_null() || deadline <= now {
        return Ok(json!({"state":"failed","message":"Cursor 登录已超时，请重试"}));
    }
    if before.value["busyUntil"].as_i64().unwrap_or(0) > now
        || before.value["nextAt"].as_i64().unwrap_or(0) > now
    {
        return Ok(login::pending(None));
    }
    let version = super::config::client_version()?;
    let now = sdk::now();
    if deadline <= now {
        return Ok(json!({"state":"failed","message":"Cursor 登录已超时，请重试"}));
    }
    let mut flow = before.value.clone();
    flow["busyUntil"] = json!(now.saturating_add(POLL_LEASE_MS).min(deadline));
    put(&before, flow.clone(), deadline - now)?;
    let reserved = state::get(Scope::Login, KEY, 1)?;
    if reserved.value != flow {
        return Err(Error::http(409, "Cursor 授权已取消"));
    }

    let uuid = flow["uuid"].as_str().unwrap_or_default();
    let verifier = flow["verifier"].as_str().unwrap_or_default();
    let url = format!(
        "{}/auth/poll?uuid={}&verifier={}",
        client::API,
        urlencoding_minimal(uuid),
        urlencoding_minimal(verifier)
    );
    let response = http::open_timeout(
        "GET",
        &url,
        json!({
            "Content-Type": "application/json",
            "x-cursor-client-version": version,
            "x-cursor-client-type": "cli",
            "Accept": "application/json",
        }),
        None,
        POLL_TIMEOUT_MS,
    );

    let (status, body) = match response {
        Ok(resp) => {
            let status = resp.status;
            let body = if (200..300).contains(&status) {
                resp.json().unwrap_or(Value::Null)
            } else {
                Value::Null
            };
            (status, body)
        }
        Err(_) => (0, Value::Null),
    };

    if status == 0 || status == 404 {
        let errors = if status == 0 {
            flow["errors"].as_i64().unwrap_or(0) + 1
        } else {
            0
        };
        if errors >= 3 {
            put(&reserved, Value::Null, 1000)?;
            return Ok(json!({"state":"failed","message":"无法连接 Cursor 授权服务"}));
        }
        flow["busyUntil"] = json!(0);
        flow["errors"] = json!(errors);
        flow["nextAt"] = json!(sdk::now() + POLL_INTERVAL);
        put(&reserved, flow, (deadline - sdk::now()).max(1000))?;
        return Ok(login::pending(None));
    }

    if status == 403 {
        put(&reserved, Value::Null, 1000)?;
        return Ok(json!({"state":"failed","message":"Cursor 拒绝了此次登录（403）"}));
    }

    if !(200..300).contains(&status) {
        let errors = flow["errors"].as_i64().unwrap_or(0) + 1;
        if errors >= 3 {
            put(&reserved, Value::Null, 1000)?;
            return Ok(
                json!({"state":"failed","message":format!("Cursor 授权失败：HTTP {status}")}),
            );
        }
        flow["busyUntil"] = json!(0);
        flow["errors"] = json!(errors);
        flow["nextAt"] = json!(sdk::now() + POLL_INTERVAL);
        put(&reserved, flow, (deadline - sdk::now()).max(1000))?;
        return Ok(login::pending(None));
    }

    put(&reserved, Value::Null, 1000)?;
    let access = body["accessToken"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| Error::http(502, "Cursor 未返回 access token"))?;
    if body.get("refreshToken").is_none() {
        return Err(Error::http(502, "Cursor 未返回 refresh token 字段"));
    }
    let refresh = body["refreshToken"].as_str();
    let expires = client::jwt_exp_ms(access)
        .unwrap_or_else(|| sdk::now().saturating_add(client::OAUTH_FALLBACK_TTL_MS));
    let secret = enrich(access, refresh, None, expires)?;
    Ok(login::done(super::normalized(secret)))
}

pub fn cancel() -> Result<Value> {
    let snapshot = state::get(Scope::Login, KEY, 1)?;
    put(&snapshot, Value::Null, 1000)?;
    Ok(Value::Null)
}

fn due(secret: &Value, lead: i64) -> bool {
    secret["expiresAt"].as_i64().unwrap_or(0) <= sdk::now().saturating_add(lead)
}

pub fn expiring(params: &Value) -> Result<bool> {
    let account = sdk::account()?;
    let secret = &account["secret"];
    Ok(secret["apiKey"].is_string() && due(secret, sdk::expiring_lead(params, LEAD)))
}

pub fn credentials(force: bool) -> Result<Value> {
    let mut initial = sdk::account()?;
    let secret = initial
        .as_object_mut()
        .and_then(|record| record.remove("secret"))
        .ok_or_else(|| Error::http(401, "Cursor 未登录"))?;

    if !force && !due(&secret, LEAD) {
        return Ok(secret);
    }

    // OAuth browser tokens cannot be refreshed; Magpie requires re-login.
    if secret["authKind"] != "api" || !secret["apiKey"].is_string() {
        if due(&secret, 0) {
            return Err(Error::http(401, "Cursor 登录已过期，请重新授权"));
        }
        return Ok(secret);
    }

    let _lock = AccountLock::acquire()?;
    let account = sdk::account()?;
    let current = &account["secret"];
    if current["accessToken"] != secret["accessToken"] || (!force && !due(current, LEAD)) {
        return Ok(current.clone());
    }
    let api_key = current["apiKey"]
        .as_str()
        .ok_or_else(|| Error::http(401, "请重新导入 Cursor API Key"))?;
    let (access, expires) = client::exchange_api_key(api_key)?;
    let old_subject = current["accessToken"]
        .as_str()
        .and_then(client::session_user_id);
    let new_subject = client::session_user_id(&access);
    if let (Some(old), Some(new)) = (old_subject, new_subject) {
        if old != new {
            return Err(Error::http(409, "Cursor 账号身份已变化"));
        }
    }
    if expires <= sdk::now() {
        return Err(Error::http(401, "Cursor 返回了已过期的凭据"));
    }
    let mut new = current.clone();
    new["publicAccountId"] = public_account_id(current);
    new["accessToken"] = json!(access);
    new["expiresAt"] = json!(expires);
    let saved = sdk::host(
        "account.update",
        json!({"expect": current, "secret": new, "public": super::public(&new)}),
    )?;
    if saved["updated"] == true {
        return Ok(new);
    }
    let latest = sdk::account()?;
    let replacement = &latest["secret"];
    if replacement["identity"] == current["identity"]
        && replacement["accessToken"] != current["accessToken"]
        && !due(replacement, 0)
    {
        return Ok(replacement.clone());
    }
    Err(Error::http(409, "保存 Cursor 续期凭据时账号已变化"))
}

pub fn normalize_api_key(api_key: &str) -> Result<Value> {
    let key = api_key.trim();
    if key.is_empty() {
        return Err(Error::http(400, "需要 Cursor API Key"));
    }
    let (access, expires) = client::exchange_api_key(key)?;
    enrich(&access, None, Some(key), expires)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anonymous_public_ids_hide_old_token_prefix_and_survive_api_key_exchange() {
        let legacy = json!({
            "accessToken": "test-old-bearer-token",
            "apiKey": "test-account-key",
            "accountId": "cursor:test-old-bearer-token"
        });
        let renewed = json!({"accessToken":"test-new-bearer-token", "apiKey":"test-account-key"});
        let public_id = public_account_id(&legacy);
        assert_eq!(public_id, public_account_id(&renewed));
        assert_ne!(public_id, legacy["accountId"]);
        assert!(!public_id.as_str().unwrap().contains("test-account-key"));
    }
}
