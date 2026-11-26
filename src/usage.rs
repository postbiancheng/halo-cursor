//! Cursor subscription usage: period pools, on-demand, grants, Grok Bot sand.

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
    let hard = client::unary_optional(
        "aiserver.v1.DashboardService/GetHardLimit",
        access,
        json!({}),
    );
    let grants = client::unary_optional(
        "aiserver.v1.DashboardService/GetUsageLimitStatusAndActiveGrants",
        access,
        json!({}),
    )
    .and_then(|v| v.get("activeGrants").cloned())
    .and_then(|v| v.as_array().cloned())
    .unwrap_or_default();
    let sand = client::sand_usage_status(access);

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

    parse(&data, hard.as_ref(), &grants, sand.as_ref(), &plan)
}

fn number(value: Option<&Value>) -> Option<f64> {
    value.and_then(|v| {
        v.as_f64()
            .or_else(|| v.as_i64().map(|n| n as f64))
            .or_else(|| v.as_str()?.parse().ok())
    })
}

fn boolish(value: Option<&Value>) -> Option<bool> {
    value.and_then(|v| {
        v.as_bool()
            .or_else(|| v.as_str().map(|s| s.eq_ignore_ascii_case("true")))
    })
}

fn dollars(n: f64) -> String {
    format!("${n:.2}")
}

fn normalize_ms(raw: i64) -> i64 {
    if raw < 100_000_000_000 {
        raw.saturating_mul(1000)
    } else {
        raw
    }
}

fn resets_ms(data: &Value) -> Option<i64> {
    data["billingCycleEnd"]
        .as_str()
        .and_then(|s| s.parse::<i64>().ok())
        .or_else(|| data["billingCycleEnd"].as_i64())
        .filter(|n| *n > 0)
        .map(normalize_ms)
}

/// Magpie onDemandOf: spendLimitUsage (cents) + GetHardLimit (dollars).
enum OnDemand {
    Fixed {
        used_dollars: f64,
        limit_dollars: f64,
        mine_dollars: Option<f64>,
    },
    Unlimited {
        used_dollars: f64,
    },
    Disabled {
        #[allow(dead_code)]
        used_dollars: f64,
    },
    Unavailable {
        #[allow(dead_code)]
        used_dollars: f64,
    },
}

fn on_demand_of(data: &Value, hard: Option<&Value>) -> OnDemand {
    let s = data.get("spendLimitUsage");
    let used_dollars = number(s.and_then(|v| v.get("individualUsed"))).unwrap_or(0.0) / 100.0;

    let own = s.and_then(|v| {
        v.get("individualLimit")?;
        let limit = number(v.get("individualLimit")).unwrap_or(0.0);
        if limit > 0.0 {
            Some(OnDemand::Fixed {
                used_dollars,
                limit_dollars: limit / 100.0,
                mine_dollars: None,
            })
        } else {
            Some(OnDemand::Disabled { used_dollars })
        }
    });

    let team = s
        .and_then(|v| v.get("limitType"))
        .and_then(Value::as_str)
        .is_some_and(|t| t == "team");

    if team {
        if let Some(own) = own {
            return own;
        }
        if let Some(h) = hard {
            let blocked = boolish(h.get("noUsageBasedAllowed")).unwrap_or(false)
                || number(h.get("hardLimit")).unwrap_or(0.0).partial_cmp(&0.0)
                    != Some(std::cmp::Ordering::Greater);
            if blocked {
                return OnDemand::Disabled { used_dollars };
            }
        }
        let pool = number(s.and_then(|v| v.get("pooledLimit"))).unwrap_or(0.0);
        if pool > 0.0 {
            let pooled_used = number(s.and_then(|v| v.get("pooledUsed"))).unwrap_or(0.0) / 100.0;
            return OnDemand::Fixed {
                used_dollars: pooled_used,
                limit_dollars: pool / 100.0,
                mine_dollars: Some(used_dollars),
            };
        }
        return if hard.is_some() {
            OnDemand::Unlimited { used_dollars }
        } else {
            OnDemand::Unavailable { used_dollars }
        };
    }

    if hard.is_none() {
        return own.unwrap_or(OnDemand::Unavailable { used_dollars });
    }
    let h = hard.unwrap();
    if boolish(h.get("noUsageBasedAllowed")).unwrap_or(false) {
        return OnDemand::Disabled { used_dollars };
    }
    let limit = number(h.get("hardLimit")).unwrap_or(0.0);
    if limit >= 2_147_483_647.0 {
        return OnDemand::Unlimited { used_dollars };
    }
    if limit > 0.0 {
        OnDemand::Fixed {
            used_dollars,
            limit_dollars: limit,
            mine_dollars: None,
        }
    } else {
        OnDemand::Disabled { used_dollars }
    }
}

