//! Cursor upstream error classification for the Agent2API host.

use agent2api_wasm_sdk::{self as sdk, json, Value};

fn detail_from_connect(body: &Value) -> Option<String> {
    // Magpie: Connect errors often put the useful text under details[].debug.details.
    let details = body
        .pointer("/error/details")
        .or_else(|| body.get("details"))
        .and_then(Value::as_array)?;
    for row in details {
        let title = row
            .pointer("/debug/details/title")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("");
        let detail = row
            .pointer("/debug/details/detail")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("");
        let combined = format!("{title}: {detail}")
            .trim_matches(|c: char| c == ':' || c.is_whitespace())
            .to_string();
        if !combined.is_empty() {
            return Some(combined);
        }
        if let Some(text) = row
            .pointer("/debug/error")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            return Some(text.to_string());
        }
    }
    None
}

fn message(body: &Value) -> String {
    detail_from_connect(body)
        .or_else(|| {
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
                    .filter(|text| !text.is_empty() && !text.eq_ignore_ascii_case("error"))
                    .map(str::to_string)
            })
        })
        .or_else(|| {
            // Fall back to bare "Error" only when nothing better exists.
            [
                "/error/message",
                "/message",
            ]
            .iter()
            .find_map(|path| {
                body.pointer(path)
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|text| !text.is_empty())
                    .map(str::to_string)
            })
        })
        .or_else(|| body.as_str().map(str::trim).filter(|text| !text.is_empty()).map(str::to_string))
        .unwrap_or_else(|| "Cursor 上游未返回错误说明".into())
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

    // Cursor sometimes wraps client-version gates as resource_exhausted.
    if lower.contains("update required")
        || lower.contains("no longer supported")
        || lower.contains("please update")
    {
        return json!({
            "kind": "fatal",
            "status": failed,
            "message": format!("Cursor 要求更新客户端版本：{raw}"),
            "upstream_code": code,
        });
    }

    // Bare Connect `{code:resource_exhausted,message:Error}` is often a model /
    // capacity failure, not a spend-limit window — keep those as fatal.
    let generic_exhausted = code == "resource_exhausted"
        && (raw.eq_ignore_ascii_case("error") || raw.is_empty() || raw == "Error");
    let quota = !generic_exhausted
        && (status == 429
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
            .any(|part| lower.contains(part)));

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

    let detail = if code.is_empty() {
        raw
    } else {
        format!("{code}: {raw}")
    };
    json!({
        "kind": "fatal",
        "status": failed,
        "message": format!("Cursor 上游请求失败：{detail}"),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_auth_quota_and_content() {
        let auth = classify(
            401,
            &json!({"error":{"message":"Unauthorized","code":"unauthenticated"}}),
        );
        assert_eq!(auth["kind"], "token_expired");

        let quota = classify(
            429,
            &json!({"error":{"message":"Rate limit exceeded","code":"rate_limit_exceeded"}}),
        );
        assert_eq!(quota["kind"], "quota_limited");

        let opaque = classify(
            200,
            &json!({"error":{"message":"Error","code":"resource_exhausted"}}),
        );
        assert_eq!(opaque["kind"], "fatal");
        assert!(opaque["message"].as_str().unwrap().contains("resource_exhausted"));

        let stale = classify(
            200,
            &json!({
                "error": {
                    "code": "resource_exhausted",
                    "message": "Error",
                    "details": [{
                        "debug": {
                            "details": {
                                "title": "Update Required",
                                "detail": "Your version of Cursor is no longer supported."
                            }
                        }
                    }]
                }
            }),
        );
        assert_eq!(stale["kind"], "fatal");
        assert!(stale["message"].as_str().unwrap().contains("更新客户端"));

        let blocked = classify(
            400,
            &json!({"message":"Blocked by safety system","code":"invalid_argument"}),
        );
        assert_eq!(blocked["kind"], "content_blocked");
    }

    #[test]
    fn retry_stops_on_auth_and_waits_on_quota() {
        let stop = retry(&json!({
            "attempt": 0,
            "budget": 3,
            "status": 401,
            "body": {"message":"sign in required"}
        }));
        assert_eq!(stop["action"], "stop");

        let wait = retry(&json!({
            "attempt": 1,
            "budget": 3,
            "status": 429,
            "body": {"message":"usage limit"}
        }));
        assert_eq!(wait["action"], "retry");
        assert_eq!(wait["delay_ms"], 10_000);
    }
}
