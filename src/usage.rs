//! Cursor subscription usage via GetCurrentPeriodUsage.

use super::{auth, client, PROVIDER};
use agent2api_wasm_sdk::{json, Error, Result, Value};

pub fn query() -> Result<Value> {
    let secret = auth::credentials(false)?;
    let access = secret["accessToken"]
        .as_str()
        .ok_or_else(|| Error::http(401, "请重新授权 Cursor"))?;

    let data = client::unary(
        "aiserver.v1.DashboardService/GetCurrentPeriodUsage",
        access,
        json!({}),
    )?;
    let plan = client::get_plan(access)
        .and_then(|p| {
            p.pointer("/planInfo/planName")
                .and_then(Value::as_str)
                .map(|s| s.trim().to_string())
        })
        .filter(|s| !s.is_empty())
        .or_else(|| {
            secret["plan"]
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        })
        .unwrap_or_default();

    parse(&data, &plan)
}

fn number(value: Option<&Value>) -> Option<f64> {
    value.and_then(|v| {
        v.as_f64()
            .or_else(|| v.as_i64().map(|n| n as f64))
            .or_else(|| v.as_str()?.parse().ok())
    })
}

fn parse(data: &Value, plan: &str) -> Result<Value> {
    let u = data.get("planUsage").filter(|v| v.is_object());
    let mut windows = Vec::new();
    let mut wallets = Vec::new();
    let mut sources = Vec::new();

    let resets_at = data["billingCycleEnd"]
        .as_str()
        .and_then(|s| s.parse::<i64>().ok())
        .or_else(|| data["billingCycleEnd"].as_i64())
        .filter(|n| *n > 0);

    let resets = resets_at.map(|ms| {
        let ms = if ms < 100_000_000_000 {
            ms.saturating_mul(1000)
        } else {
            ms
        };
        json!({ "resetsAt": ms })
    });

    if let Some(u) = u {
        let auto = number(u.get("autoPercentUsed")).unwrap_or(0.0);
        let api = number(u.get("apiPercentUsed")).unwrap_or(0.0);
        let total = number(u.get("totalPercentUsed")).unwrap_or(auto.max(api));

        push_window(
            &mut windows,
            &mut wallets,
            &mut sources,
            "Cursor Models",
            auto,
            resets.as_ref(),
        );
        push_window(
            &mut windows,
            &mut wallets,
            &mut sources,
            "Other Models",
            api,
            resets.as_ref(),
        );
        push_window(
            &mut windows,
            &mut wallets,
            &mut sources,
            "Total",
            total,
            resets.as_ref(),
        );

        let bonus = number(u.get("remainingBonus")).or_else(|| {
            number(u.get("bonusSpend")).map(|spent| {
                let total_bonus = number(u.get("totalBonus")).unwrap_or(spent);
                (total_bonus - spent).max(0.0) / 100.0
            })
        });

        let mut credit_rows = Vec::new();
        if let Some(remain) = bonus {
            credit_rows.push(json!({
                "name": "赠金",
                "remain": remain,
                "unit": "credits"
            }));
            wallets.push(json!({
                "type": "credits",
                "displayName": "赠金",
                "balance": remain,
                "unit": "credits"
            }));
        } else {
            // Host subscription line needs a non-empty credits array.
            credit_rows.push(json!({ "name": "赠金", "remain": 0.0, "unit": "credits" }));
        }

        let available = windows
            .iter()
            .filter_map(|w| w["remainingPercent"].as_f64())
            .reduce(f64::min);

        let mut report = json!({
            "available": available,
            "unit": "%",
            "windows": windows,
            "wallets": wallets,
            "quotaSources": sources,
            "credits": credit_rows,
            "subscription": if plan.is_empty() { Value::Null } else { json!({ "planName": plan }) },
            "raw": {
                "planUsage": u,
                "billingCycleEnd": data.get("billingCycleEnd"),
                "billingCycleStart": data.get("billingCycleStart"),
            }
        });
        if let Some(ms) = resets_at {
            let ms = if ms < 100_000_000_000 {
                ms.saturating_mul(1000)
            } else {
                ms
            };
            if let Some(obj) = report["subscription"].as_object_mut() {
                obj.insert("expireAt".into(), json!(ms));
            } else if !plan.is_empty() {
                report["subscription"] = json!({ "planName": plan, "expireAt": ms });
            }
        }
        return Ok(report);
    }

    // Enterprise / missing planUsage: still return a usable empty shell.
    Ok(json!({
        "available": Value::Null,
        "unit": "%",
        "windows": [],
        "wallets": [],
        "quotaSources": [],
        "credits": [{ "name": "赠金", "remain": 0.0, "unit": "credits" }],
        "subscription": if plan.is_empty() { Value::Null } else { json!({ "planName": plan }) },
        "raw": data,
    }))
}

fn push_window(
    windows: &mut Vec<Value>,
    wallets: &mut Vec<Value>,
    sources: &mut Vec<Value>,
    name: &str,
    used: f64,
    resets: Option<&Value>,
) {
    let used = used.clamp(0.0, 100.0);
    let remaining = (100.0 - used).max(0.0);
    let mut row = json!({
        "name": name,
        "usedPercent": used,
        "remainingPercent": remaining,
        "window": name,
    });
    if let Some(r) = resets {
        if let Some(at) = r.get("resetsAt") {
            row["resetsAt"] = at.clone();
        }
    }
    windows.push(row.clone());
    wallets.push(json!({
        "type": "percent",
        "displayName": name,
        "usedPercent": used,
        "remainingPercent": remaining,
        "resetsAt": row.get("resetsAt").cloned().unwrap_or(Value::Null),
    }));
    sources.push(json!({
        "id": name,
        "provider": PROVIDER,
        "label": name,
        "unit": "%",
        "usedPercent": used,
        "remainingPercent": remaining,
    }));
}