struct PercentWindow<'a> {
    name: &'a str,
    used: f64,
    resets_at: Option<i64>,
    aside: bool,
    display: Option<String>,
}

fn push_percent_window(
    windows: &mut Vec<Value>,
    wallets: &mut Vec<Value>,
    sources: &mut Vec<Value>,
    window: PercentWindow<'_>,
) {
    let PercentWindow {
        name,
        used,
        resets_at,
        aside,
        display,
    } = window;
    let used = used.clamp(0.0, 100.0);
    let remaining = (100.0 - used).max(0.0);
    let mut row = json!({
        "name": name,
        "usedPercent": used,
        "remainingPercent": remaining,
        "window": name,
        "aside": aside,
    });
    if let Some(ms) = resets_at {
        row["resetsAt"] = json!(ms);
    }
    if let Some(d) = display {
        row["display"] = json!(d);
    }
    windows.push(row.clone());
    wallets.push(json!({
        "type": if aside { "aside" } else { "percent" },
        "displayName": name,
        "usedPercent": used,
        "remainingPercent": remaining,
        "resetsAt": row.get("resetsAt").cloned().unwrap_or(Value::Null),
        "aside": aside,
    }));
    sources.push(json!({
        "id": name,
        "provider": PROVIDER,
        "label": name,
        "unit": "%",
        "usedPercent": used,
        "remainingPercent": remaining,
        "aside": aside,
    }));
}

fn parse(
    data: &Value,
    hard: Option<&Value>,
    grants: &[Value],
    sand: Option<&Value>,
    plan: &str,
) -> Result<Value> {
    let u = data.get("planUsage").filter(|v| v.is_object());
    let mut windows = Vec::new();
    let mut wallets = Vec::new();
    let mut sources = Vec::new();
    let mut credit_rows = Vec::new();

    let resets_at = resets_ms(data);
    let now = agent2api_wasm_sdk::now();

    let od = on_demand_of(data, hard);
    let free = build_credits(u, grants, resets_at, now, &mut credit_rows, &mut wallets);
    let od_open = matches!(
        &od,
        OnDemand::Fixed {
            used_dollars,
            limit_dollars,
            ..
        } if *used_dollars < *limit_dollars
    ) || matches!(od, OnDemand::Unlimited { .. });

    if let Some(u) = u {
        let auto = number(u.get("autoPercentUsed")).unwrap_or(0.0);
        let api = number(u.get("apiPercentUsed")).unwrap_or(0.0);
        let total = number(u.get("totalPercentUsed")).unwrap_or(auto.max(api));

        // When free credit covers every model, Magpie treats on-demand as aside.
        let od_aside = free.all || matches!(od, OnDemand::Unlimited { .. });

        push_percent_window(
            &mut windows,
            &mut wallets,
            &mut sources,
            PercentWindow {
                name: "Cursor Models",
                used: auto,
                resets_at,
                aside: false,
                display: None,
            },
        );
        push_percent_window(
            &mut windows,
            &mut wallets,
            &mut sources,
            PercentWindow {
                name: "Other Models",
                used: api,
                resets_at,
                aside: false,
                display: None,
            },
        );

        match &od {
            OnDemand::Fixed {
                used_dollars,
                limit_dollars,
                mine_dollars,
            } if *limit_dollars > 0.0 => {
                let used_pct = ((*used_dollars / *limit_dollars) * 100.0).clamp(0.0, 100.0);
                let mut display =
                    format!("{} / {}", dollars(*used_dollars), dollars(*limit_dollars));
                if let Some(mine) = mine_dollars {
                    display.push_str(&format!(" (you: {})", dollars(*mine)));
                }
                push_percent_window(
                    &mut windows,
                    &mut wallets,
                    &mut sources,
                    PercentWindow {
                        name: "On-demand",
                        used: used_pct,
                        resets_at,
                        aside: od_aside,
                        display: Some(display),
                    },
                );
            }
            OnDemand::Unlimited { used_dollars } => {
                push_percent_window(
                    &mut windows,
                    &mut wallets,
                    &mut sources,
                    PercentWindow {
                        name: "On-demand",
                        used: 0.0,
                        resets_at,
                        aside: true,
                        display: Some(dollars(*used_dollars)),
                    },
                );
            }
            _ => {}
        }

        if let Some(bot) = sand_window(sand, now) {
            let name = bot.name;
            push_percent_window(
                &mut windows,
                &mut wallets,
                &mut sources,
                PercentWindow {
                    name: &name,
                    used: bot.used,
                    resets_at: bot.resets_at,
                    aside: true,
                    display: None,
                },
            );
        }

        // Free-credit windows (aside) sit with Grok Bot before Total.
        for row in &free.aside_windows {
            windows.push(row.clone());
        }

        push_percent_window(
            &mut windows,
            &mut wallets,
            &mut sources,
            PercentWindow {
                name: "Total",
                used: total,
                resets_at,
                aside: true,
                display: None,
            },
        );

        if credit_rows.is_empty() {
            credit_rows.push(json!({ "name": "赠金", "remain": 0.0, "unit": "credits" }));
        }

        let pool_available = windows
            .iter()
            .filter(|w| w["aside"].as_bool() != Some(true))
            .filter_map(|w| w["remainingPercent"].as_f64())
            .reduce(f64::min);

        // Spent included pools can still serve via on-demand or unrestricted free credit.
        let available = if pool_available.is_some_and(|p| p <= 0.0) && (od_open || free.all) {
            Value::Null
        } else {
            json!(pool_available)
        };

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
                "spendLimitUsage": data.get("spendLimitUsage"),
                "hardLimit": hard,
                "activeGrants": grants,
                "sandUsage": sand,
            }
        });
        if let Some(ms) = resets_at {
            if let Some(obj) = report["subscription"].as_object_mut() {
                obj.insert("expireAt".into(), json!(ms));
            } else if !plan.is_empty() {
                report["subscription"] = json!({ "planName": plan, "expireAt": ms });
            }
        }
        return Ok(report);
    }

    // Enterprise / missing planUsage: still surface sand + grants when present.
    if let Some(bot) = sand_window(sand, now) {
        push_percent_window(
            &mut windows,
            &mut wallets,
            &mut sources,
            PercentWindow {
                name: &bot.name,
                used: bot.used,
                resets_at: bot.resets_at,
                aside: true,
                display: None,
            },
        );
    }
    let _ = build_credits(None, grants, resets_at, now, &mut credit_rows, &mut wallets);
    if credit_rows.is_empty() {
        credit_rows.push(json!({ "name": "赠金", "remain": 0.0, "unit": "credits" }));
    }

    Ok(json!({
        "available": Value::Null,
        "unit": "%",
        "windows": windows,
        "wallets": wallets,
        "quotaSources": sources,
        "credits": credit_rows,
        "subscription": if plan.is_empty() { Value::Null } else { json!({ "planName": plan }) },
        "raw": data,
    }))
}

