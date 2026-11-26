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
  const scenarioAttempts = scenario.attempts.map((config) => ({...config, chunks: [...config.chunks]}))
  const events = []
  const calls = []
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
        break
      case "clock.now":
        result = 1_700_000_000_000 + tick
        break
      case "clock.sleep":
        assert.equal(params.milliseconds, 50)
        tick += params.milliseconds
        break
      case "state.get":
        result = {revision: 0, schema: 1, compatible: true, value: null}
        break
      case "catalog.get":
      case "stream.head":
      case "settings.get":
      case "http.close":
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
        if (params.url.endsWith("/RunSSE")) {
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
            const offset = message.indexOf(Buffer.from([0x42, 0x20]))
            assert.notEqual(offset, -1)
            const hash = message.subarray(offset + 2, offset + 34)
            config.chunks.unshift(frame(field(4, concat(field(1, 1), field(2, field(1, hash))))))
          }
          streams.set(id, initial && config.appendError ? [JSON.stringify(config.appendError)] : [])
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
  const data = Buffer.from(JSON.stringify({method: "conversation", params: {body}}))
  const pointer = instance.exports.a2a_alloc(data.length)
  data.copy(memory(), pointer)
  const packed = instance.exports.a2a_call(pointer, data.length)
  const start = Number(packed & 0xffffffffn)
  const output = JSON.parse(memory().subarray(start, start + Number(packed >> 32n)).toString())
  return {
    output, calls, appended, elapsed: tick, attempts: attempt + 1,
    done: events.some((line) => line === "data: [DONE]\n\n"),
    frames: events.filter((line) => !line.includes("[DONE]")).map((line) => JSON.parse(line.slice(6))),
  }
}

test("empty reads back off instead of spinning until the fuel window is exhausted", () => {
  const run = hostRun({attempts: [{chunks: [Buffer.alloc(0), Buffer.alloc(0), content("ok"), end()]}]})
  assert.equal(run.done, true)
  assert.equal(run.frames.at(-1).choices[0].finish_reason, "stop")
  assert.equal(run.calls.filter((method) => method === "clock.sleep").length, 2)
  assert.equal(run.elapsed, 100)
})

test("normal completion preserves trailing usage and supports arbitrary framing", () => {
  const data = concat(content("hello"), usage, end({metadata: {}}))
  const run = hostRun({attempts: [{chunks: [data.subarray(0, 3), data.subarray(3, 12), data.subarray(12)]}]})
  assert.equal(run.output.result, null)
  assert.equal(run.done, true)
  assert.equal(run.frames.at(-1).choices[0].finish_reason, "stop")
  assert.equal(run.frames.at(-1).usage.total_tokens, 6)
  assert.equal(run.frames[1].choices[0].delta.content, "hello")
  assert.ok(run.calls.indexOf("stream.head") < run.calls.indexOf("account.get"))
})

for (const [label, tail] of [
  ["after upstream error", [failedEnd]],
  ["after EOF", []],
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
    attempts: [
      {
        chunks: [content("hello ")],
        readError: {message: "error decoding response body → peer closed connection without sending TLS close_notify", status: 502},
      },
      {chunks: [content("world"), end()]},
    ],
  })
  assert.equal(run.attempts, 2)
  assert.equal(run.done, true)
  const texts = run.frames.map((item) => item.choices[0].delta.content).filter(Boolean)
  assert.deepEqual(texts, ["hello ", "world"])
  assert.equal(run.frames.at(-1).choices[0].finish_reason, "stop")
})

test("parallel tool calls across reads are forwarded before closing the batch", () => {
  const run = hostRun({attempts: [{chunks: [tool("first"), toolCount(2), tool("first"), tool("second")]}]})
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
