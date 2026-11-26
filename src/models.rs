//! Cursor model catalog with cache (AvailableModels).

use super::{auth, client, PROVIDER};
use agent2api_wasm_sdk::{
    self as sdk, json,
    state::{self, Scope},
    Error, Result, Value,
};

const KEY: &str = "model-catalog";
const REFRESH_INTERVAL: i64 = 600_000;
const RETAIN_MS: i64 = 7 * 24 * 60 * 60 * 1000;

const FALLBACK: &[(&str, &str, i64)] = &[
    ("claude-sonnet-4-6", "Claude Sonnet 4.6", 200_000),
    ("claude-opus-4-6", "Claude Opus 4.6", 200_000),
    ("claude-opus-4-8", "Claude Opus 4.8", 200_000),
    ("composer-2.5", "Composer 2.5", 200_000),
    ("gpt-5.4", "GPT-5.4", 200_000),
];

pub fn fallback() -> Value {
    let automatic = std::iter::once(model("auto", "Auto", 200_000, Some("default"), false));
    let built_in = FALLBACK
        .iter()
        .map(|(id, name, limit)| model(id, name, *limit, None, false));
    Value::Array(automatic.chain(built_in).collect())
}

fn run_from_models(
    models: &[Value],
    model_raw: &str,
    bare: &str,
    mapped: &str,
) -> Option<(String, bool)> {
    let row = models
        .iter()
        .find(|row| row["id"] == model_raw)
        .or_else(|| models.iter().find(|row| row["id"] == bare))?;
    let run = row["runId"]
        .as_str()
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .unwrap_or(mapped);
    Some((run.to_string(), row["maxMode"] == true))
}

/// Resolve a picker model id to the Agent Run id (legacySlug) + Max Mode flag.
pub fn resolve_run(model_raw: &str, account: &Value) -> (String, bool) {
    let bare = model_raw.split('@').next().unwrap_or(model_raw);
    let mapped = if bare == "auto" { "default" } else { bare };

    // Prefer the account-scoped catalog we store on refresh — it keeps `runId`.
    // Host `catalog.cached` may strip unknown fields.
    if let Some(catalog) = cached_for(account) {
        if let Some(hit) = catalog["models"]
            .as_array()
            .and_then(|models| run_from_models(models, model_raw, bare, mapped))
        {
            return hit;
        }
    }

    if let Ok(cached) = sdk::catalog::cached() {
        if let Some(models) = cached["models"].as_array() {
            if let Some(hit) = run_from_models(models, model_raw, bare, mapped) {
                return hit;
            }
        }
    }
    (mapped.to_string(), false)
}

fn model(id: &str, name: &str, limit: i64, run_id: Option<&str>, max_mode: bool) -> Value {
    // Agent2API routing / capability::effective read TOP-LEVEL keys
    // (supportsToolCall, maxInputTokens, …). Nested `capabilities.supportsTools`
    // alone is ignored, which left auto/composer as “工具调用能力未声明”.
    let mut row = json!({
        "id": id,
        "name": name,
        "kind": "chat",
        "provider": PROVIDER,
        "supportsToolCall": true,
        "supportsImages": true,
        "maxInputTokens": limit,
        "maxOutputTokens": 128000,
        "capabilities": {
            "maxInputTokens": limit,
            "maxOutputTokens": 128000,
            "supportsToolCall": true,
            "supportsImages": true,
            "supportsTools": true
        }
    });
    if let Some(run) = run_id.filter(|s| !s.is_empty()) {
        row["runId"] = json!(run);
    }
    if max_mode {
        row["maxMode"] = json!(true);
    }
    row
}

fn variant_slug(v: &Value) -> Option<&str> {
    v.get("legacySlug")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

/// Magpie `served`: keep variants whose legacySlug is in GetUsableModels (when known).
fn served_variants<'a>(
    row: &'a Value,
    usable: Option<&std::collections::BTreeSet<String>>,
) -> Vec<&'a Value> {
    let Some(variants) = row.get("variants").and_then(Value::as_array) else {
        return Vec::new();
    };
    let Some(ids) = usable else {
        return variants.iter().collect();
    };
    let got: Vec<&Value> = variants
        .iter()
        .filter(|v| variant_slug(v).is_none_or(|slug| ids.contains(slug)))
        .collect();
    if got.iter().any(|v| variant_slug(v).is_some()) {
        got
    } else {
        variants.iter().collect()
    }
}

