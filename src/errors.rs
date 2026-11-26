//! Cursor upstream error classification for the Agent2API host.

use agent2api_wasm_sdk::{self as sdk, http, json, Error, Result, Value};

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
            ["/error/message", "/message"].iter().find_map(|path| {
                body.pointer(path)
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|text| !text.is_empty())
                    .map(str::to_string)
            })
        })
        .or_else(|| {
            body.as_str()
                .map(str::trim)
                .filter(|text| !text.is_empty())
                .map(str::to_string)
        })
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
    .find_map(|value| {
        value
            .as_str()
            .map(str::trim)
            .filter(|text| !text.is_empty())
    })
    .unwrap_or("")
    .to_ascii_lowercase()
}

fn retry_after(header: &str) -> Option<i64> {
    let header = header.trim();
    let now = sdk::now();
    header
        .parse::<i64>()
        .ok()
        .filter(|seconds| *seconds >= 0)
        .map(|seconds| now.saturating_add(seconds.saturating_mul(1_000)))
        .or_else(|| {
            chrono::DateTime::parse_from_rfc2822(header)
                .ok()
                .map(|date| date.timestamp_millis())
        })
        .filter(|time| *time >= now)
}

fn max_mode_required(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    (lower.contains("max mode") && lower.contains("required")) || lower.contains("请开启 max mode")
}

pub fn http_response(operation: &str, response: http::Response) -> Error {
    let status = response.status;
    let reset = response
        .header_values("Retry-After")
        .into_iter()
        .find_map(retry_after);
    let body = match response.bytes(256 << 10) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| json!({"message": String::from_utf8_lossy(&bytes)})),
        Err(error) => json!({ "message": error.message }),
    };
    upstream_error(operation, status, &body, reset)
}

pub fn connect_end(bytes: &[u8]) -> Result<()> {
    if bytes.is_empty() {
        return Ok(());
    }
    let body: Value = serde_json::from_slice(bytes)
        .map_err(|_| Error::http(502, "Cursor Connect 结束帧不是有效 JSON"))?;
    if !body.is_object() {
        return Err(Error::http(502, "Cursor Connect 结束帧格式无效"));
    }
    if body.get("error").is_some() || body.get("code").is_some() {
        let reset = body["metadata"]
            .as_object()
            .into_iter()
            .flat_map(|headers| headers.iter())
            .find(|(key, _)| key.eq_ignore_ascii_case("retry-after"))
            .and_then(|(_, values)| values.as_array())
            .and_then(|values| {
                values
                    .iter()
                    .filter_map(Value::as_str)
                    .find_map(retry_after)
            });
        return Err(upstream_error("RunSSE", 200, &body, reset));
    }
    Ok(())
}

fn upstream_error(operation: &str, status: u16, body: &Value, reset: Option<i64>) -> Error {
    let class = classify(status, body);
    let classified = class["status"].as_u64().unwrap_or(502) as u16;
    let detail: String = class["message"]
        .as_str()
        .unwrap_or("Cursor 请求失败")
        .chars()
        .take(512)
        .collect();
    let kind = class["kind"].as_str().unwrap_or("fatal");
    let code = if matches!(kind, "fatal" | "request_invalid" | "capacity")
        && max_mode_required(&message(body))
    {
        "cursor_max_mode_required".into()
    } else {
        format!("cursor_{kind}")
    };
    Error::http(classified, format!("Cursor {operation}：{detail}"))
        .with_code(code)
        .with_reset(reset)
}

pub fn classify(status: u16, body: &Value) -> Value {
    let raw = message(body);
    let lower = raw.to_ascii_lowercase();
    let code = code_text(body);
    let failed = if (400..600).contains(&status) {
        status
    } else {
        match code.as_str() {
            "invalid_argument" | "failed_precondition" | "out_of_range" => 400,
            "permission_denied" => 403,
            "not_found" => 404,
            "unavailable" => 503,
            "deadline_exceeded" => 504,
            _ => 502,
        }
    };

    if code == "cursor_partial_stream" || raw.contains("Cursor 流已输出后中断") {
        return json!({"kind":"partial_stream","status":failed,"message":raw});
    }
    if code == "cursor_append_unconfirmed" || raw.contains("Cursor BidiAppend 确认失败") {
        return json!({"kind":"append_unconfirmed","status":failed,"message":raw});
    }

    if status == 401
        || matches!(
            code.as_str(),
            "unauthenticated" | "unauthorized" | "token_expired" | "authentication_error"
        )
        || lower.contains("sign in")
        || lower.contains("unauthorized")
        || lower.contains("unauthenticated")
        || lower.contains("access token") && (lower.contains("expir") || lower.contains("invalid"))
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
            "kind": "client_version",
            "status": failed,
            "message": format!("Cursor 要求更新客户端版本：{raw}"),
            "upstream_code": code,
        });
    }

    let contains_any = |parts: &[&str]| parts.iter().any(|part| lower.contains(part));
    let quota = matches!(code.as_str(), "usage_limit" | "quota_exceeded")
        || contains_any(&[
            "usage limit",
            "quota",
            "budget limit",
            "spend limit",
            "额度",
            "用量已用完",
        ]);
    let rate = status == 429
        || code == "rate_limit_exceeded"
        || contains_any(&["rate limit", "too many requests"]);
    let overloaded = (code == "resource_exhausted" && !quota && !rate)
        || contains_any(&["model is overloaded", "at capacity", "capacity exhausted"]);
    if quota || rate || overloaded {
        return json!({
            "kind": if quota { "quota_limited" } else if rate { "rate_limited" } else { "capacity" },
            "status": if overloaded { 503 } else { 429 },
            "message": format!("Cursor 上游限制：{raw}"),
            "upstream_code": code,
        });
    }

    let blocked = ["content", "safety", "refused to serve", "policy", "blocked"]
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
        "kind": if matches!(failed, 400 | 403 | 404 | 413 | 422) { "request_invalid" } else { "fatal" },
        "status": failed,
        "message": format!("Cursor 上游请求失败：{detail}"),
        "upstream_code": code,
    })
}

