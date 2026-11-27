import assert from "node:assert/strict"
import { readFileSync } from "node:fs"
import { test } from "node:test"
import { gzipSync } from "node:zlib"

const wasm = new WebAssembly.Module(readFileSync(process.argv[2] ??
  "target/wasm32-unknown-unknown/release/agent2api_halo_cursor.wasm"))
const concat = (...parts) => Buffer.concat(parts)
function varint(value) {
  const bytes = []
  while (value >= 128) {
    bytes.push(value % 128 + 128)
    value = Math.floor(value / 128)
  }
  bytes.push(value)
  return Buffer.from(bytes)
}

function field(num, value) {
  if (typeof value === "number") return concat(varint(num * 8), varint(value))
  const bytes = Buffer.from(value)
  return concat(varint(num * 8 + 2), varint(bytes.length), bytes)
}
function frame(bytes, flags = 0) {
  const header = Buffer.alloc(5)
  header[0] = flags
  header.writeUInt32BE(bytes.length, 1)
  return concat(header, bytes)
}

const update = (num, value) => frame(field(1, field(num, value)))
const content = (text) => update(1, field(1, text))
const usage = update(14, concat(field(1, 4), field(2, 2)))
const toolCount = (count) => update(27, field(1, count))
const end = (data = {}) => frame(Buffer.from(JSON.stringify(data)), 2)
const failedEnd = end({error: {code: "unavailable", message: "test unavailable"}})
const maxRequired = {error: {code: "failed_precondition", details: [
  {debug: {details: {title: "Max Mode Required"}}},
]}}
const tool = (id) => frame(field(2, concat(field(1, 1), field(11, concat(field(3, id), field(5, "lookup"))))))

function requestLength(bytes) {
  assert.equal(bytes[0], 10)
  let value = 0
  let offset = 1
  for (let scale = 1; ; scale *= 128) {
    const byte = bytes[offset++]
    value += (byte & 127) * scale
    if (byte < 128) return [offset, value]
  }
}