fn is_stable_slug(slug: &str) -> bool {
    // Magpie request pick defaults to fast=false (fast costs more); thinking is opt-in.
    let s = slug.to_ascii_lowercase();
    !s.contains("fast") && !s.contains("thinking")
}

/// Pick run variant: prefer non-fast / non-thinking (Magpie `fromUsable(..., false)`),
/// then Magpie defaults, then first served.
fn default_variant<'a>(
    row: &'a Value,
    usable: Option<&std::collections::BTreeSet<String>>,
) -> Option<&'a Value> {
    let variants = served_variants(row, usable);
    let stable = |v: &&Value| variant_slug(v).is_some_and(is_stable_slug);
    variants
        .iter()
        .copied()
        .find(|v| v["isDefaultNonMaxConfig"] == true && stable(&v))
        .or_else(|| {
            variants
                .iter()
                .copied()
                .find(|v| v["isDefaultMaxConfig"] == true && stable(&v))
        })
        .or_else(|| variants.iter().copied().find(stable))
        .or_else(|| {
            variants
                .iter()
                .copied()
                .find(|v| v["isDefaultNonMaxConfig"] == true)
        })
        .or_else(|| {
            variants
                .iter()
                .copied()
                .find(|v| v["isDefaultMaxConfig"] == true)
        })
        .or_else(|| variants.first().copied())
}

/// Rough Magpie family key: drop cursor- / effort / fast / thinking suffixes.
fn family_key(id: &str) -> String {
    let mut s = id.trim().to_ascii_lowercase();
    if let Some(rest) = s.strip_prefix("cursor-") {
        s = rest.to_string();
    }
    for _ in 0..4 {
        let before = s.clone();
        for suffix in [
            "-fast",
            "-thinking",
            "-xhigh",
            "-extra-high",
            "-high",
            "-medium",
            "-low",
            "-minimal",
            "-none",
            "-max",
        ] {
            if let Some(rest) = s.strip_suffix(suffix) {
                s = rest.to_string();
                break;
            }
        }
        if s == before {
            break;
        }
    }
    s
}

/// Magpie `fromUsable(fam, "", false)`: stable usable id for this picker model.
fn usable_stable_run(
    row: &Value,
    current: Option<&str>,
    usable: &std::collections::BTreeSet<String>,
) -> Option<String> {
    let name = row["name"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())?;
    let keys = {
        let mut ks = std::collections::BTreeSet::new();
        ks.insert(family_key(name));
        if let Some(cur) = current {
            ks.insert(family_key(cur));
        }
        for slug in row
            .get("variants")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(variant_slug)
        {
            ks.insert(family_key(slug));
        }
        ks
    };
    let mut hits: Vec<&String> = usable
        .iter()
        .filter(|id| is_stable_slug(id) && keys.contains(&family_key(id)))
        .collect();
    if hits.is_empty() {
        return None;
    }
    // Magpie fromUsable(..., false): exact name → no effort → medium → rest.
    fn effort_rank(id: &str) -> u8 {
        let lower = id.to_ascii_lowercase();
        for (suf, rank) in [
            ("-xhigh", 6u8),
            ("-extra-high", 6),
            ("-high", 5),
            ("-max", 7),
            ("-medium", 2),
            ("-low", 4),
            ("-minimal", 3),
            ("-none", 3),
        ] {
            if lower.ends_with(suf) {
                return rank;
            }
        }
        1 // bare family id (Cursor's default effort)
    }
    hits.sort_by_key(|id| (id.as_str() != name, effort_rank(id), id.len(), *id));
    Some(hits[0].clone())
}