struct FreeCredit {
    all: bool,
    aside_windows: Vec<Value>,
}

fn build_credits(
    plan: Option<&Value>,
    grants: &[Value],
    resets_at: Option<i64>,
    now: i64,
    credit_rows: &mut Vec<Value>,
    wallets: &mut Vec<Value>,
) -> FreeCredit {
    let mut aside_windows = Vec::new();
    let mut all = false;

    let bonus_spend = number(plan.and_then(|p| p.get("bonusSpend"))).unwrap_or(0.0) / 100.0;
    let remaining_bonus = boolish(plan.and_then(|p| p.get("remainingBonus"))).unwrap_or(false);
    if bonus_spend > 0.0 || remaining_bonus {
        let display = format!(
            "+{} on us{}",
            dollars(bonus_spend),
            if remaining_bonus { ", more left" } else { "" }
        );
        let mut win = json!({
            "name": "Free usage",
            "usedPercent": 0.0,
            "remainingPercent": if remaining_bonus { 100.0 } else { 0.0 },
            "window": "Free usage",
            "aside": true,
            "display": display,
            "unit": "USD",
        });
        if let Some(ms) = resets_at {
            win["resetsAt"] = json!(ms);
        }
        aside_windows.push(win);
        credit_rows.push(json!({
            "name": "Free usage",
            "remain": if remaining_bonus { Value::Null } else { json!(0.0) },
            "display": format!("+{} on us{}", dollars(bonus_spend), if remaining_bonus { ", more left" } else { "" }),
            "unit": "USD",
            "aside": true,
        }));
        if remaining_bonus {
            all = true;
            wallets.push(json!({
                "type": "credits",
                "displayName": "Free usage",
                "balance": Value::Null,
                "unit": "USD",
                "aside": true,
            }));
        }
    }

    for g in grants {
        let total = number(g.get("totalCents")).unwrap_or(0.0) / 100.0;
        let left = number(g.get("remainingCents")).unwrap_or(0.0).max(0.0) / 100.0;
        if total.partial_cmp(&0.0) != Some(std::cmp::Ordering::Greater)
            || !total.is_finite()
            || !left.is_finite()
        {
            continue;
        }
        let exp = number(g.get("expiresAtMs")).unwrap_or(0.0) as i64;
        if exp > 0 && exp <= now {
            continue;
        }
        let clients: Vec<String> = g
            .get("allowedClientTypes")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        let mine = clients.is_empty() || clients.iter().any(|c| c == "cli");
        let named: Vec<String> = {
            let mut ids = g
                .get("allowedModelIds")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            if let Some(sels) = g.get("allowedModelSelections").and_then(Value::as_array) {
                for m in sels {
                    if let Some(id) = m.get("modelId").and_then(Value::as_str) {
                        ids.push(id.to_string());
                    }
                }
            }
            ids
        };
        let tags: Vec<String> = g
            .get("allowedModelTags")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        let limited = !named.is_empty() || !tags.is_empty();

        let campaign = g
            .get("campaignName")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("Free credit");
        let name = if mine {
            campaign.to_string()
        } else if clients.iter().any(|c| c == "glass") && !clients.iter().any(|c| c == "ide") {
            format!("{campaign} (Cloud Agents only)")
        } else {
            format!("{campaign} ({})", clients.join(", "))
        };

        let used_amt = (total - left).max(0.0);
        let used_pct = ((used_amt / total) * 100.0).clamp(0.0, 100.0);
        let display = format!("{} / {}", dollars(used_amt), dollars(total));
        let mut win = json!({
            "name": name,
            "usedPercent": used_pct,
            "remainingPercent": (100.0 - used_pct).max(0.0),
            "window": name,
            "aside": true,
            "display": display,
            "amount": used_amt,
            "limit": total,
            "unit": "USD",
        });
        if exp > 0 {
            win["expireAt"] = json!(exp);
            win["runsOut"] = json!(left);
        }
        if limited {
            let mut models = named.clone();
            models.extend(tags.clone());
            win["models"] = json!(models);
        }
        aside_windows.push(win);

        let mut credit = json!({
            "name": name,
            "remain": left,
            "limit": total,
            "used": used_amt,
            "unit": "USD",
            "display": display,
            "aside": true,
        });
        if exp > 0 {
            credit["expireAt"] = json!(exp);
        }
        if limited {
            let mut models = named;
            models.extend(tags);
            credit["models"] = json!(models);
        }
        credit_rows.push(credit);

        wallets.push(json!({
            "type": "credits",
            "displayName": name,
            "balance": left,
            "unit": "USD",
            "aside": true,
            "expireAt": if exp > 0 { json!(exp) } else { Value::Null },
        }));

        if mine && left > 0.0 && !limited {
            all = true;
        }
    }

    FreeCredit { all, aside_windows }
}