function hostRun(scenario) {
  let instance
  let reply
  let random = 0
  let attempt = -1
  let tick = 0
  const streams = new Map()
  const scenarioAttempts = (scenario.attempts ?? []).map((config) => ({...config, chunks: [...config.chunks]}))
  let snapshot = scenario.snapshot ?? {revision: 0, schema: 1, compatible: true, value: null}
  const events = []
  const calls = []
  const sleeps = []
  const appended = []
  const memory = () => Buffer.from(instance.exports.memory.buffer)
  const base64 = (data) => Buffer.from(data).toString("base64")
  function call(pointer, length) {
    const {method, params} = JSON.parse(memory().subarray(pointer, pointer + length).toString())
    calls.push(method)
    let result = null
    let error
    switch (method) {
      case "account.get":
        result = {secret: {accessToken: "test-token", expiresAt: 2_000_000_000_000, identity: "cursor:test"}}
        if (scenario.accountError) error = scenario.accountError
        break
      case "clock.now":
        result = 1_700_000_000_000 + tick
        break
      case "clock.sleep":
        sleeps.push(params.milliseconds)
        tick += params.milliseconds
        break
      case "state.get":
        result = snapshot
        break
      case "state.compare_exchange":
        assert.equal(params.revision, snapshot.revision)
        snapshot = {...snapshot, value: params.value, revision: snapshot.revision + 1}
        result = {updated: true}
        break
      case "lock.acquire":
        result = "test-lock"
        break
      case "lock.release":
      case "catalog.get":
      case "stream.head":
      case "http.close":
        break
      case "settings.get":
        error = scenario.settingsError
        break
      case "random.bytes":
        result = base64(Buffer.alloc(params.length, ++random))
        break
      case "stream.chunk":
        if (scenario.cancelOnChunk && events.length === 1) {
          error = {message: "test cancel", status: 499}
          break
        }
        events.push(Buffer.from(params.data, "base64").toString())
        break
      case "http.open": {
        const path = new URL(params.url).pathname
        const configured = scenario.httpResponses?.[path]
        if (configured) {
          streams.set(path, configured.chunks ?? [])
          result = {status: configured.status ?? 200, id: path, headers: {}}
        } else if (path.endsWith("/RunSSE")) {
          assert.equal(params.headers["connect-timeout-ms"], undefined)
          assert.equal(params.timeout_ms, undefined)
          const config = scenarioAttempts[++attempt]
          assert.ok(config, "unexpected replay")
          const id = "run-" + attempt
          streams.set(id, config.chunks)
          result = {status: config.status ?? 200, id, headers: config.headers ?? {}}
        } else {
          assert.ok(params.url.endsWith("/BidiAppend"), "chat must not fetch model discovery")
          assert.equal(params.headers["connect-timeout-ms"], undefined)
          const config = scenarioAttempts[attempt]
          const bytes = Buffer.from(params.body, "base64")
          const [start, length] = requestLength(bytes)
          const message = Buffer.from(bytes.subarray(start, start + length).toString(), "hex")
          const id = "append-" + appended.length
          const initial = !appended.some((row) => row.attempt === attempt)
          appended.push({attempt, message})
          if (initial && config.appendTransportError) {
            error = {message: "test append connection interrupted", status: 502}
            break
          }
          if (initial && config.kvLookup) {
            const tag = config.kvLookup === "history" ? 0x0a : 0x42
            let state = message
            for (let i = 0; i < 2; i++) {
              const [start, length] = requestLength(state)
              state = state.subarray(start, start + length)
            }
            const offset = state.indexOf(Buffer.from([tag, 0x20]))
            assert.notEqual(offset, -1)
            const hash = state.subarray(offset + 2, offset + 34)
            config.chunks.unshift(frame(field(4, concat(field(1, 1), field(2, field(1, hash))))))
          }
          if (initial) {
            const chunks = config.appendError ? [JSON.stringify(config.appendError)] : config.appendChunks
            streams.set(id, [...(chunks ?? [])])
          } else {
            streams.set(id, [])
          }
          result = {status: initial && config.appendError ? 400 : 200, id, headers: {}}
        }
        break
      }
      case "http.read": {
        const source = streams.get(params.id)
        assert.ok(source, "unknown response id")
        tick += scenario.readStep ?? 0
        const config = scenarioAttempts[attempt]
        // readError is for the RunSSE download only; BidiAppend bodies are empty OK.
        if (String(params.id).startsWith("run-") && !source.length && config?.readError) {
          error = config.readError
          break
        }
        result = source.length ? {data: base64(source.shift())} : {done: true}
        break
      }
      default:
        throw Error("unexpected host call " + method)
    }
    reply = Buffer.from(JSON.stringify(error ? {error} : {result}))
    return reply.length
  }
  instance = new WebAssembly.Instance(wasm, {agent2api: {
    call,
    read(pointer) {
      reply.copy(memory(), pointer)
      return reply.length
    },
  }})
  const body = scenario.body ?? {
    model: "auto", messages: [{role: "user", content: "fixture"}],
    tools: [{type: "function", function: {name: "lookup"}}],
  }
  const data = Buffer.from(JSON.stringify({
    method: scenario.method ?? "conversation",
    params: scenario.params ?? {body},
  }))
  const pointer = instance.exports.a2a_alloc(data.length)
  data.copy(memory(), pointer)
  const packed = instance.exports.a2a_call(pointer, data.length)
  const start = Number(packed & 0xffffffffn)
  const output = JSON.parse(memory().subarray(start, start + Number(packed >> 32n)).toString())
  return {
    output, calls, sleeps, appended, snapshot, elapsed: tick, attempts: attempt + 1,
    done: events.some((line) => line === "data: [DONE]\n\n"),
    frames: events.filter((line) => !line.includes("[DONE]")).map((line) => JSON.parse(line.slice(6))),
  }
}

test("empty reads back off instead of spinning until the fuel window is exhausted", () => {
  const run = hostRun({attempts: [{chunks: [Buffer.alloc(0), Buffer.alloc(0), content("ok"), end()]}]})
  assert.equal(run.done, true)
  assert.equal(run.frames.at(-1).choices[0].finish_reason, "stop")
  assert.equal(run.calls.filter((method) => method === "clock.sleep").length, 2)
  assert.equal(run.elapsed, 150)
})

test("sustained empty reads cap at one second and real data resets the delay", () => {
  const run = hostRun({attempts: [{chunks: [
    ...Array.from({length: 7}, () => Buffer.alloc(0)),
    content("ok"), Buffer.alloc(0), Buffer.alloc(0), end(),
  ]}]})
  assert.equal(run.done, true)
  assert.deepEqual(run.sleeps, [50, 100, 200, 400, 800, 1000, 1000, 50, 100])
})

