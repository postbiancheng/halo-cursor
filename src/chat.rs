//! Cursor Agent RunSSE + BidiAppend chat forward (HTTP/1.1, Magpie-aligned).

use super::{
    auth, client, config, errors, models,
    proto::{self, Field, FrameDecoder, Pb},
};
use agent2api_wasm_sdk::{self as sdk, http, json, Error, Result, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

const NS: &str = "tools";
const CALL: &str = "CallDynamicTool";
const NO_RESULT: &str = "Tool use was interrupted and did not produce a result.";
const HEARTBEAT_MS: i64 = 5_000;
const FIRST_OUTPUT_MS: i64 = 60_000;
const STALL_MS: i64 = 90_000;
const TOOL_COUNT_WAIT_MS: i64 = 30_000;
const APPEND_TIMEOUT_MS: u64 = 60_000;
const MAX_REQUEST_BYTES: usize = 4 * 1024 * 1024;
/// Agent2API's reqwest/rustls RunSSE path is often peer-closed around ~30s;
/// Magpie's Node fetch to the same endpoint is not. Soft-continue stitches a
/// new RunSSE onto the same client stream when that transport cut happens.
const MAX_STREAM_CONTINUES: u32 = 16;
const CONTINUE_USER: &str = "请从断点处继续输出，不要重复已经写过的内容。";

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

fn conversation_blobs(body: &Value, tools: &[ToolDef]) -> Result<(Vec<Vec<u8>>, String)> {
    let messages = body["messages"]
        .as_array()
        .ok_or_else(|| Error::http(400, "缺少 messages"))?;
    let mut out: Vec<Vec<u8>> = Vec::new();
    let mut last = ".".to_string();
    let mut system = Vec::new();
    for m in messages {
        if m["content"].as_array().is_some_and(|parts| {
            parts.iter().any(|part| {
                !matches!(
                    part["type"].as_str(),
                    None | Some("text" | "input_text" | "output_text")
                )
            })
        }) {
            return Err(Error::http(400, "Halo Cursor 仅支持文本消息内容"));
        }
        let role = m["role"].as_str().unwrap_or("");
        if role == "system" || role == "developer" {
            system.push(text_of(&m["content"]));
        }
    }
    let sys = {
        let mut s = system
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n");
        s.push_str(&catalog(tools));
        s
    };
    if !sys.is_empty() {
        out.push(serde_json::to_vec(&json!({"role":"system","content":sys})).unwrap_or_default());
    }

    let mut pending: Vec<String> = Vec::new();
    let mut results: Vec<Value> = Vec::new();
    let flush_results =
        |pending: &mut Vec<String>, results: &mut Vec<Value>, out: &mut Vec<Vec<u8>>| {
            for id in pending.drain(..) {
                results.push(tool_result(&id, NO_RESULT, true));
            }
            if !results.is_empty() {
                out.push(
                    serde_json::to_vec(&json!({"role":"tool","content":results.clone()}))
                        .unwrap_or_default(),
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
                }
                let text = text_of(&m["content"]);
                if !text.is_empty() {
                    content.push(json!({"type":"text","text":text}));
                }
                for call in m["tool_calls"].as_array().into_iter().flatten() {
                    let name = call["function"]["name"].as_str().unwrap_or("");
                    let args_raw = call["function"]["arguments"].as_str().unwrap_or("{}");
                    let args = parse_args(args_raw);

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
                    }
                    Value::Array(parts) => {
                        for p in parts {
                            match p["type"].as_str().unwrap_or("text") {
                                "text" | "input_text" | "output_text" => {
                                    if let Some(t) = p["text"].as_str() {
                                        content.push(json!({"type":"text","text":t}));
                                        if first.is_empty() {
                                            first = t.to_string();
                                        }
                                    }
                                }
                                _ => {
                                    return Err(Error::http(400, "Halo Cursor 仅支持文本消息内容"))
                                }
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
    Ok((out, last))
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
    Pb::new()
        .str(1, "macos")
        .str(2, "/tmp")
        .str(10, "UTC")
        .done()
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
) -> Result<(Vec<u8>, BTreeMap<String, Vec<u8>>)> {
    let mut blobs = BTreeMap::new();
    let mut state = Pb::new();
    for m in msgs {
        let id = put_blob(&mut blobs, m.clone());
        state = state.bytes(1, &id);
    }
    let mid = client::uuid_v4()?;
    let user = Pb::new().str(1, last_user).str(2, &mid).varint(4, 1).done();
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
        client::uuid_v4()?
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
    Ok((first, blobs))
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
        .map(|m| m.split(['@', '-']).any(|part| part == "1m"))
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

pub(crate) fn bidi_append(headers: &Value, request_id: &str, msg: &[u8], seq: u64) -> Result<()> {
    let body = Pb::new()
        .str(1, &hex::encode(msg))
        .nested(2, Pb::new().str(1, request_id))
        .varint(3, seq)
        .done();
    let mut h = headers.clone();
    h["content-type"] = json!("application/proto");
    if let Some(values) = h.as_object_mut() {
        values.remove("connect-timeout-ms");
        values.remove("connect-accept-encoding");
    }
    let unconfirmed = |mut error: Error| {
        error.message = format!("Cursor BidiAppend 确认失败：{}", error.message);
        error.code = Some("cursor_append_unconfirmed".into());
        error
    };
    let response = http::open_raw_timeout(
        "POST",
        &format!("{}/aiserver.v1.BidiService/BidiAppend", client::API),
        h,
        Some(&body),
        APPEND_TIMEOUT_MS,
    )
    .map_err(unconfirmed)?;
    if !(200..300).contains(&response.status) {
        return Err(errors::http_response("BidiAppend", response));
    }
    response.bytes(64 << 10).map_err(unconfirmed)?;
    Ok(())
}

struct StreamOut<'a> {
    model: &'a str,
    id: String,
    started: bool,
    created: i64,
    text: String,
    tools_started: bool,
}

impl StreamOut<'_> {
    fn delta(&mut self, delta: Value) -> Result<()> {
        if !self.started {
            self.emit(json!({"role":"assistant"}), Value::Null, None)?;
            self.started = true;
        }
        if let Some(text) = delta["content"].as_str() {
            self.text.push_str(text);
        }
        if delta.get("tool_calls").is_some() {
            self.tools_started = true;
        }
        self.emit(delta, Value::Null, None)
    }

    fn finish(&self, tools: bool, usage: Option<Value>) -> Result<()> {
        self.emit(
            json!({}),
            json!(if tools { "tool_calls" } else { "stop" }),
            usage,
        )?;
        sdk::stream_bytes(b"data: [DONE]\n\n")
    }

    fn emit(&self, delta: Value, finish: Value, usage: Option<Value>) -> Result<()> {
        let mut frame = json!({
            "id": self.id,
            "object": "chat.completion.chunk",
            "model": self.model,
            "created": self.created,
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
    let prompt = input.saturating_add(cr).saturating_add(cw);
    let mut usage = json!({
        "prompt_tokens": prompt,
        "completion_tokens": out,
        "total_tokens": prompt.saturating_add(out),
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

fn is_heartbeat(messages: &[Field]) -> bool {
    !messages.is_empty()
        && messages.iter().all(|message| {
            let updates = proto::fields(&message.data);
            message.num == 1 && !updates.is_empty() && updates.iter().all(|update| update.num == 13)
        })
}

/// Host reqwest/rustls cuts that Magpie (Node fetch) does not see on the same API.
fn is_resumable_transport_cut(error: &Error) -> bool {
    let message = error.message.to_ascii_lowercase();
    [
        "peer closed",
        "error decoding response body",
        "incomplete chunked",
        "connection reset",
        "connection interrupted",
        "tls close_notify",
        "unexpected eof",
    ]
    .iter()
    .any(|part| message.contains(part))
}

fn push_continue_turn(messages: &mut Vec<Vec<u8>>, partial: &str) -> Result<()> {
    messages.push(
        serde_json::to_vec(&json!({
            "role": "assistant",
            "content": [{"type": "text", "text": partial}]
        }))
        .map_err(|_| Error::http(500, "续写上下文编码失败"))?,
    );
    messages.push(
        serde_json::to_vec(&json!({
            "role": "user",
            "content": [{"type": "text", "text": CONTINUE_USER}]
        }))
        .map_err(|_| Error::http(500, "续写上下文编码失败"))?,
    );
    Ok(())
}

enum AttemptEnd {
    Done,
    Continue,
}

pub fn forward(body: Value) -> Result<()> {
    if http::json_exceeds(&body, MAX_REQUEST_BYTES)? {
        return Err(Error::http(413, "Halo Cursor 请求正文超过 4 MiB"));
    }
    let tools = if body["tool_choice"] == "none" {
        Vec::new()
    } else {
        tools_of(&body)
    };
    let (mut messages, mut last) = conversation_blobs(&body, &tools)?;
    let model_raw = body["model"].as_str().unwrap_or("claude-sonnet-4-6");
    let conv = conversation_id(&body);
    sdk::stream_head(200)?;
    let (body_max, params) = model_params(&body);
    let account = auth::credentials(false)?;
    let access = account["accessToken"]
        .as_str()
        .filter(|token| !token.is_empty())
        .ok_or_else(|| Error::http(401, "Cursor 未登录"))?;

    let (model, catalog_max) = models::resolve_run(model_raw, &account);
    let mut max_mode = body_max || catalog_max;
    let mut headers = cursor_stream_headers(access)?;
    let mut out = StreamOut {
        model: model_raw,
        id: format!("chatcmpl-{}", client::uuid_v4()?),
        started: false,
        created: sdk::now() / 1000,
        text: String::new(),
        tools_started: false,
    };
    let mut continues = 0_u32;
    let mut continued_from = 0usize;

    loop {
        let request_id = client::uuid_v4()?;
        headers["x-request-id"] = json!(&request_id);
        let (first, blobs) = build_run(&messages, &last, &tools, &model, &conv, max_mode, &params)?;
        let result = run_attempt(&mut out, &headers, &request_id, &first, &blobs, &tools);
        match result {
            Err(error)
                if !max_mode
                    && !out.started
                    && error.code.as_deref() == Some("cursor_max_mode_required") =>
            {
                max_mode = true;
            }
            Ok(AttemptEnd::Continue)
                if !out.tools_started
                    && !out.text.is_empty()
                    && continues < MAX_STREAM_CONTINUES =>
            {
                let partial = out.text[continued_from..].to_string();
                if partial.trim().is_empty() {
                    return Err(Error::http(502, "Cursor 流已输出后中断：无可续写正文")
                        .with_code("cursor_partial_stream"));
                }
                push_continue_turn(&mut messages, &partial)?;
                last = CONTINUE_USER.to_string();
                continued_from = out.text.len();
                continues += 1;
            }
            Ok(AttemptEnd::Done) => return Ok(()),
            Ok(AttemptEnd::Continue) => {
                return Err(Error::http(
                    502,
                    format!(
                        "Cursor 流已输出后中断：传输层切断且续写已用尽（{continues} 次）"
                    ),
                )
                .with_code("cursor_partial_stream"));
            }
            Err(mut error) if out.started && error.status != Some(499) => {
                error.message = format!("Cursor 流已输出后中断：{}", error.message);
                error.code = Some("cursor_partial_stream".into());
                return Err(error);
            }
            Err(error) => return Err(error),
        }
    }
}

fn cursor_stream_headers(access: &str) -> Result<Value> {
    // Magpie/cursor-agent RunSSE headers: no connect-timeout-ms. Host-side
    // timeout_ms would also install a hard total deadline on the download;
    // long streams rely on stream-idle instead (see open_raw below).
    let mut headers = json!({
        "content-type": "application/connect+proto",
        "connect-protocol-version": "1",
        "authorization": format!("Bearer {access}"),
        "x-cursor-client-version": config::client_version()?,
        "x-cursor-client-type": "cli",
        "x-ghost-mode": "true",
        "x-cursor-streaming": "true",
        "accept-encoding": "identity",
        "x-cursor-agent-allowed-tools": "mcp_tool_call,get_mcp_tools_tool_call",
    });
    if let Some(team) = config::team_id()? {
        headers["x-cursor-team-id"] = json!(team);
    }
    Ok(headers)
}

#[derive(Default)]
struct ToolBatch {
    expected: u64,
    emitted: BTreeSet<String>,
    answered: u64,
    writing: BTreeSet<String>,
    started: BTreeSet<String>,
    completed: BTreeSet<String>,
}

impl ToolBatch {
    fn update(&mut self, kind: u32, id: String) {
        if id.is_empty() {
            return;
        }
        match kind {
            2 => {
                self.started.insert(id);
            }
            3 if self.writing.contains(&id)
                && !self.started.contains(&id)
                && !self.emitted.contains(&id) =>
            {
                self.completed.insert(id);
            }
            7 => {
                self.writing.insert(id);
            }
            _ => {}
        }
    }

    fn ready(&mut self) -> bool {
        let count = self.emitted.len() as u64 + self.answered + self.completed.len() as u64;
        if self.expected == 0 || count < self.expected {
            return false;
        }
        if self.emitted.is_empty() {
            *self = Self::default();
            return false;
        }
        true
    }

    fn quiet_without_count(&self, elapsed: i64) -> bool {
        self.expected == 0
            && !self.emitted.is_empty()
            && elapsed >= TOOL_COUNT_WAIT_MS
            && self.writing.iter().all(|id| {
                self.started.contains(id)
                    || self.emitted.contains(id)
                    || self.completed.contains(id)
            })
    }

    fn incomplete(&self) -> bool {
        self.expected > 0
            && self.emitted.len() as u64 + self.answered + (self.completed.len() as u64)
                < self.expected
    }
}

fn run_attempt(
    out: &mut StreamOut<'_>,
    headers: &Value,
    request_id: &str,
    first: &[u8],
    blobs: &BTreeMap<String, Vec<u8>>,
    tools: &[ToolDef],
) -> Result<AttemptEnd> {
    let open_body = proto::frame(&Pb::new().str(1, request_id).done());
    // Do not pass timeout_ms: Agent2API treats it as a hard total deadline on
    // the upstream download. Magpie's RunSSE fetch likewise has no wall-clock
    // AbortSignal; longevity is gated by stream-idle + our own stall checks.
    let mut response = http::open_raw(
        "POST",
        &format!("{}/agent.v1.AgentService/RunSSE", client::API),
        headers.clone(),
        Some(&open_body),
    )
    .map_err(|mut error| {
        error.message = format!("Cursor RunSSE 建连失败：{}", error.message);
        error
    })?;
    if !(200..300).contains(&response.status) {
        return Err(errors::http_response("RunSSE", response));
    }

    bidi_append(headers, request_id, first, 0)?;
    let mut seq = 1;
    let mut decoder = FrameDecoder::new(
        response
            .header_values("connect-content-encoding")
            .first()
            .copied()
            .unwrap_or("identity"),
    );
    let started_at = sdk::now();
    let mut last_beat = started_at;
    let mut last_activity = started_at;
    let mut heartbeats = 0_u32;
    let mut turn_ended = false;
    let mut wrote = false;
    let mut usage = None;
    let mut batch = ToolBatch::default();
    let resumable =
        |error: Error, wrote: bool, tools_started: bool| -> Result<AttemptEnd> {
            if wrote && !tools_started && is_resumable_transport_cut(&error) {
                Ok(AttemptEnd::Continue)
            } else {
                Err(error)
            }
        };

    loop {
        let now = sdk::now();
        let idle = now.saturating_sub(last_activity);
        if !decoder.has_partial_frame() && batch.quiet_without_count(idle) {
            out.finish(true, usage)?;
            return Ok(AttemptEnd::Done);
        }
        if !out.started && now.saturating_sub(started_at) >= FIRST_OUTPUT_MS {
            return Err(Error::http(504, "Cursor 流等待首个有效输出超时")
                .with_code("cursor_first_output_timeout"));
        }
        if idle >= STALL_MS {
            return Err(
                Error::http(504, "Cursor 流长时间没有有效事件").with_code("cursor_stream_idle")
            );
        }
        if now.saturating_sub(last_beat) >= HEARTBEAT_MS {
            bidi_append(headers, request_id, &Pb::new().bytes(7, &[]).done(), seq)?;
            seq += 1;
            last_beat = sdk::now();
            heartbeats += 1;
        }

        let chunk = match response.read() {
            Ok(chunk) => chunk,
            Err(mut error) => {
                let elapsed = sdk::now().saturating_sub(started_at);
                let turn = if turn_ended {
                    "已收到回合结束"
                } else {
                    "未收到回合结束"
                };
                error.message = format!(
                    "Cursor RunSSE 读取失败（{elapsed} ms，{heartbeats} 次上行心跳，{turn}）：{}",
                    error.message
                );
                return resumable(error, wrote, out.tools_started || !batch.emitted.is_empty());
            }
        };
        let Some(chunk) = chunk else {
            let detail = if decoder.has_partial_frame() {
                "Connect 帧未收全"
            } else {
                "缺少结束帧"
            };
            let error = Error::http(502, format!("Cursor 回复中断（EOF，{detail}）"))
                .with_code("cursor_stream_eof");
            return resumable(error, wrote, out.tools_started || !batch.emitted.is_empty());
        };
        if chunk.is_empty() {
            continue;
        }

        let frames = decoder
            .push(&chunk)
            .map_err(|error| Error::http(502, error).with_code("cursor_connect_protocol"))?;
        let mut frames = frames.into_iter().peekable();
        let mut checkpoint_ready = false;
        while let Some(frame) = frames.next() {
            if frame.end {
                if frames.peek().is_some() || decoder.has_partial_frame() {
                    return Err(Error::http(502, "Cursor Connect 结束帧后仍有数据"));
                }
                errors::connect_end(&frame.data)?;
                if batch.incomplete() {
                    return Err(Error::http(502, "Cursor 工具调用批次未收全")
                        .with_code("cursor_tool_batch_incomplete"));
                }
                if !wrote && batch.emitted.is_empty() {
                    return Err(
                        Error::http(502, "Cursor 返回空回复").with_code("cursor_empty_reply")
                    );
                }
                out.finish(!batch.emitted.is_empty(), usage)?;
                return Ok(AttemptEnd::Done);
            }

            let messages =
                proto::checked_fields(&frame.data).map_err(|error| Error::http(502, error))?;
            if !is_heartbeat(&messages) {
                last_activity = sdk::now();
            }
            for message in messages {
                match message.num {
                    1 => {
                        for update in proto::checked_fields(&message.data)
                            .map_err(|error| Error::http(502, error))?
                        {
                            let fields = proto::checked_fields(&update.data)
                                .map_err(|error| Error::http(502, error))?;
                            match update.num {
                                1 | 4 => {
                                    let text = proto::pb_str(&fields, 1);
                                    if !text.is_empty() {
                                        wrote |= update.num == 1;
                                        out.delta(if update.num == 1 {
                                            json!({"content":text})
                                        } else {
                                            json!({"reasoning_content":text})
                                        })?;
                                    }
                                }
                                2 | 3 | 7 => batch.update(
                                    update.num,
                                    proto::pb_str(&fields, 1).replace('\n', "__"),
                                ),
                                14 => {
                                    turn_ended = true;
                                    usage = Some(usage_of(&fields, 0));
                                }
                                27 => batch.expected = proto::pb_num(&fields, 1),
                                31 if fields.iter().any(|f| (2..=6).contains(&f.num)) => {
                                    usage = Some(usage_of(&fields, 1))
                                }
                                _ => {}
                            }
                        }
                    }
                    2 => {
                        let exec = proto::checked_fields(&message.data)
                            .map_err(|error| Error::http(502, error))?;
                        if let Some(call) =
                            handle_exec(&exec, tools, &mut seq, headers, request_id)?
                        {
                            let index = batch.emitted.len();
                            let id = call["id"].as_str().unwrap_or_default().to_string();
                            if batch.emitted.insert(id) {
                                out.delta(json!({"tool_calls":[{
                                    "index": index,
                                    "id": call["id"],
                                    "type":"function",
                                    "function":{"name":call["name"],"arguments":call["arguments"]}
                                }]}))?;
                            }
                        } else if !exec.iter().any(|field| field.num == 10) {
                            batch.answered += 1;
                        }
                    }
                    3 if batch.expected > 0 && !batch.emitted.is_empty() => {
                        let pending = proto::checked_fields(&message.data)
                            .map_err(|error| Error::http(502, error))?
                            .into_iter()
                            .filter(|field| field.num == 4)
                            .count();
                        checkpoint_ready = pending == batch.emitted.len();
                    }
                    4 => handle_kv(&message.data, blobs, &mut seq, headers, request_id)?,
                    _ => {}
                }
                // A batch answered inside the plugin clears its count before the next step.
                if batch.emitted.is_empty() {
                    batch.ready();
                }
            }
        }
        if !decoder.has_partial_frame() && (batch.ready() || checkpoint_ready) {
            // Cursor waits for tool results once the declared batch has arrived.
            // The caller supplies results in a new request.
            out.finish(true, usage)?;
            return Ok(AttemptEnd::Done);
        }
    }
}

fn handle_kv(
    data: &[u8],
    blobs: &BTreeMap<String, Vec<u8>>,
    seq: &mut u64,
    headers: &Value,
    request_id: &str,
) -> Result<()> {
    let kv = proto::checked_fields(data).map_err(|error| Error::http(502, error))?;
    let id = proto::pb_num(&kv, 1);
    for k in &kv {
        if k.num == 2 {
            let fields = proto::checked_fields(&k.data).map_err(|error| Error::http(502, error))?;
            let want = proto::field(&fields, 1)
                .map(|field| hex::encode(&field.data))
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
    es: &[Field],
    tools: &[ToolDef],
    seq: &mut u64,
    headers: &Value,
    request_id: &str,
) -> Result<Option<Value>> {
    let id = proto::pb_num(es, 1);
    let exec_id = proto::pb_str(es, 15);

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
        close_exec(seq)?;
        Ok(None)
    };

    for e in es {
        match e.num {
            11 => {
                let a = proto::checked_fields(&e.data).map_err(|error| Error::http(502, error))?;
                let mut args = serde_json::Map::new();
                for kv in &a {
                    if kv.num != 2 {
                        continue;
                    }
                    let ef =
                        proto::checked_fields(&kv.data).map_err(|error| Error::http(502, error))?;
                    let key = proto::pb_str(&ef, 1);
                    let value = match proto::field(&ef, 2) {
                        Some(value) => proto::pb_any(&value.data)
                            .ok_or_else(|| Error::http(502, "Cursor 返回无效工具参数"))?,
                        None => Value::Null,
                    };
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
                let cid = {
                    let raw = proto::pb_str(&a, 3).replace('\n', "__");
                    if raw.is_empty() {
                        format!("call_{}", hex::encode(client::random_bytes(12)?))
                    } else {
                        raw
                    }
                };
                let (tool_name, tool_args) =
                    if name == CALL && !tools.iter().any(|tool| tool.name == CALL) {
                        let wrapper = Value::Object(args);
                        if wrapper["namespace"]
                            .as_str()
                            .is_some_and(|namespace| namespace != NS)
                        {
                            return unavailable(seq);
                        }
                        let Some(tool_name) = wrapper["toolName"].as_str() else {
                            return unavailable(seq);
                        };
                        let tool_args = wrapper["arguments"]
                            .as_object()
                            .ok_or_else(|| Error::http(502, "Cursor 返回无效工具参数"))?;
                        (tool_name.to_string(), Value::Object(tool_args.clone()))
                    } else {
                        (name, Value::Object(args))
                    };
                if !tools.iter().any(|tool| tool.name == tool_name) {
                    return unavailable(seq);
                }
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
                    .nested(2, Pb::new().varint(1, id).str(15, &exec_id).bytes(36, &res))
                    .done();
                bidi_append(headers, request_id, &msg, *seq)?;
                *seq += 1;
                close_exec(seq)?;
                return Ok(None);
            }
            10 => {
                let res = Pb::new()
                    .nested(1, Pb::new().nested(1, Pb::new().bytes(4, &env_bytes())))
                    .done();
                let msg = Pb::new()
                    .nested(2, Pb::new().varint(1, id).str(15, &exec_id).bytes(10, &res))
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
