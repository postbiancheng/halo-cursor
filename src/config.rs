use agent2api_wasm_sdk::{self as sdk, json, Result, Value};

/// Magpie/cursor-agent calendar build (no `cli-` prefix). Prefer a version that
/// exists under `~/.local/share/cursor-agent/versions`.
pub const DEFAULT_CLIENT_VERSION: &str = "2026.09.26-dd393fe";
pub const REASONING_AUTO: &str = "auto";

pub fn schema() -> Value {
    json!({
        "version": 1,
        "title": "Halo Cursor 设置",
        "description": "控制客户端身份、Team 与默认推理强度；模型列表在登录后从 Cursor 目录读取。",
        "runtime_overrides": false,
        "fields": [
            {
                "key": "clientVersion",
                "label": "Cursor 客户端版本",
                "type": "string",
                "default": DEFAULT_CLIENT_VERSION,
                "trim": true,
                "empty_is_default": true,
                "max_length": 64,
                "description": "上报给 Cursor 的 x-cursor-client-version（自动补 cli- 前缀），建议与本机 cursor-agent 一致。"
            },
            {
                "key": "teamId",
                "label": "Team ID",
                "type": "string",
                "default": "",
                "trim": true,
                "max_length": 128,
                "description": "可选。对应 x-cursor-team-id；Teams 账号预算限制时可填。"
            },
            {
                "key": "reasoningEffort",
                "label": "默认推理强度",
                "type": "select",
                "default": REASONING_AUTO,
                "description": "请求未带 reasoning_effort 时使用；auto 表示不强制覆盖。",
                "options": [
                    { "value": REASONING_AUTO, "label": "跟随请求（auto）" },
                    { "value": "low", "label": "low" },
                    { "value": "medium", "label": "medium" },
                    { "value": "high", "label": "high" },
                    { "value": "xhigh", "label": "xhigh" },
                    { "value": "max", "label": "max" }
                ]
            }
        ]
    })
}

/// Header value Cursor expects: `cli-<yyyy.mm.dd-hash>` (Magpie/`cursor-agent`).
pub fn client_version() -> Result<String> {
    let value = sdk::settings::get("clientVersion")?;
    let trimmed = value.as_str().map(str::trim).unwrap_or("");
    let raw = if trimmed.is_empty() {
        DEFAULT_CLIENT_VERSION
    } else {
        trimmed
    };
    let bare = raw
        .strip_prefix("cli-")
        .unwrap_or(raw)
        .chars()
        .take(48)
        .collect::<String>();
    Ok(format!("cli-{bare}"))
}

pub fn team_id() -> Result<Option<String>> {
    let value = sdk::settings::get("teamId")?;
    let trimmed = value.as_str().map(str::trim).unwrap_or("");
    if trimmed.is_empty() {
        Ok(None)
    } else {
        Ok(Some(trimmed.chars().take(128).collect()))
    }
}

pub fn reasoning_effort() -> Result<Option<String>> {
    let value = sdk::settings::get("reasoningEffort")?;
    Ok(match value.as_str().map(str::trim).unwrap_or("") {
        "" | REASONING_AUTO => None,
        other => Some(other.to_string()),
    })
}
