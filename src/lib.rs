mod auth;
mod chat;
mod client;
mod config;
mod errors;
mod models;
mod proto;
mod usage;

use agent2api_wasm_sdk::{self as sdk, json, Error, Result, Value};

const PROVIDER: &str = "halo-cursor";

sdk::export_provider!(handle);

fn login_methods() -> Value {
    json!({"methods": [
      {
        "methodIndex": 0,
        "type": "oauth",
        "label": "Cursor 浏览器授权",
        "browserMode": "external",
        "prompts": []
      },
      {
        "methodIndex": 1,
        "type": "api",
        "label": "导入 Cursor API Key",
        "prompts": [{
          "key": "apiKey",
          "label": "Cursor API Key（Integrations）",
          "type": "text",
          "secret": true
        }]
      }
    ]})
}

fn metadata() -> Value {
    json!({
        "id": PROVIDER,
        "label": "Halo Cursor",
        "order": 47,
        "capabilities": {
            "chat": true,
            "stateful": true,
            "supports_default_model": false,
            "web_login": true,
            "refresh": true,
            "refresh_uses_account": true,
            "usage": true,
            "model_refresh": true,
            "images": false
        },
        "account_features": {
            "usage": true,
            "credit_detail": true,
            "credit_expiry": true,
            "identifier": ["accountId", "email"],
            "expiry": ["expiresAt", "tokenExpiresAt"]
        },
        "default_models": models::fallback(),
        "settings_schema": config::schema(),
        "settings_group": { "label": "Halo Cursor" }
    })
}

fn public(secret: &Value) -> Value {
    let email = secret["email"]
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let name = email.unwrap_or("Halo Cursor");
    let mut view = json!({
        "name": name,
        "accountId": auth::public_account_id(secret),
        "plan": secret["plan"],
        "expiresAt": secret["expiresAt"],
        "tokenExpiresAt": secret["expiresAt"]
    });
    if let Some(email) = email {
        view["email"] = json!(email);
        view["loginName"] = json!(email);
    }
    view
}

fn admin(params: &Value) -> Result<Value> {
    match params["action"].as_str().unwrap_or("") {
        "auth.methods" => Ok(login_methods()),
        // Outbound UA is per request (x-cursor-client-*), not account-wide.
        "network.user-agent" => Ok(json!({ "accountWide": false })),
        _ => Err(Error::unsupported()),
    }
}

fn normalized(secret: Value) -> Value {
    json!({
        "identity": secret["identity"],
        "public": public(&secret),
        "secret": secret,
        "desktop": false
    })
}

fn normalize(params: &Value) -> Result<Value> {
    let source = &params["source"];
    let payload = &source["payload"];
    let api_key = payload
        .pointer("/inputs/apiKey")
        .or_else(|| payload.get("apiKey"))
        .or_else(|| payload.get("accessToken"))
        .or_else(|| payload.get("token"))
        .and_then(Value::as_str)
        .ok_or_else(|| Error::http(400, "需要 Cursor API Key"))?;
    let secret = auth::normalize_api_key(api_key)?;
    Ok(normalized(secret))
}

fn handle(method: &str, mut params: Value) -> Result<Value> {
    match method {
        "init" => Ok(json!([metadata()])),
        "ping" => Ok(json!(true)),
        "provider.info" => Ok(metadata()),
        "account.normalize" => normalize(&params),
        "account.view" => Ok(public(&params["secret"])),
        "account.identity" => Ok(params["secret"]["identity"].clone()),
        "models.list" => models::list(&params),
        "models.refresh" => models::refresh(&params),
        "admin.action" => admin(&params),
        "usage.query" => usage::query(),
        "conversation" => {
            let body =
                sdk::take_body(&mut params).ok_or_else(|| Error::http(400, "请求缺少正文"))?;
            chat::forward(body)?;
            Ok(Value::Null)
        }
        "token.ensure" => Ok(auth::credentials(false)?["accessToken"].clone()),
        "token.refresh" => Ok(auth::credentials(true)?["accessToken"].clone()),
        "token.expiring" => Ok(json!(auth::expiring(&params)?)),
        "login.start" => auth::start(&params),
        "login.poll" => auth::poll(),
        "login.cancel" => auth::cancel(),
        "reasoning_patch" => Ok(json!({"kind":"none"})),
        "classify" => Ok(errors::classify(
            params["status"]
                .as_u64()
                .and_then(|value| u16::try_from(value).ok())
                .unwrap_or(502),
            &params["body"],
        )),
        "error.policy" => Ok(errors::policy(
            params["status"]
                .as_u64()
                .and_then(|value| u16::try_from(value).ok())
                .unwrap_or(502),
            &params["body"],
        )),
        "retry.decision" => Ok(errors::retry(&params)),
        "retry_advice" => Ok(errors::advice(&params)),
        _ => Err(Error::unsupported()),
    }
}