test("normal completion preserves trailing usage and supports arbitrary framing", () => {
  const data = concat(content("hello"), usage, end({metadata: {}}))
  const run = hostRun({attempts: [{chunks: [data.subarray(0, 3), data.subarray(3, 12), data.subarray(12)]}]})
  assert.equal(run.output.result, null)
  assert.equal(run.done, true)
  assert.equal(run.frames.at(-1).choices[0].finish_reason, "stop")
  assert.equal(run.frames.at(-1).usage.total_tokens, 6)
  assert.equal(run.frames[1].choices[0].delta.content, "hello")
  assert.equal(run.calls.filter((call) => call === "stream.head").length, 1)
  assert.equal(run.calls.indexOf("stream.head") + 1, run.calls.indexOf("stream.chunk"))
  assert.ok(run.calls.indexOf("stream.head") > run.calls.indexOf("http.read"))
})

for (const [label, tail] of [
  ["after upstream error", [failedEnd]],
  ["after turn usage followed by EOF", [usage]],
  ["after a truncated envelope", [failedEnd.subarray(0, 8)]],
  ["after usage followed by a later upstream error", [usage, failedEnd]],
]) {
  test("partial output is not completed " + label, () => {
    const run = hostRun({attempts: [{chunks: [content("partial"), ...tail]}]})
    assert.equal(run.output.error.code, "cursor_partial_stream")
    assert.equal(run.done, false)
    assert.equal(run.attempts, 1)
  })
}

for (const [label, config] of Object.entries({
  append: {chunks: [], appendError: maxRequired},
  HTTP: {status: 400, chunks: [JSON.stringify(maxRequired)]},
  terminal: {chunks: [end(maxRequired)]},
})) {
  test("Max Mode " + label + " retry uses the matching blob table", () => {
    const run = hostRun({attempts: [config, {chunks: [content("ok"), end()], kvLookup: true}]})
    assert.equal(run.attempts, 2)
    assert.equal(run.done, true)
    const second = run.appended.filter(({attempt}) => attempt === 1)
    assert.equal(second.length, 2)
    assert.ok(!second[1].message.includes(Buffer.from("blob not found")))
  })
}

test("explicit Max Mode refusals after output do not replay", () => {
  const run = hostRun({attempts: [{chunks: [content("partial"), end(maxRequired)]}]})
  assert.equal(run.attempts, 1)
  assert.equal(run.output.error.code, "cursor_partial_stream")
  assert.equal(run.done, false)
})

test("transport peer-closed mid-stream continues on a new RunSSE", () => {
  const run = hostRun({
    body: {model: "auto", messages: [{role: "user", content: "fixture"}]},
    attempts: [
      {
        chunks: [content("hello ")],
        readError: {message: "error decoding response body → peer closed connection without sending TLS close_notify", status: 502},
        kvLookup: "history",
      },
      {chunks: [content("world"), end()], kvLookup: "history"},
    ],
  })
  assert.equal(run.attempts, 2)
  assert.equal(run.done, true)
  const texts = run.frames.map((item) => item.choices[0].delta.content).filter(Boolean)
  assert.deepEqual(texts, ["hello ", "world"])
  assert.equal(run.frames.at(-1).choices[0].finish_reason, "stop")
  const [first, resumed] = [0, 1].map((attempt) =>
    run.appended.filter((entry) => entry.attempt === attempt)[1].message)
  assert.ok(first.includes(Buffer.from("fixture")))
  assert.deepEqual(resumed, first)
})

test("parallel tool calls across reads are forwarded before closing the batch", () => {
  const checkpoint = frame(field(3, field(4, "first")))
  const run = hostRun({attempts: [{
    chunks: [tool("first"), toolCount(2), checkpoint, tool("first"), tool("second")],
  }]})
  assert.equal(run.done, true)
  const calls = run.frames.flatMap((item) => item.choices[0].delta.tool_calls ?? [])
  assert.deepEqual(calls.map(({id, index}) => [id, index]), [["first", 0], ["second", 1]])
  assert.equal(run.frames.at(-1).choices[0].finish_reason, "tool_calls")
})

test("declared incomplete tool batch is reported as an error", () => {
  const run = hostRun({attempts: [{chunks: [toolCount(2), tool("first"), end()]}]})
  assert.equal(run.done, false)
  assert.match(run.output.error.message, /批次未收全/)
})