fn variant_run(
    row: &Value,
    usable: Option<&std::collections::BTreeSet<String>>,
) -> (Option<String>, bool) {
    let (run, max_mode) = match default_variant(row, usable) {
        Some(v) => {
            let run = variant_slug(v).map(str::to_string);
            let max_mode = row["supportsNonMaxMode"] == false || v["isMaxMode"] == true;
            (run, max_mode)
        }
        None => (None, false),
    };
    if let Some(ids) = usable {
        let current = run.as_deref();
        let needs_swap = current.is_none_or(|s| !is_stable_slug(s) || !ids.contains(s));
        if needs_swap {
            if let Some(preferred) = usable_stable_run(row, current, ids) {
                return (Some(preferred), max_mode);
            }
        }
    }
    (run, max_mode)
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

fn parse_models(body: &Value, usable: Option<&std::collections::BTreeSet<String>>) -> Vec<Value> {
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
                let (run, max_mode) = variant_run(&row, usable);
                out.push(model(
                    "auto",
                    row["clientDisplayName"].as_str().unwrap_or("Auto"),
                    row["contextTokenLimit"].as_i64().unwrap_or(200_000).max(1),
                    run.as_deref().or(Some("default")),
                    max_mode,
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
        let (run, max_mode) = variant_run(&row, usable);

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
                out.push(model(name, title, limit, run.as_deref(), max_mode));
            }
        } else {
            for ctx in contexts {
                let id = format!("{name}@{ctx}");
                if seen.insert(id.clone()) {
                    let tokens = parse_context_tokens(&ctx).unwrap_or(limit);
                    // Prefer a stable (non-fast / non-thinking) variant at this context.
                    let ctx_match = |v: &Value| {
                        v.pointer("/parameterValues/context")
                            .or_else(|| v.get("context"))
                            .and_then(Value::as_str)
                            == Some(ctx.as_str())
                    };
                    let ctx_variants: Vec<&Value> = row
                        .get("variants")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter(|v| ctx_match(v))
                        .collect();
                    let ctx_v = ctx_variants
                        .iter()
                        .copied()
                        .find(|v| variant_slug(v).is_some_and(is_stable_slug))
                        .or_else(|| ctx_variants.first().copied());
                    let ctx_run = ctx_v
                        .and_then(variant_slug)
                        .map(str::to_string)
                        .or(run.clone());
                    let ctx_max = ctx_v
                        .map(|v| row["supportsNonMaxMode"] == false || v["isMaxMode"] == true)
                        .unwrap_or(max_mode);
                    out.push(model(
                        &id,
                        &format!("{title} @ {ctx}"),
                        tokens,
                        ctx_run.as_deref(),
                        ctx_max,
                    ));
                }
            }
        }
    }

    if out.is_empty() {
        FALLBACK
            .iter()
            .map(|(id, name, limit)| model(id, name, *limit, None, false))
            .collect()
    } else {
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_default_model_to_auto_with_run_id() {
        let models = parse_models(
            &json!({
                "models": [{
                    "name": "default",
                    "clientDisplayName": "Auto",
                    "contextTokenLimit": 200000,
                    "supportsAgent": true,
                    "variants": [{
                        "legacySlug": "default",
                        "isDefaultNonMaxConfig": true
                    }]
                }, {
                    "name": "grok-4.6",
                    "clientDisplayName": "Grok 4.6",
                    "contextTokenLimit": 256000,
                    "supportsAgent": true,
                    "variants": [{
                        "legacySlug": "cursor-grok-4.6-high-fast",
                        "isDefaultNonMaxConfig": true
                    }, {
                        "legacySlug": "grok-4.6-high"
                    }]
                }, {
                    "name": "claude-haiku-4-5",
                    "clientDisplayName": "Claude Haiku 4.5",
                    "contextTokenLimit": 200000,
                    "supportsAgent": true,
                    "variants": [{
                        "legacySlug": "claude-4.5-haiku-thinking",
                        "isDefaultNonMaxConfig": true
                    }, {
                        "legacySlug": "claude-4.5-haiku"
                    }]
                }]
            }),
            None,
        );
        assert_eq!(models[0]["id"], "auto");
        assert_eq!(models[0]["runId"], "default");
        assert_eq!(models[0]["supportsToolCall"], true);
        assert_eq!(models[0]["supportsImages"], true);
        assert_eq!(models[0]["maxInputTokens"], 200000);
        assert_eq!(models[1]["id"], "grok-4.6");
        assert_eq!(models[1]["supportsToolCall"], true);
        // Prefer non-fast even when Magpie default is the fast SKU.
        assert_eq!(models[1]["runId"], "grok-4.6-high");
        assert_eq!(models[2]["id"], "claude-haiku-4-5");
        assert_eq!(models[2]["runId"], "claude-4.5-haiku");

        let mut usable = std::collections::BTreeSet::new();
        usable.insert("composer-2.5".into());
        usable.insert("composer-2.5-fast".into());
        let models = parse_models(
            &json!({
                "models": [{
                    "name": "composer-2.5",
                    "clientDisplayName": "Composer 2.5",
                    "supportsAgent": true,
                    "variants": [{
                        "legacySlug": "composer-2.5-fast",
                        "isDefaultNonMaxConfig": true
                    }]
                }]
            }),
            Some(&usable),
        );
        assert_eq!(models[0]["runId"], "composer-2.5");
    }

    #[test]
    fn run_mapping_prefers_exact_variant() {
        let rows = [
            json!({"id":"model","runId":"normal"}),
            json!({"id":"model@1m","runId":"long","maxMode":true}),
        ];
        assert_eq!(
            run_from_models(&rows, "model@1m", "model", "model"),
            Some(("long".into(), true))
        );
    }

    #[test]
    fn resolve_run_maps_auto_without_catalog() {
        let (id, max) = resolve_run("auto", &Value::Null);
        assert_eq!(id, "default");
        assert!(!max);
        let (id, _) = resolve_run("composer-2.5@200k", &Value::Null);
        assert_eq!(id, "composer-2.5");
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
    let started_at = sdk::now();
    let catalog_lock = client::sha256_b64url(identity(&credentials).as_bytes());
    let _lock = sdk::AccountLock::acquire_named_conversation(&format!(
        "halo-cursor/models/{catalog_lock}"
    ))?;
    let previous = cached_for(&credentials).unwrap_or_else(|| catalog(fallback(), false, 0));
    let age = sdk::now().saturating_sub(previous["meta"]["refreshed_at"].as_i64().unwrap_or(0));
    let refreshed_at = previous["meta"]["refreshed_at"].as_i64().unwrap_or(0);
    if previous["meta"]["remote"] == true
        && ((params["force"] != true && (0..REFRESH_INTERVAL).contains(&age))
            || (refreshed_at >= started_at && refreshed_at > 0))
    {
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

    let body = match report {
        Ok(body) => body,
        Err(error) => {
            return Ok(json!({
                "refreshed": false,
                "message": error.message,
                "count": previous["models"].as_array().map(Vec::len).unwrap_or(0)
            }));
        }
    };
    let usable = client::unary_optional("agent.v1.AgentService/GetUsableModels", access, json!({}))
        .and_then(|body| {
            let models = body.get("models")?.as_array()?;
            let mut ids = std::collections::BTreeSet::new();
            for row in models {
                for key in ["modelId", "displayModelId"] {
                    if let Some(id) = row.get(key).and_then(Value::as_str).map(str::trim) {
                        if !id.is_empty() {
                            ids.insert(id.to_string());
                        }
                    }
                }
            }
            if ids.is_empty() {
                None
            } else {
                Some(ids)
            }
        });

    let models = parse_models(&body, usable.as_ref());
    if models.is_empty() {
        return Ok(json!({
            "refreshed": false,
            "message": "Cursor 返回空模型目录",
            "count": previous["models"].as_array().map(Vec::len).unwrap_or(0)
        }));
    }

    let now = sdk::now();
    let next = catalog(Value::Array(models.clone()), true, now);
    let payload = json!({ "source": identity(&credentials), "catalog": next });
    // Retry once on revision conflict so chat can still see runId.
    for _ in 0..2 {
        let snapshot = load()?;
        if state::compare_exchange(
            Scope::Account,
            KEY,
            1,
            snapshot.revision,
            payload.clone(),
            RETAIN_MS,
        )? {
            return Ok(sdk::catalog::refreshed(models));
        }
    }
    Err(Error::http(409, "保存 Cursor 模型目录时发生并发更新"))
}