struct SandWindow {
    name: String,
    used: f64,
    resets_at: Option<i64>,
}

fn sand_window(sand: Option<&Value>, now: i64) -> Option<SandWindow> {
    let data = sand?;
    let paid = boolish(data.get("includedLimitZero"))
        .map(|zero| !zero)
        .or_else(|| boolish(data.get("hasNonZeroIncludedLimit")))
        .unwrap_or(false);
    let trial = !paid
        && data
            .get("sandTrialExpiresAt")
            .and_then(Value::as_str)
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .is_some_and(|t| t.timestamp_millis() > now);
    let usage = number(data.get("usagePercent"))?;
    if (!paid && !trial) || !usage.is_finite() {
        return None;
    }
    let resets_at = if paid {
        data.get("nextResetTimestampUtc")
            .and_then(Value::as_str)
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|t| t.timestamp_millis())
    } else {
        None
    };
    Some(SandWindow {
        name: if trial {
            "Grok Bot (trial)".into()
        } else {
            "Grok Bot".into()
        },
        used: usage.clamp(0.0, 100.0),
        resets_at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_included_pools_and_aside_total() {
        let report = parse(
            &json!({
                "billingCycleEnd": "1800000000",
                "planUsage": {
                    "autoPercentUsed": 25.0,
                    "apiPercentUsed": 10.0,
                    "totalPercentUsed": 20.0,
                    "remainingBonus": false,
                    "bonusSpend": 0
                }
            }),
            None,
            &[],
            None,
            "Pro+",
        )
        .unwrap();
        assert_eq!(report["available"], json!(75.0));
        assert_eq!(report["windows"][0]["name"], "Cursor Models");
        assert_eq!(report["windows"][0]["remainingPercent"], json!(75.0));
        assert_eq!(report["windows"][1]["name"], "Other Models");
        let total = report["windows"]
            .as_array()
            .unwrap()
            .iter()
            .find(|w| w["name"] == "Total")
            .unwrap();
        assert_eq!(total["aside"], true);
        assert_eq!(report["subscription"]["planName"], "Pro+");
    }

    #[test]
    fn surfaces_on_demand_and_ignores_disabled_hard_limit() {
        let open = parse(
            &json!({
                "billingCycleEnd": 1800000000000_i64,
                "planUsage": {
                    "autoPercentUsed": 100.0,
                    "apiPercentUsed": 100.0,
                    "totalPercentUsed": 100.0
                },
                "spendLimitUsage": { "individualUsed": 250, "individualLimit": 1000 }
            }),
            Some(&json!({ "hardLimit": 50, "noUsageBasedAllowed": false })),
            &[],
            None,
            "Pro",
        )
        .unwrap();
        // Magpie: when GetHardLimit is present (non-team), hardLimit (dollars) wins.
        let od = open["windows"]
            .as_array()
            .unwrap()
            .iter()
            .find(|w| w["name"] == "On-demand")
            .unwrap();
        assert_eq!(od["display"], "$2.50 / $50.00");
        assert!(open["available"].is_null()); // pools spent but on-demand open

        let by_spend = parse(
            &json!({
                "planUsage": {
                    "autoPercentUsed": 50.0,
                    "apiPercentUsed": 0.0,
                    "totalPercentUsed": 50.0
                },
                "spendLimitUsage": { "individualUsed": 250, "individualLimit": 1000 }
            }),
            None,
            &[],
            None,
            "",
        )
        .unwrap();
        let od2 = by_spend["windows"]
            .as_array()
            .unwrap()
            .iter()
            .find(|w| w["name"] == "On-demand")
            .unwrap();
        assert_eq!(od2["display"], "$2.50 / $10.00");

        let disabled = parse(
            &json!({
                "planUsage": {
                    "autoPercentUsed": 50.0,
                    "apiPercentUsed": 0.0,
                    "totalPercentUsed": 50.0
                }
            }),
            Some(&json!({ "noUsageBasedAllowed": true })),
            &[],
            None,
            "",
        )
        .unwrap();
        assert!(disabled["windows"]
            .as_array()
            .unwrap()
            .iter()
            .all(|w| w["name"] != "On-demand"));
    }

    #[test]
    fn surfaces_grok_bot_aside_and_free_usage() {
        let report = parse(
            &json!({
                "billingCycleEnd": "1800000000",
                "planUsage": {
                    "autoPercentUsed": 10.0,
                    "apiPercentUsed": 0.0,
                    "totalPercentUsed": 10.0,
                    "remainingBonus": true,
                    "bonusSpend": 1234
                }
            }),
            None,
            &[],
            Some(&json!({
                "hasNonZeroIncludedLimit": true,
                "usagePercent": 40.0,
                "nextResetTimestampUtc": "2026-12-01T00:00:00.000Z"
            })),
            "Pro+",
        )
        .unwrap();
        let bot = report["windows"]
            .as_array()
            .unwrap()
            .iter()
            .find(|w| w["name"] == "Grok Bot")
            .unwrap();
        assert_eq!(bot["aside"], true);
        assert_eq!(bot["remainingPercent"], json!(60.0));
        let free = report["credits"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["name"] == "Free usage")
            .unwrap();
        assert!(free["display"].as_str().unwrap().contains("$12.34"));
    }

    #[test]
    fn surfaces_active_grants_as_usd_credits() {
        let report = parse(
            &json!({
                "planUsage": {
                    "autoPercentUsed": 0.0,
                    "apiPercentUsed": 0.0,
                    "totalPercentUsed": 0.0
                }
            }),
            None,
            &[json!({
                "campaignName": "Promo",
                "totalCents": "1000",
                "remainingCents": "250",
                "expiresAtMs": "9999999999999",
                "allowedClientTypes": ["cli"]
            })],
            None,
            "Pro",
        )
        .unwrap();
        let credit = report["credits"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["name"] == "Promo")
            .unwrap();
        assert_eq!(credit["remain"], json!(2.5));
        assert_eq!(credit["limit"], json!(10.0));
        assert_eq!(credit["unit"], "USD");
    }
}