test("gzip frames are supported and unknown compression is rejected", () => {
  const zipped = frame(gzipSync(content("gzip").subarray(5)), 1)
  const success = hostRun({attempts: [{headers: {"connect-content-encoding": "gzip"}, chunks: [zipped, end()]}]})
  assert.equal(success.done, true)
  const failure = hostRun({attempts: [{chunks: [zipped, end()]}]})
  assert.equal(failure.done, false)
  assert.equal(failure.output.error.code, "cursor_connect_protocol")
})

test("one stream remains live across forty simulated seconds of active output", () => {
  const chunks = Array.from({length: 15}, (_, index) => content(String(index)))
  const run = hostRun({readStep: 3000, attempts: [{chunks: [...chunks, end()]}]})
  assert.equal(run.done, true)
  assert.equal(run.frames.filter((row) => row.choices[0].delta.content).length, 15)
  assert.ok(run.elapsed > 40_000)
  assert.ok(run.appended.length > 1)
})

test("an ambiguous initial append is not replayed", () => {
  const run = hostRun({attempts: [{chunks: [], appendTransportError: true}]})
  assert.equal(run.attempts, 1)
  assert.equal(run.done, false)
  assert.equal(run.output.error.code, "cursor_append_unconfirmed")
})

test("heartbeat-only streams report a first-output timeout", () => {
  const heartbeat = update(13, Buffer.alloc(0))
  const run = hostRun({readStep: 11_000, attempts: [{chunks: Array(10).fill(heartbeat)}]})
  assert.equal(run.output.error.code, "cursor_first_output_timeout")
  assert.equal(run.done, false)
  assert.equal(run.frames.length, 0)
})

test("tool batches with no count settle after a quiet interval", () => {
  const heartbeat = update(13, Buffer.alloc(0))
  const run = hostRun({readStep: 6_000, attempts: [{chunks: [tool("first"), ...Array(8).fill(heartbeat)]}]})
  assert.equal(run.done, true)
  assert.equal(run.frames.at(-1).choices[0].finish_reason, "tool_calls")
})

test("client cancellation stops forwarding and closes the upstream connection", () => {
  const run = hostRun({cancelOnChunk: true, attempts: [{chunks: [content("test"), end()]}]})
  assert.equal(run.done, false)
  assert.equal(run.output.error.status, 499)
  assert.ok(run.calls.includes("http.close"))
})

test("oversized requests and attachments fail before any outbound network call", () => {
  for (const [body, status] of [
    [{messages: [{role: "user", content: "x".repeat(4 * 1024 * 1024)}]}, 413],
    [{messages: [{role: "user", content: [{type: "image_url", image_url: {url: "example"}}]}]}, 400],
  ]) {
    const run = hostRun({attempts: [], body})
    assert.equal(run.output.error.status, status)
    assert.equal(run.attempts, 0)
  }
})

test("account and upstream auth failures retain their status before opening a stream", () => {
  for (const scenario of [
    {accountError: {status: 401, message: "account expired"}},
    {attempts: [{status: 401, chunks: []}]},
  ]) {
    const run = hostRun(scenario)
    assert.equal(run.output.error.status, 401)
    assert.ok(!run.calls.includes("stream.head"))
  }
})

test("BidiAppend backs off on empty reads and resets the delay after data", () => {
  const empty = Buffer.alloc(0)
  const run = hostRun({attempts: [{
    appendChunks: [empty, empty, Buffer.from("ack"), empty],
    chunks: [content("ok"), end()],
  }]})
  assert.equal(run.done, true)
  assert.deepEqual(run.sleeps, [50, 100, 50])
})

test("BidiAppend stops waiting for an empty acknowledgement at its deadline", () => {
  const run = hostRun({attempts: [{
    appendChunks: Array(300).fill(Buffer.alloc(0)),
    chunks: [content("unexpected")],
  }]})
  assert.equal(run.output.error.code, "cursor_append_unconfirmed")
  assert.equal(run.output.error.status, 504)
  assert.equal(run.elapsed, 60_000)
  assert.ok(run.sleeps.length < 70)
  assert.equal(run.attempts, 1)
  assert.ok(!run.calls.includes("stream.head"))
})

test("EOF at a frame boundary resumes partial text once without reopening client headers", () => {
  const run = hostRun({attempts: [
    {chunks: [content("prefix ")]},
    {chunks: [content("suffix"), end()]},
  ]})
  assert.equal(run.attempts, 2)
  assert.equal(run.done, true)
  assert.deepEqual(run.frames.flatMap((frame) => frame.choices[0].delta.content ?? []), ["prefix ", "suffix"])
  assert.equal(run.calls.filter((call) => call === "stream.head").length, 1)
})

