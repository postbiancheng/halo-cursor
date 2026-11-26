//! Cursor upstream error classification for the Agent2API host.

use agent2api_wasm_sdk::{self as sdk, json, Value};

fn message(body: &Value) -> String {
    [
        "/error/message",
        "/message",
        "/error",
        "/detail",
        "/debug/details/detail",
        "/debug/details/title",
    ]
    .iter()
    .find_map(|path| {
        body.pointer(path)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
    })
    .or_else(|| body.as_str().map(str::trim).filter(|text| !text.is_empty()))
    .unwrap_or("Cursor 上游未返回错误说明")
    .to_string()
}

fn code_text(body: &Value) -> String {
    [
        body.get("code"),
        body.pointer("/error/code"),
        body.pointer("/response/error/code"),
    ]
    .into_iter()
    .flatten()
    .find_map(|value| value.as_str().map(str::trim).filter(|text| !text.is_empty()))
    .unwrap_or("")
    .to_ascii_lowercase()
}

pub fn classify(status: u16, body: &Value) -> Value {
    let raw = message(body);
    let lower = raw.to_ascii_lowercase();
    let code = code_text(body);
    let failed = if (400..600).contains(&status) {
        status
    } else {
        502
    };

    if status == 401
        || matches!(
            code.as_str(),
            "unauthenticated" | "unauthorized" | "token_expired" | "authentication_error"
        )
        || lower.contains("sign in")
        || lower.contains("unauthorized")
        || lower.contains("unauthenticated")
        || lower.contains("access token")
            && (lower.contains("expir") || lower.contains("invalid"))
    {
        return json!({
            "kind": "token_expired",
            "status": 401,
            "message": format!("Cursor 登录已失效：{raw}"),
            "upstream_code": code,
        });
    }

    let quota = status == 429
        || matches!(
            code.as_str(),
            "resource_exhausted" | "rate_limit_exceeded" | "usage_limit" | "quota_exceeded"
        )
        || [
            "rate limit",
            "usage limit",
            "quota",
            "too many requests",
            "budget limit",
            "额度",
            "用量已用完",
        ]
        .iter()
        .any(|part| lower.contains(part));

    if quota {
        return json!({
            "kind": "quota_limited",
            "status": 429,
            "message": format!("Cursor 上游限制：{raw}"),
            "upstream_code": code,
            "reset_at": sdk::now().saturating_add(60_000),
        });
    }

    let blocked = [
        "content",
        "safety",
        "refused to serve",
        "policy",
        "blocked",
    ]
    .iter()
    .any(|part| lower.contains(part))
        && (lower.contains("refus")
            || lower.contains("block")
            || lower.contains("policy")
            || lower.contains("safety"));

    if blocked || code == "invalid_argument" && lower.contains("content") {
        return json!({
            "kind": "content_blocked",
            "status": failed,
            "message": format!("Cursor 内容策略拒绝：{raw}"),
            "upstream_code": code,
        });
    }

    json!({
        "kind": "fatal",
        "status": failed,
        "message": format!("Cursor 上游请求失败：{raw}"),
        "upstream_code": code,
    })
}

pub fn policy(status: u16, body: &Value) -> Value {
    let class = classify(status, body);
    let kind = class["kind"].as_str().unwrap_or("fatal");
    json!({
        "stop": matches!(kind, "token_expired" | "content_blocked"),
        "kind": kind,
    })
}

pub fn retry(params: &Value) -> Value {
    let attempt = params["attempt"].as_u64().unwrap_or(0);
    let budget = params["budget"].as_u64().unwrap_or(0);
    if budget > 0 && attempt >= budget {
        return json!({ "action": "stop", "reason": "已达到重试上限" });
    }
    let status = params["status"]
        .as_u64()
        .and_then(|value| u16::try_from(value).ok())
        .unwrap_or(502);
    let class = classify(status, &params["body"]);
    match class["kind"].as_str().unwrap_or("fatal") {
        "token_expired" | "content_blocked" => json!({
            "action": "stop",
            "reason": class["message"].as_str().unwrap_or("不可重试的上游错误"),
        }),
        "quota_limited" => json!({
            "action": "retry",
            "delay_ms": 10_000u64,
            "reason": "Cursor 限流或额度窗口，等待后重试",
        }),
        _ if matches!(status, 408 | 425 | 500 | 502 | 503 | 504) => json!({
            "action": "retry",
            "delay_ms": 2_000u64.saturating_mul(attempt.saturating_add(1).min(5)),
            "reason": "Cursor 上游暂时失败，短暂重试",
        }),
        _ => json!({ "action": "default" }),
    }
}

pub fn advice(params: &Value) -> Value {
    let decision = retry(params);
    if decision["action"] == "retry" {
        json!({
            "delay_ms": decision["delay_ms"],
            "reason": decision["reason"],
        })
    } else {
        Value::Null
    }
}
