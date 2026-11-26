//! Cursor Agent RunSSE + BidiAppend chat forward (HTTP/1.1, Magpie-aligned).

use super::{
    auth, client, config, errors, models,
    proto::{self, Field, FrameDecoder, Pb},
};
use agent2api_wasm_sdk::{self as sdk, http, json, Error, Result, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

const NS: &str = "tools";
const CALL: &str = "CallDynamicTool";
const NO_RESULT: &str = "Tool use was interrupted and did not produce a result.";
const HEARTBEAT_MS: i64 = 5_000;
const STALL_MS: i64 = 120_000;
const APPEND_TIMEOUT_MS: u64 = 60_000;

fn text_of(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|p| p["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn reasoning_of(message: &Value) -> String {
    message["reasoning_content"]
        .as_str()
        .or_else(|| message["reasoning"].as_str())
        .unwrap_or("")
        .to_string()
}

fn parse_args(raw: &str) -> Value {
    serde_json::from_str(raw).unwrap_or_else(|_| json!({}))
}

struct ToolDef {
    name: String,
    description: String,
    schema: Value,
    schema_text: String,
}

fn tools_of(body: &Value) -> Vec<ToolDef> {
    let mut out = Vec::new();
    for tool in body["tools"].as_array().into_iter().flatten() {
        let function = if tool["type"] == "function" || tool.get("function").is_some() {
            tool.get("function").unwrap_or(tool)
        } else {
            tool
        };
        let name = function["name"].as_str().unwrap_or("").trim();
        if name.is_empty() {
            continue;
        }
        let description = function["description"].as_str().unwrap_or("").to_string();
        let schema = function
            .get("parameters")
            .cloned()
            .unwrap_or_else(|| json!({"type":"object","properties":{}}));
        let schema_text = schema.to_string();
        out.push(ToolDef {
            name: name.to_string(),
            description,
            schema,
            schema_text,
        });
    }
    out
}

fn catalog(tools: &[ToolDef]) -> String {
    if tools.is_empty() {
        return String::new();
    }
    let mut s = format!(
        "\n\n<dynamic_tool_catalog>\nThe MCP namespace \"{NS}\" has the tools below. They are reached only through `{CALL}`, never as tools of their own. Each entry is the exact call to make; its arguments must match the schema given.\n"
    );
    for t in tools {
        s.push_str(&format!(
            "<call>{CALL}({{\"namespace\":\"{NS}\",\"toolName\":\"{}\",\"arguments\":{{...}}}})\n{}\narguments schema: {}\n</call>\n",
            t.name, t.description, t.schema_text
        ));
    }
    s.push_str("</dynamic_tool_catalog>");
    s
}

fn tool_result(id: &str, text: &str, is_error: bool) -> Value {
    let result = serde_json::from_str::<Value>(text).unwrap_or_else(|_| json!(text));
    let mut row = json!({
        "type": "tool-result",
        "toolCallId": id,
        "toolName": CALL,
        "result": result,
        "experimental_content": [{ "type": "text", "text": text }],
    });
    if is_error {
        row["isError"] = json!(true);
    }
    row
}

fn conversation_blobs(body: &Value, tools: &[ToolDef]) -> Result<(Vec<Vec<u8>>, String, usize)> {
    let messages = body["messages"]
        .as_array()
        .ok_or_else(|| Error::http(400, "缺少 messages"))?;
    let mut out: Vec<Vec<u8>> = Vec::new();
    let mut size = 0usize;
    let mut last = ".".to_string();
    let mut system = Vec::new();
    for m in messages {
        let role = m["role"].as_str().unwrap_or("");
        if role == "system" || role == "developer" {
            system.push(text_of(&m["content"]));
        }
    }
    let sys = {
        let mut s = system.into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join("\n\n");
        s.push_str(&catalog(tools));
        s
    };
    size += sys.len();
    if !sys.is_empty() {
        out.push(serde_json::to_vec(&json!({"role":"system","content":sys})).unwrap_or_default());
    }

    let mut pending: Vec<String> = Vec::new();
    let mut results: Vec<Value> = Vec::new();
    let flush_results = |pending: &mut Vec<String>, results: &mut Vec<Value>, out: &mut Vec<Vec<u8>>| {
        for id in pending.drain(..) {
            results.push(tool_result(&id, NO_RESULT, true));
        }
        if !results.is_empty() {
            out.push(
                serde_json::to_vec(&json!({"role":"tool","content":results.clone()})).unwrap_or_default(),
            );
            results.clear();
        }
    };

    for m in messages {
        let role = m["role"].as_str().unwrap_or("");
        match role {
            "system" | "developer" => {}
            "tool" => {
                let id = m["tool_call_id"].as_str().unwrap_or("").to_string();
                if let Some(pos) = pending.iter().position(|p| p == &id) {
                    pending.remove(pos);
                    let text = text_of(&m["content"]);
                    size += text.len();
                    let is_error = m["is_error"] == true || m["isError"] == true;
                    results.push(tool_result(&id, &text, is_error));
                }
            }
            "assistant" => {
                flush_results(&mut pending, &mut results, &mut out);
                let mut content = Vec::new();
                let reasoning = reasoning_of(m);
                if !reasoning.is_empty() {
                    content.push(json!({"type":"reasoning","text":reasoning}));
                    size += reasoning.len();
                }
                let text = text_of(&m["content"]);
                if !text.is_empty() {
                    content.push(json!({"type":"text","text":text}));
                    size += text.len();
                }
                for call in m["tool_calls"].as_array().into_iter().flatten() {
                    let name = call["function"]["name"].as_str().unwrap_or("");
                    let args_raw = call["function"]["arguments"].as_str().unwrap_or("{}");
                    let args = parse_args(args_raw);
                    size += args_raw.len() + name.len();
                    let id = call["id"].as_str().unwrap_or("").to_string();
                    content.push(json!({
                        "type": "tool-call",
                        "toolCallId": id,
                        "toolName": CALL,
                        "args": { "namespace": NS, "toolName": name, "arguments": args }
                    }));
                    pending.push(call["id"].as_str().unwrap_or("").to_string());
                }
                if !content.is_empty() {
                    out.push(
                        serde_json::to_vec(&json!({"role":"assistant","content":content}))
                            .unwrap_or_default(),
                    );
                }
            }
            "user" => {
                flush_results(&mut pending, &mut results, &mut out);
                let mut content = Vec::new();
                let mut first = String::new();
                match &m["content"] {
                    Value::String(s) if !s.is_empty() => {
                        content.push(json!({"type":"text","text":s}));
                        first = s.clone();
                        size += s.len();
                    }
                    Value::Array(parts) => {
                        for p in parts {
                            match p["type"].as_str().unwrap_or("text") {
                                "text" | "input_text" => {
                                    if let Some(t) = p["text"].as_str() {
                                        content.push(json!({"type":"text","text":t}));
                                        if first.is_empty() {
                                            first = t.to_string();
                                        }
                                        size += t.len();
                                    }
                                }
                                "image_url" => {
                                    content.push(json!({
                                        "type": "text",
                                        "text": "[attachment image]"
                                    }));
                                }
                                _ => {}
                            }
                        }
                    }
                    _ => {}
                }
                if !first.is_empty() {
                    last = first;
                }
                if !content.is_empty() {
                    out.push(
                        serde_json::to_vec(&json!({"role":"user","content":content}))
                            .unwrap_or_default(),
                    );
                }
            }
            _ => return Err(Error::http(400, "Halo Cursor 不支持该消息角色")),
        }
    }
    flush_results(&mut pending, &mut results, &mut out);
    Ok((out, last, size / 4))
}

fn tool_def_bytes(t: &ToolDef) -> Vec<u8> {
    let mut d = Pb::new().str(1, &t.name);
    if !t.description.is_empty() {
        d = d.str(2, &t.description);
    }
    d.bytes(3, &proto::pb_value(&t.schema))
        .str(4, NS)
        .str(5, &t.name)
        .str(6, &t.schema_text)
        .done()
}

fn env_bytes() -> Vec<u8> {
    Pb::new().str(1, "macos").str(2, "/tmp").str(10, "UTC").done()
}

fn put_blob(blobs: &mut BTreeMap<String, Vec<u8>>, bytes: Vec<u8>) -> [u8; 32] {
    let id = proto::sha256_raw(&bytes);
    blobs.insert(hex::encode(id), bytes);
    id
}

fn build_run(
    msgs: &[Vec<u8>],
    last_user: &str,
    tools: &[ToolDef],
    model: &str,
    conv: &str,
    max_mode: bool,
    params: &[(String, String)],
) -> (Vec<u8>, BTreeMap<String, Vec<u8>>) {
    let mut blobs = BTreeMap::new();
    let mut state = Pb::new();
    for m in msgs {
        let id = put_blob(&mut blobs, m.clone());
        state = state.bytes(1, &id);
    }
    let mid = client::uuid_v4().unwrap_or_else(|_| "00000000-0000-4000-8000-000000000000".into());
    let user = Pb::new()
        .str(1, last_user)
        .str(2, &mid)
        .varint(4, 1)
        .done();
    let user_id = put_blob(&mut blobs, user);
    let turn_inner = Pb::new().bytes(1, &user_id).str(10, &mid).done();
    let turn = Pb::new().bytes(1, &turn_inner).done();
    let turn_id = put_blob(&mut blobs, turn);
    state = state.bytes(8, &turn_id).varint(10, 1).str(22, "cli");

    let defs: Vec<Vec<u8>> = tools.iter().map(tool_def_bytes).collect();
    let mut rc = Pb::new().bytes(4, &env_bytes());
    let mut mcp = Pb::new();
    for d in &defs {
        rc = rc.bytes(7, d);
        mcp = mcp.bytes(1, d);
    }
    let action = Pb::new().nested(2, Pb::new().nested(2, rc));

    let mut details = Pb::new().str(1, model).str(3, model).str(4, model);
    let mut requested = Pb::new().str(1, model);
    if max_mode {
        details = details.varint(7, 1);
        requested = requested.varint(2, 1);
    }
    for (id, value) in params {
        requested = requested.nested(3, Pb::new().str(1, id).str(2, value));
    }

    let cid = if conv.is_empty() {
        client::uuid_v4().unwrap_or_else(|_| mid.clone())
    } else {
        conv.to_string()
    };

    let rr = Pb::new()
        .nested(1, state)
        .nested(2, action)
        .nested(3, details)
        .nested(4, mcp)
        .str(5, &cid)
        .nested(9, requested)
        .varint(19, 1)
        .varint(37, 1);

    let first = Pb::new().nested(1, rr).done();
    (first, blobs)
}

fn conversation_id(body: &Value) -> String {
    let key = body["prompt_cache_key"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("");
    if key.is_empty() {
        return String::new();
    }
    let first = body["messages"]
        .as_array()
        .and_then(|msgs| {
            msgs.iter()
                .find(|m| m["role"] == "user")
                .or_else(|| msgs.first())
        })
        .cloned()
        .unwrap_or(Value::Null);
    let mut hasher = Sha256::new();
    hasher.update(b"cursor conversation\0");
    hasher.update(key.as_bytes());
    hasher.update(b"\0");
    hasher.update(first.to_string().as_bytes());
    let sum = hasher.finalize();
    let mut b = sum[..16].to_vec();
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h = hex::encode(b);
    format!(
        "{}-{}-{}-{}-{}",
        &h[0..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..32]
    )
}

fn model_params(body: &Value) -> (bool, Vec<(String, String)>) {
    let mut params = Vec::new();
    let mut max_mode = body["model"]
        .as_str()
        .map(|m| m.contains("@1m") || m.contains("1m"))
        .unwrap_or(false);

    let default_effort = config::reasoning_effort().ok().flatten();
    if let Some(effort) = body["reasoning_effort"]
        .as_str()
        .or_else(|| body.pointer("/reasoning/effort").and_then(Value::as_str))
        .or(default_effort.as_deref())
    {
        params.push(("effort".into(), effort.to_string()));
    }

    if let Some(ctx) = body["model"].as_str().and_then(|m| m.split('@').nth(1)) {
        params.push(("context".into(), ctx.to_string()));
        if ctx == "1m" {
            max_mode = true;
        }
    }
    (max_mode, params)
}

fn run_model_id(model: &str) -> String {
    let bare = model.split('@').next().unwrap_or(model);
    if bare == "auto" {
        "default".into()
    } else {
        bare.to_string()
    }
}

fn max_mode_required(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("max mode required")
        || lower.contains("max mode") && lower.contains("required")
        || lower.contains("请开启 max mode")
}

fn bidi_append(headers: &Value, request_id: &str, msg: &[u8], seq: u64) -> Result<()> {
    let body = Pb::new()
        .str(1, &hex::encode(msg))
        .nested(2, Pb::new().str(1, request_id))
        .varint(3, seq)
        .done();
    let mut h = headers.clone();
    if let Some(obj) = h.as_object_mut() {
        obj.insert("content-type".into(), json!("application/proto"));
        obj.insert("connect-protocol-version".into(), json!("1"));
        obj.insert("accept-encoding".into(), json!("identity"));
    }
    let response = http::open_raw_timeout(
        "POST",
        &format!("{}/aiserver.v1.BidiService/BidiAppend", client::API),
        h,
        Some(&body),
        APPEND_TIMEOUT_MS,
    )?;
    let status = response.status;
    if !(200..300).contains(&status) {
        let text = String::from_utf8_lossy(&response.bytes(4096)?).into_owned();
        return Err(Error::http(
            status,
            format!("BidiAppend 失败：{}", text.chars().take(200).collect::<String>()),
        ));
    }
    let _ = response.bytes(64 << 10);
    Ok(())
}

struct StreamOut<'a> {
    model: &'a str,
    id: String,
    tool_index: usize,
}

impl StreamOut<'_> {
    fn emit(&self, delta: Value, finish: Value, usage: Option<Value>) -> Result<()> {
        let mut frame = json!({
            "id": self.id,
            "object": "chat.completion.chunk",
            "model": self.model,
            "created": sdk::now() / 1000,
            "choices": [{ "index": 0, "delta": delta, "finish_reason": finish }]
        });
        if let Some(value) = usage {
            frame["usage"] = value;
        }
        sdk::stream_event(&frame)
    }
}

fn usage_of(uf: &[Field], shift: u32) -> Value {
    let inp = proto::pb_num(uf, 1 + shift);
    let out = proto::pb_num(uf, 2 + shift);
    let cr = proto::pb_num(uf, 3 + shift);
    let cw = proto::pb_num(uf, 4 + shift);
    let reasoning = proto::pb_num(uf, 5 + shift);
    let input = inp.saturating_sub(cr).saturating_sub(cw);
    let prompt = input + cr + cw;
    let mut usage = json!({
        "prompt_tokens": prompt,
        "completion_tokens": out,
        "total_tokens": prompt + out,
        "prompt_tokens_details": {
            "cached_tokens": cr,
            "cache_write_tokens": cw
        }
    });
    if reasoning > 0 {
        usage["completion_tokens_details"] = json!({ "reasoning_tokens": reasoning });
    }
    usage
}

fn is_heartbeat(data: &[u8]) -> bool {
    let ms = proto::fields(data);
    !ms.is_empty()
        && ms.iter().all(|m| {
            m.num == 1
                && proto::fields(&m.data)
                    .iter()
                    .all(|u| u.num == 13)
        })
}

pub fn forward(body: Value, _params: &Value, account: &Value) -> Result<()> {
    let access = account["accessToken"]
        .as_str()
        .ok_or_else(|| Error::http(401, "Cursor 未登录"))?;
    if account["expiresAt"].as_i64().unwrap_or(0) <= sdk::now() {
        let refreshed = auth::credentials(true)?;
        return forward(body, _params, &refreshed);
    }

    let model_raw = body["model"].as_str().unwrap_or("claude-sonnet-4-6");
    let (resolved, catalog_max) = models::resolve_run(model_raw);
    let model = if resolved.is_empty() {
        run_model_id(model_raw)
    } else {
        resolved
    };
    let tools = tools_of(&body);
    let (msgs, last, _estimate) = conversation_blobs(&body, &tools)?;
    let (body_max, params) = model_params(&body);
    let mut max_mode = body_max || catalog_max;
    let conv = conversation_id(&body);

    let version = config::client_version()?;
    let chat_id = client::uuid_v4()?;

    // Match halo-codex: open the host SSE response before any upstream work so
    // failures surface as stream/proxy errors instead of "插件尚未发送响应头".
    sdk::stream_head(200)?;
    let mut out = StreamOut {
        model: model_raw,
        id: format!("chatcmpl-{}", &chat_id[..8.min(chat_id.len())]),
        tool_index: 0,
    };
    out.emit(json!({"role":"assistant"}), Value::Null, None)?;

    let mut first_attempt = true;
    let (mut first, blobs) = build_run(&msgs, &last, &tools, &model, &conv, max_mode, &params);

    // Open RunSSE with request id envelope. Retry once in Max Mode when Cursor requires it.
    let mut response;
    let mut seq;
    let mut request_id;
    let mut headers;
    loop {
        request_id = client::uuid_v4()?;
        headers = json!({
            "content-type": "application/connect+proto",
            "connect-protocol-version": "1",
            "authorization": format!("Bearer {access}"),
            "x-cursor-client-version": version,
            "x-cursor-client-type": "cli",
            "x-ghost-mode": "true",
            "x-request-id": request_id,
            "x-cursor-streaming": "true",
            "accept-encoding": "identity",
            // Magpie/cursor-agent: comma-separated ToolCall oneof names, not JSON.
            "x-cursor-agent-allowed-tools": "mcp_tool_call,get_mcp_tools_tool_call",
        });
        if let Some(team) = config::team_id()? {
            headers["x-cursor-team-id"] = json!(team);
        }

        let open_body = proto::frame(&Pb::new().str(1, &request_id).done());
        response = http::open_raw_timeout(
            "POST",
            &format!("{}/agent.v1.AgentService/RunSSE", client::API),
            headers.clone(),
            Some(&open_body),
            120_000,
        )?;
        let run_status = response.status;
        if !(200..300).contains(&run_status) {
            let text = String::from_utf8_lossy(&response.bytes(1 << 20)?).into_owned();
            if first_attempt && !max_mode && max_mode_required(&text) {
                max_mode = true;
                first_attempt = false;
                let rebuilt = build_run(&msgs, &last, &tools, &model, &conv, true, &params);
                first = rebuilt.0;
                continue;
            }
            let class = errors::classify(run_status, &json!({ "message": text }));
            return Err(Error::http(
                class["status"].as_u64().unwrap_or(run_status as u64) as u16,
                class["message"].as_str().unwrap_or("Cursor RunSSE 失败"),
            ));
        }

        seq = 0u64;
        match bidi_append(&headers, &request_id, &first, seq) {
            Ok(()) => {
                seq += 1;
                break;
            }
            Err(err) if first_attempt && !max_mode && max_mode_required(&err.message) => {
                max_mode = true;
                first_attempt = false;
                let rebuilt = build_run(&msgs, &last, &tools, &model, &conv, true, &params);
                first = rebuilt.0;
                let _ = response;
                continue;
            }
            Err(err) => return Err(err),
        }
    }

    let mut decoder = FrameDecoder::new();

    let mut last_beat = sdk::now();
    let mut last_frame = sdk::now();
    let mut wrote = 0usize;
    let mut calls = 0usize;
    let mut finished = false;
    let mut usage = Value::Null;

    let send = |msg: &[u8], seq: &mut u64, headers: &Value, request_id: &str| -> Result<()> {
        bidi_append(headers, request_id, msg, *seq)?;
        *seq += 1;
        Ok(())
    };

    loop {
        let now = sdk::now();
        if now.saturating_sub(last_frame) > STALL_MS {
            return Err(Error::http(504, "Cursor 对话长时间无响应（stall）"));
        }
        if now.saturating_sub(last_beat) >= HEARTBEAT_MS {
            let hb = Pb::new().bytes(7, &[]).done();
            send(&hb, &mut seq, &headers, &request_id)?;
            last_beat = now;
        }

        let chunk = match response.read()? {
            Some(bytes) if !bytes.is_empty() => bytes,
            Some(_) => continue,
            None => break,
        };

        let frames = decoder
            .push(&chunk)
            .map_err(|e| Error::http(502, e))?;
        for frame in frames {
            if !is_heartbeat(&frame.data) {
                last_frame = sdk::now();
            }
            if frame.end {
                if wrote == 0 && calls == 0 {
                    let text = String::from_utf8_lossy(&frame.data);
                    let class = errors::classify(502, &json!({ "message": text }));
                    if class["kind"] != "fatal"
                        || text.contains("\"error\"")
                        || text.contains("code")
                    {
                        let parsed: Value =
                            serde_json::from_slice(&frame.data).unwrap_or(json!({ "message": text }));
                        let class = errors::classify(200, &parsed);
                        if class["kind"] != "fatal"
                            || parsed.pointer("/error").is_some()
                            || parsed.get("code").is_some()
                        {
                            let status = class["status"].as_u64().unwrap_or(502) as u16;
                            if !(200..300).contains(&status)
                                || parsed.pointer("/error").is_some()
                            {
                                return Err(Error::http(
                                    if (200..300).contains(&status) {
                                        502
                                    } else {
                                        status
                                    },
                                    format!(
                                        "{} (model={model})",
                                        class["message"].as_str().unwrap_or("Cursor 对话失败")
                                    ),
                                ));
                            }
                        }
                    }
                    return Err(Error::http(
                        502,
                        format!("Cursor 返回空回复 (model={model})"),
                    ));
                }
                let finish = if calls > 0 { "tool_calls" } else { "stop" };
                out.emit(
                    json!({}),
                    json!(finish),
                    if usage.is_null() { None } else { Some(usage.clone()) },
                )?;
                sdk::stream_bytes(b"data: [DONE]\n\n")?;
                finished = true;
                break;
            }

            for m in proto::fields(&frame.data) {
                match m.num {
                    1 => {
                        for u in proto::fields(&m.data) {
                            let uf = proto::fields(&u.data);
                            match u.num {
                                1 => {
                                    let t = proto::pb_str(&uf, 1);
                                    if !t.is_empty() {
                                        wrote += t.len();
                                        out.emit(json!({"content": t}), Value::Null, None)?;
                                    }
                                }
                                4 => {
                                    let t = proto::pb_str(&uf, 1);
                                    if !t.is_empty() {
                                        out.emit(
                                            json!({"reasoning_content": t}),
                                            Value::Null,
                                            None,
                                        )?;
                                    }
                                }
                                14 => {
                                    usage = usage_of(&uf, 0);
                                    if calls == 0 {
                                        if wrote == 0 {
                                            return Err(Error::http(502, "Cursor 结束回合但无正文"));
                                        }
                                        out.emit(json!({}), json!("stop"), Some(usage.clone()))?;
                                        sdk::stream_bytes(b"data: [DONE]\n\n")?;
                                        finished = true;
                                    }
                                }
                                31 => {
                                    if uf.iter().any(|f| (2..=6).contains(&f.num)) {
                                        usage = usage_of(&uf, 1);
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                    2 => {
                        if let Some(call) = handle_exec(&m.data, &tools, &mut seq, &headers, &request_id)?
                        {
                            let index = out.tool_index;
                            out.tool_index += 1;
                            calls += 1;
                            out.emit(
                                json!({"tool_calls":[{
                                    "index": index,
                                    "id": call["id"],
                                    "type": "function",
                                    "function": {
                                        "name": call["name"],
                                        "arguments": call["arguments"]
                                    }
                                }]}),
                                Value::Null,
                                None,
                            )?;
                        }
                    }
                    4 => {
                        handle_kv(&m.data, &blobs, &mut seq, &headers, &request_id)?;
                    }
                    _ => {}
                }
                if finished {
                    break;
                }
            }
            if finished {
                break;
            }
            // After tool calls are emitted, end the turn for the host to run tools.
            if calls > 0 {
                out.emit(
                    json!({}),
                    json!("tool_calls"),
                    if usage.is_null() {
                        None
                    } else {
                        Some(usage.clone())
                    },
                )?;
                sdk::stream_bytes(b"data: [DONE]\n\n")?;
                finished = true;
                break;
            }
        }
        if finished {
            break;
        }
    }

    if !finished {
        if calls > 0 {
            out.emit(json!({}), json!("tool_calls"), None)?;
            sdk::stream_bytes(b"data: [DONE]\n\n")?;
        } else if wrote > 0 {
            out.emit(json!({}), json!("stop"), None)?;
            sdk::stream_bytes(b"data: [DONE]\n\n")?;
        } else {
            return Err(Error::http(502, "Cursor 回复中断（EOF）"));
        }
    }
    Ok(())
}

fn handle_kv(
    data: &[u8],
    blobs: &BTreeMap<String, Vec<u8>>,
    seq: &mut u64,
    headers: &Value,
    request_id: &str,
) -> Result<()> {
    let kv = proto::fields(data);
    let id = proto::pb_num(&kv, 1);
    for k in &kv {
        if k.num == 2 {
            let want = proto::field(&proto::fields(&k.data), 1)
                .map(|f| hex::encode(&f.data))
                .unwrap_or_default();
            let res = if let Some(b) = blobs.get(&want) {
                Pb::new().bytes(1, b).done()
            } else {
                Pb::new()
                    .nested(2, Pb::new().str(1, "blob not found"))
                    .done()
            };
            let msg = Pb::new()
                .nested(3, Pb::new().varint(1, id).bytes(2, &res))
                .done();
            bidi_append(headers, request_id, &msg, *seq)?;
            *seq += 1;
        } else if k.num == 3 {
            let msg = Pb::new()
                .nested(3, Pb::new().varint(1, id).bytes(3, &[]))
                .done();
            bidi_append(headers, request_id, &msg, *seq)?;
            *seq += 1;
        }
    }
    Ok(())
}

fn handle_exec(
    data: &[u8],
    tools: &[ToolDef],
    seq: &mut u64,
    headers: &Value,
    request_id: &str,
) -> Result<Option<Value>> {
    let es = proto::fields(data);
    let id = proto::pb_num(&es, 1);
    let exec_id = proto::pb_str(&es, 15);

    let close_exec = |seq: &mut u64| -> Result<()> {
        let msg = Pb::new()
            .nested(5, Pb::new().nested(1, Pb::new().varint(1, id)))
            .done();
        bidi_append(headers, request_id, &msg, *seq)?;
        *seq += 1;
        Ok(())
    };

    let unavailable = |seq: &mut u64| -> Result<Option<Value>> {
        let msg = Pb::new()
            .nested(
                5,
                Pb::new().nested(2, Pb::new().varint(1, id).str(2, "not available")),
            )
            .done();
        bidi_append(headers, request_id, &msg, *seq)?;
        *seq += 1;
        Ok(None)
    };

    for e in &es {
        match e.num {
            11 => {
                let a = proto::fields(&e.data);
                let mut args = serde_json::Map::new();
                for kv in &a {
                    if kv.num != 2 {
                        continue;
                    }
                    let ef = proto::fields(&kv.data);
                    let key = proto::pb_str(&ef, 1);
                    let value = proto::field(&ef, 2)
                        .map(|v| proto::pb_any(&v.data))
                        .unwrap_or(Value::Null);
                    args.insert(key, value);
                }
                let name = {
                    let bare = proto::pb_str(&a, 5);
                    if bare.is_empty() {
                        let full = proto::pb_str(&a, 1);
                        full.strip_prefix(&format!("{NS}-"))
                            .unwrap_or(&full)
                            .to_string()
                    } else {
                        bare
                    }
                };
                if !tools.iter().any(|t| t.name == name) {
                    return unavailable(seq);
                }
                let cid = {
                    let raw = proto::pb_str(&a, 3).replace('\n', "__");
                    if raw.is_empty() {
                        format!("call_{}", hex::encode(client::random_bytes(12)?))
                    } else {
                        raw
                    }
                };
                let tool_args = args
                    .get("arguments")
                    .cloned()
                    .unwrap_or_else(|| Value::Object(args.clone()));
                let tool_name = args
                    .get("toolName")
                    .and_then(Value::as_str)
                    .unwrap_or(&name)
                    .to_string();
                return Ok(Some(json!({
                    "id": cid,
                    "name": tool_name,
                    "arguments": tool_args.to_string()
                })));
            }
            36 => {
                let mut srv = Pb::new().str(1, NS).str(2, NS).str(7, "connected");
                for t in tools {
                    srv = srv.bytes(5, &tool_def_bytes(t));
                }
                let res = Pb::new().nested(1, Pb::new().nested(1, srv)).done();
                let msg = Pb::new()
                    .nested(
                        2,
                        Pb::new()
                            .varint(1, id)
                            .str(15, &exec_id)
                            .bytes(36, &res),
                    )
                    .done();
                bidi_append(headers, request_id, &msg, *seq)?;
                *seq += 1;
                close_exec(seq)?;
                return Ok(None);
            }
            10 => {
                let res = Pb::new()
                    .nested(
                        1,
                        Pb::new().nested(1, Pb::new().bytes(4, &env_bytes())),
                    )
                    .done();
                let msg = Pb::new()
                    .nested(
                        2,
                        Pb::new()
                            .varint(1, id)
                            .str(15, &exec_id)
                            .bytes(10, &res),
                    )
                    .done();
                bidi_append(headers, request_id, &msg, *seq)?;
                *seq += 1;
                close_exec(seq)?;
                return Ok(None);
            }
            _ => {}
        }
    }
    unavailable(seq)
}