test("repeated EOFs stop at the continuation limit", () => {
  const run = hostRun({attempts: Array.from({length: 17}, (_, index) => ({
    chunks: [content(String(index))],
  }))})
  assert.equal(run.attempts, 17)
  assert.equal(run.done, false)
  assert.equal(run.output.error.code, "cursor_partial_stream")
  assert.match(run.output.error.message, /16 次/)
  assert.equal(run.calls.filter((call) => call === "stream.head").length, 1)
})

test("EOF before output or after tool calls never resumes", () => {
  const noOutput = hostRun({attempts: [{chunks: []}]})
  assert.equal(noOutput.output.error.code, "cursor_stream_eof")
  assert.ok(!noOutput.calls.includes("stream.head"))
  const tools = hostRun({attempts: [{chunks: [toolCount(2), tool("first")]}]})
  assert.equal(tools.output.error.code, "cursor_partial_stream")
  assert.equal(tools.done, false)
  assert.equal(tools.attempts, 1)
})

test("a cancelled upstream read never starts continuation", () => {
  const run = hostRun({attempts: [{
    chunks: [content("partial")],
    readError: {status: 499, message: "peer closed on client cancellation"},
  }]})
  assert.equal(run.output.error.status, 499)
  assert.equal(run.attempts, 1)
  assert.equal(run.done, false)
})

test("usage snapshots replace counters within an attempt and sum across continuation", () => {
  const run = hostRun({attempts: [
    {
      chunks: [content("first"), update(31, concat(field(3, 1), field(4, 4)))],
    },
    {
      chunks: [
        content("second"),
        update(31, concat(field(2, 10), field(3, 19), field(6, 3))),
        update(14, concat(field(1, 10), field(2, 20))),
        end(),
      ],
    },
  ]})
  assert.equal(run.done, true)
  assert.deepEqual(run.frames.at(-1).usage, {
    prompt_tokens: 14,
    completion_tokens: 21,
    total_tokens: 35,
    prompt_tokens_details: {cached_tokens: 4, cache_write_tokens: 0},
    completion_tokens_details: {reasoning_tokens: 3},
  })
})

test("login polling can recover from a settings failure and an expired reservation", () => {
  const snapshot = {
    revision: 1, schema: 1, compatible: true,
    value: {
      expiresAt: 1_700_000_900_000, nextAt: 0, uuid: "fake", verifier: "fake",
    },
  }
  const poll = {
    method: "login.poll", params: {},
    httpResponses: {"/auth/poll": {status: 404}},
  }
  const failure = hostRun({...poll, snapshot, settingsError: {status: 503, message: "unavailable"}})
  assert.equal(failure.output.error.status, 503)
  assert.deepEqual(failure.snapshot, snapshot)
  const recovered = hostRun({...poll, snapshot: failure.snapshot})
  assert.equal(recovered.output.result.state, "pending")
  assert.ok(recovered.calls.includes("http.open"))
  assert.equal(recovered.snapshot.value.busyUntil, 0)

  const reserved = hostRun({
    ...poll, snapshot: {...snapshot, value: {...snapshot.value, busyUntil: 1_700_000_020_000}},
  })
  assert.ok(!reserved.calls.includes("http.open"))
  const expired = hostRun({
    ...poll, snapshot: {...snapshot, value: {...snapshot.value, busyUntil: 1_699_999_999_000}},
  })
  assert.ok(expired.calls.includes("http.open"))
})

test("empty model refresh preserves the previously discovered catalog", () => {
  const snapshot = {
    revision: 1, schema: 1, compatible: true,
    value: {source: "cursor:test", catalog: {
      meta: {remote: true, refreshed_at: 1_699_999_000_000},
      models: [{id: "auto"}, {id: "saved-model"}],
    }},
  }
  const run = hostRun({
    method: "models.refresh", params: {force: true}, snapshot,
    httpResponses: {
      "/aiserver.v1.AiService/AvailableModels": {chunks: ['{"models":[]}']},
      "/agent.v1.AgentService/GetUsableModels": {chunks: ['{"models":[]}']},
    },
  })
  assert.equal(run.output.result.refreshed, false)
  assert.equal(run.output.result.count, 2)
  assert.deepEqual(run.snapshot, snapshot)
})
