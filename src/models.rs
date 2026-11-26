//! Cursor model catalog with cache (AvailableModels).

use super::{auth, client, PROVIDER};
use agent2api_wasm_sdk::{
    self as sdk, json,
    state::{self, Scope},
    Error, Result, Value,
};

const KEY: &str = "model-catalog";
const TTL: i64 = 600_000;

const FALLBACK: &[(&str, &str, i64)] = &[
    ("claude-sonnet-4-6", "Claude Sonnet 4.6", 200_000),
    ("claude-opus-4-6", "Claude Opus 4.6", 200_000),
    ("claude-opus-4-8", "Claude Opus 4.8", 200_000),
    ("composer-2.5", "Composer 2.5", 200_000),
    ("gpt-5.4", "GPT-5.4", 200_000),
];

pub fn fallback() -> Value {
    Value::Array(
        FALLBACK
            .iter()
            .map(|(id, name, limit)| model(id, name, *limit))
            .collect(),
    )
}

fn model(id: &str, name: &str, limit: i64) -> Value {
    json!({
        "id": id,
        "name": name,
        "kind": "chat",
        "provider": PROVIDER,
        "capabilities": {
            "maxInputTokens": limit,
            "maxOutputTokens": 128000,
            "supportsTools": true
        }
    })
}

fn catalog(models: Value, remote: bool, refreshed_at: i64) -> Value {
    json!({ "models": models, "meta": { "remote": remote, "refreshed_at": refreshed_at } })
}

fn load() -> Result<state::Snapshot> {
    let snapshot = state::get(Scope::Account, KEY, 1)?;
    if !snapshot.compatible {
        return Err(Error::http(409, "Cursor 模型目录状态格式已变化"));
    }
    Ok(snapshot)
}

fn identity(secret: &Value) -> String {
    secret["identity"].as_str().unwrap_or_default().to_string()
}

fn cached_for(secret: &Value) -> Option<Value> {
    let snapshot = load().ok()?;
    if snapshot.value["source"] != identity(secret) {
        return None;
    }
    let catalog = snapshot.value.get("catalog")?;
    catalog["models"].as_array()?;
    Some(catalog.clone())
}

fn parse_models(body: &Value) -> Vec<Value> {
    let mut out = Vec::new();
    let mut seen = std::collections::BTreeSet::new();

    let models = body
        .get("models")
        .or_else(|| body.get("availableModels"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    for row in models {
        if row["isHidden"] == true
            || row["isChatOnly"] == true
            || row["onlySupportsCmdK"] == true
            || row["supportsAgent"] == false
        {
            continue;
        }
        let name = row["name"]
            .as_str()
            .or_else(|| row["modelId"].as_str())
            .unwrap_or("")
            .trim();
        if name.is_empty() {
            continue;
        }
        if name == "default" {
            if seen.insert("auto".into()) {
                out.push(model(
                    "auto",
                    row["clientDisplayName"].as_str().unwrap_or("Auto"),
                    row["contextTokenLimit"].as_i64().unwrap_or(200_000).max(1),
                ));
            }
            continue;
        }
        let title = row["clientDisplayName"]
            .as_str()
            .or_else(|| row["displayName"].as_str())
            .unwrap_or(name);
        let limit = row["contextTokenLimit"]
            .as_i64()
            .or_else(|| row["contextTokenLimitForMaxMode"].as_i64())
            .unwrap_or(200_000)
            .max(1);

        let contexts: Vec<String> = row
            .pointer("/variants")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|v| {
                v.pointer("/parameterValues/context")
                    .or_else(|| v.get("context"))
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .collect();

        if contexts.is_empty() {
            if seen.insert(name.to_string()) {
                out.push(model(name, title, limit));
            }
        } else {
            for ctx in contexts {
                let id = format!("{name}@{ctx}");
                if seen.insert(id.clone()) {
                    let tokens = parse_context_tokens(&ctx).unwrap_or(limit);
                    out.push(model(&id, &format!("{title} @ {ctx}"), tokens));
                }
            }
        }
    }

    if out.is_empty() {
        FALLBACK
            .iter()
            .map(|(id, name, limit)| model(id, name, *limit))
            .collect()
    } else {
        out
    }
}

fn parse_context_tokens(ctx: &str) -> Option<i64> {
    let lower = ctx.to_ascii_lowercase();
    if let Some(num) = lower.strip_suffix('k') {
        return num.parse::<i64>().ok().map(|n| n * 1000);
    }
    if let Some(num) = lower.strip_suffix('m') {
        return num.parse::<i64>().ok().map(|n| n * 1_000_000);
    }
    lower.parse().ok()
}

pub fn list(params: &Value) -> Result<Value> {
    if params["account"].as_str().unwrap_or("").is_empty() {
        if let Ok(cached) = sdk::catalog::cached() {
            if cached["models"].is_array() && cached["meta"]["remote"] == true {
                return Ok(cached);
            }
        }
        return Ok(catalog(fallback(), false, 0));
    }
    let account = sdk::account()?;
    if let Some(cached) = cached_for(&account["secret"]) {
        return Ok(cached);
    }
    Ok(catalog(fallback(), false, 0))
}

pub fn refresh(params: &Value) -> Result<Value> {
    let credentials = match auth::credentials(false) {
        Ok(value) => value,
        Err(error) => {
            return Ok(json!({
                "refreshed": false,
                "message": error.message,
                "count": fallback().as_array().map(Vec::len).unwrap_or(0)
            }));
        }
    };
    let previous = cached_for(&credentials).unwrap_or_else(|| catalog(fallback(), false, 0));
    let age = sdk::now().saturating_sub(previous["meta"]["refreshed_at"].as_i64().unwrap_or(0));
    if params["force"] != true && previous["meta"]["remote"] == true && (0..TTL).contains(&age) {
        return Ok(sdk::catalog::unchanged(
            previous["models"].as_array().map(Vec::len).unwrap_or(0),
        ));
    }

    let access = credentials["accessToken"]
        .as_str()
        .ok_or_else(|| Error::http(401, "请重新授权 Cursor"))?;

    let report = client::unary_timeout(
        "aiserver.v1.AiService/AvailableModels",
        access,
        json!({ "useModelParameters": true, "doNotUseMarkdown": true }),
        30_000,
    );

    let models = match report {
        Ok(body) => parse_models(&body),
        Err(error) => {
            return Ok(json!({
                "refreshed": false,
                "message": error.message,
                "count": previous["models"].as_array().map(Vec::len).unwrap_or(0)
            }));
        }
    };

    let now = sdk::now();
    let next = catalog(Value::Array(models.clone()), true, now);
    let snapshot = load()?;
    let _ = state::compare_exchange(
        Scope::Account,
        KEY,
        1,
        snapshot.revision,
        json!({ "source": identity(&credentials), "catalog": next }),
        TTL,
    )?;
    Ok(sdk::catalog::refreshed(models))
}