fn reported_code(params: &Value) -> &str {
    params["code"]
        .as_str()
        .or_else(|| params.pointer("/error/code").and_then(Value::as_str))
        .or_else(|| params.pointer("/body/error/code").and_then(Value::as_str))
        .or_else(|| params.pointer("/body/code").and_then(Value::as_str))
        .unwrap_or_default()
}

pub fn policy(status: u16, body: &Value) -> Value {
    let class = classify(status, body);
    let kind = class["kind"].as_str().unwrap_or("fatal");
    let stopped = matches!(
        code_text(body).as_str(),
        "cursor_partial_stream" | "cursor_append_unconfirmed"
    );
    json!({
        "stop": stopped || matches!(kind, "token_expired" | "content_blocked" | "request_invalid" | "client_version" | "partial_stream" | "append_unconfirmed"),
        "kind": kind,
    })
}

pub fn retry(params: &Value) -> Value {
    let attempt = params["attempt"].as_u64().unwrap_or(0);
    let budget = params["budget"]
        .as_u64()
        .filter(|budget| *budget > 0)
        .unwrap_or(2)
        .min(2);
    if attempt >= budget {
        return json!({ "action": "stop", "reason": "已达到重试上限" });
    }
    let status = params["status"]
        .as_u64()
        .and_then(|value| u16::try_from(value).ok())
        .unwrap_or(502);
    let class = classify(status, &params["body"]);
    let code = reported_code(params);
    let kind = match code {
        "cursor_quota_limited" => "quota_limited",
        "cursor_rate_limited" => "rate_limited",
        "cursor_capacity" => "capacity",
        _ => class["kind"].as_str().unwrap_or("fatal"),
    };
    if matches!(
        code,
        "cursor_partial_stream"
            | "cursor_append_unconfirmed"
            | "cursor_max_mode_required"
            | "cursor_client_version"
    ) || matches!(
        kind,
        "token_expired"
            | "content_blocked"
            | "quota_limited"
            | "request_invalid"
            | "client_version"
            | "partial_stream"
            | "append_unconfirmed"
    ) {
        return json!({ "action": "stop", "reason": "当前请求不可直接重试" });
    }
    if !matches!(kind, "rate_limited" | "capacity")
        && !matches!(status, 408 | 425 | 429 | 500 | 502 | 503 | 504)
    {
        return json!({ "action": "default" });
    }
    let now = sdk::now();
    let reset = params["resetAt"]
        .as_i64()
        .or_else(|| params["reset_at"].as_i64())
        .or_else(|| params.pointer("/error/reset_at").and_then(Value::as_i64))
        .or_else(|| params.pointer("/body/reset_at").and_then(Value::as_i64))
        .or_else(|| {
            params
                .pointer("/body/error/reset_at")
                .and_then(Value::as_i64)
        });
    let delay = reset
        .map(|at| at.saturating_sub(now))
        .filter(|delay| *delay > 0);
    if delay.is_some_and(|delay| delay > 30_000) {
        return json!({"action":"stop","reason":"上游要求等待超过当前请求的重试预算"});
    }
    let jitter = super::client::random_bytes(2)
        .ok()
        .map(|bytes| u16::from_be_bytes([bytes[0], bytes[1]]) as u64 % 500)
        .unwrap_or(0);
    let base = if kind == "rate_limited" { 5_000 } else { 1_000 };
    let delay = delay
        .map(|delay| delay as u64)
        .unwrap_or(base * (1 << attempt))
        + jitter;
    json!({ "action": "retry", "delay_ms": delay, "reason": "Cursor 上游暂时失败，退避后重试" })
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

        let rate = classify(
            429,
            &json!({"error":{"message":"Rate limit exceeded","code":"rate_limit_exceeded"}}),
        );
        assert_eq!(rate["kind"], "rate_limited");

        let opaque = classify(
            200,
            &json!({"error":{"message":"Error","code":"resource_exhausted"}}),
        );
        assert_eq!(opaque["kind"], "capacity");
        assert_eq!(opaque["upstream_code"], "resource_exhausted");

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
        assert_eq!(stale["kind"], "client_version");
        assert!(stale["message"].as_str().unwrap().contains("更新客户端"));

        let blocked = classify(
            400,
            &json!({"message":"Blocked by safety system","code":"invalid_argument"}),
        );
        assert_eq!(blocked["kind"], "content_blocked");
    }

    #[test]
    fn retry_distinguishes_quota_rate_limit_and_partial_stream() {
        let stop = retry(&json!({
            "attempt": 0,
            "budget": 3,
            "status": 401,
            "body": {"message":"sign in required"}
        }));
        assert_eq!(stop["action"], "stop");

        let quota = retry(&json!({"status":429,"body":{"message":"usage limit"}}));
        assert_eq!(quota["action"], "stop");
        let wait = retry(&json!({"status":429,"attempt":1,"resetAt":12_000}));
        assert_eq!(wait["action"], "retry");
        assert_eq!(wait["delay_ms"], 12_000);
        let limit = retry(&json!({"status":503,"attempt":2,"budget":10}));
        assert_eq!(limit["action"], "stop");
        for code in ["cursor_partial_stream", "cursor_append_unconfirmed"] {
            let decision = retry(&json!({"status":502,"body":{"error":{"code":code}}}));
            assert_eq!(decision["action"], "stop");
        }
    }
}
