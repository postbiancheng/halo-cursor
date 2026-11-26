#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
export HALO_CURSOR_EXPECTED_VERSION="${HALO_CURSOR_EXPECTED_VERSION:-$VERSION}"
export AGENT2API_HOST="${AGENT2API_HOST:-http://127.0.0.1:3065}"

python3 - <<'PY'
import http.client, json, os, pathlib, sys, time, urllib.error, urllib.request

HOST = os.environ["AGENT2API_HOST"].rstrip("/")
EXPECT = os.environ["HALO_CURSOR_EXPECTED_VERSION"]
PROVIDER = "halo-cursor"
PACK = "agent2api-wasm-halo-cursor"

key = os.environ.get("AGENT2API_API_KEY")
cfg = pathlib.Path.home() / ".agent2api" / "config.json"
if not key and cfg.exists():
    data = json.loads(cfg.read_text())
    key = data.get("apiKey") or data.get("api_key")
if not key:
    creds = pathlib.Path.home() / ".dsh" / ".credentials.yaml"
    if creds.exists():
        try:
            import yaml  # type: ignore

            refs = (yaml.safe_load(creds.read_text()) or {}).get("refs") or {}
            key = refs.get("AGENT2API_API_KEY")
        except Exception:
            key = None
if not key:
    print(
        "FAIL: set AGENT2API_API_KEY, ~/.agent2api/config.json apiKey, or .dsh credentials",
        file=sys.stderr,
    )
    raise SystemExit(2)

# Distinctive UA so request logs are attributable (Host may still show 未知客户端
# unless the catalog knows cursor-agent).
CLIENT_UA = os.environ.get(
    "HALO_CURSOR_SMOKE_UA",
    f"cursor-agent/2026.09.26 (Halo-Cursor-smoke-{os.getpid()}; darwin)",
)
H = {
    "Authorization": f"Bearer {key}",
    "Content-Type": "application/json",
    "User-Agent": CLIENT_UA,
}


def call(method, path, body=None, timeout=120, stream=False):
    data = None if body is None else json.dumps(body).encode()
    req = urllib.request.Request(f"{HOST}{path}", data=data, headers=H, method=method)
    started = time.monotonic()
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            if stream:
                return resp.status, read_sse(resp, started)
            raw = resp.read().decode()
            return resp.status, json.loads(raw) if raw else None
    except urllib.error.HTTPError as err:
        raw = err.read().decode("utf-8", "replace")
        try:
            payload = json.loads(raw) if raw else raw
        except json.JSONDecodeError:
            payload = raw
        return err.code, payload
    except (http.client.HTTPException, OSError, ValueError) as err:
        fail(f"{method} {path}: {err}")


def read_sse(response, started):
    outcome = {"chars": 0, "events": 0, "first_content": None, "finish": None, "done": False}
    data = []
    event = ""
    last_content = None
    for raw in response:
        line = raw.decode("utf-8").rstrip("\r\n")
        if line.startswith("data:"):
            data.append(line[5:].lstrip(" "))
        elif line.startswith("event:"):
            event = line[6:].strip()
        elif not line:
            if not data:
                if event == "error":
                    raise ValueError("SSE error event without a body")
                event = ""
                continue
            payload = "\n".join(data)
            data.clear()
            if event == "error":
                raise ValueError("SSE error event")
            event = ""
            if payload == "[DONE]":
                outcome["done"] = True
                break
            frame = json.loads(payload)
            if not isinstance(frame, dict):
                raise ValueError("invalid SSE JSON frame")
            if "error" in frame or frame.get("type") == "error":
                error = frame.get("error")
                detail = error.get("code") or error.get("message") if isinstance(error, dict) else error
                elapsed = time.monotonic() - started
                raise ValueError(
                    f"SSE upstream error after {elapsed:.1f}s and {outcome['events']} content deltas: "
                    f"{str(detail or frame.get('message'))[:400]}"
                )
            for choice in frame.get("choices") or []:
                delta = choice.get("delta") or {}
                text = delta.get("content")
                # Host may normalize missing finish_reason to ""; only non-empty counts.
                finish = choice.get("finish_reason") or None
                if isinstance(text, str) and text:
                    if outcome["finish"] is not None:
                        raise ValueError("content after finish_reason")
                    last_content = time.monotonic() - started
                    if outcome["first_content"] is None:
                        outcome["first_content"] = last_content
                    outcome["chars"] += len(text)
                    outcome["events"] += 1
                if finish:
                    if outcome["finish"] is not None:
                        raise ValueError("repeated finish_reason")
                    outcome["finish"] = finish
    outcome["elapsed"] = time.monotonic() - started
    if outcome["first_content"] is not None:
        outcome["content_span"] = last_content - outcome["first_content"]
    return outcome


def fail(msg):
    print(f"FAIL: {msg}", file=sys.stderr)
    raise SystemExit(1)


def ok(msg):
    print(f"OK: {msg}")


status, plugins = call("GET", "/api/plugins")
if status != 200:
    fail(f"plugins HTTP {status}: {plugins}")
packs = ((plugins.get("data") or plugins).get("packs") or [])
pack = next((item for item in packs if item.get("id") == PACK), None)
if not pack:
    fail(f"pack {PACK} not installed")
phase = (pack.get("startup") or {}).get("phase")
if pack.get("version") != EXPECT or phase != "ready":
    fail(f"pack version/phase want {EXPECT}/ready got {pack.get('version')}/{phase}")
ok(f"plugin {EXPECT} ready")

status, models = call("GET", "/v1/models")
if status != 200:
    fail(f"models HTTP {status}: {models}")
rows = (models.get("data") or []) if isinstance(models, dict) else []
owned = [row for row in rows if row.get("owned_by") == PROVIDER]
chat_models = [
    row["id"]
    for row in owned
    if row.get("id") and row.get("kind", "chat") != "image"
]
if not chat_models:
    fail("no halo-cursor chat models in /v1/models")
if "auto" not in chat_models:
    fail(f"auto missing from halo-cursor catalog: {chat_models[:12]}")
ok(f"/v1/models has {len(owned)} halo-cursor models; auto present")

refresh = None
for attempt in range(3):
    status, refresh = call(
        "POST", "/api/models/refresh", {"provider": PROVIDER, "force": True}, timeout=90
    )
    if status != 200 or not isinstance(refresh, dict):
        if attempt == 2:
            fail(f"models.refresh HTTP {status}")
        time.sleep(1.5)
        continue
    refresh_data = refresh.get("data") or refresh
    failed = refresh_data.get("failed")
    if failed or refresh_data.get("error"):
        if attempt == 2:
            fail(f"models.refresh reported an upstream failure: {failed or refresh_data.get('error')}")
        time.sleep(1.5)
        continue
    rows = refresh_data.get("results") or []
    if rows and not any(
        row.get("provider") == PROVIDER and row.get("status") in {"refreshed", "unchanged"}
        for row in rows
        if isinstance(row, dict)
    ):
        if attempt == 2:
            fail("models.refresh did not refresh halo-cursor")
        time.sleep(1.5)
        continue
    break
ok("models.refresh completed")

status, accounts = call("GET", "/api/accounts?includeHidden=true")
if status != 200:
    fail(f"accounts HTTP {status}: {accounts}")
account = next(
    (
        item
        for item in ((accounts.get("data") or accounts).get("accounts") or [])
        if item.get("provider") == PROVIDER and item.get("enabled")
    ),
    None,
)
if not account:
    fail("no enabled halo-cursor account")
account_id = account["id"]
ok(f"enabled {PROVIDER} account found")

status, usage_start = call(
    "POST",
    "/api/accounts/usage/start",
    {"accountIds": [account_id]},
)
if status != 200:
    fail(f"usage.start HTTP {status}: {usage_start}")

usage_ok = False
last = None
for _ in range(40):
    status, snap = call("GET", "/api/accounts/usage/snapshot")
    if status != 200:
        time.sleep(0.5)
        continue
    data = snap.get("data") or snap
    results = data.get("results") or []
    if isinstance(results, dict):
        row = results.get(account_id) or {}
    else:
        row = next((item for item in results if item.get("id") == account_id), {}) or {}
    usage = row.get("usage") if isinstance(row.get("usage"), dict) else row
    last = usage if isinstance(usage, dict) else row
    if row and not row.get("error") and isinstance(last, dict) and (
        last.get("windows") or last.get("credits")
    ):
        usage_ok = True
        break
    queries = data.get("queries") or []
    active = next((q for q in queries if q.get("id") == account_id), None)
    if active and active.get("error"):
        fail(f"usage query error: {active.get('error')}")
    if active and active.get("status") in {"queued", "running", None}:
        time.sleep(0.5)
        continue
    time.sleep(0.5)
if not usage_ok:
    fail(f"usage snapshot empty/failed: {json.dumps(last, ensure_ascii=False)[:300]}")
ok(
    f"usage available={last.get('available')} windows={len(last.get('windows') or [])} "
    f"credits={bool(last.get('credits'))}"
)

# Host may hand bare `auto` to higher-priority allowRouteSkip accounts even when
# they do not advertise that model. Park them for the chat portion only.
paused = [
    item["id"]
    for item in ((accounts.get("data") or accounts).get("accounts") or [])
    if item.get("enabled")
    and item.get("id") != account_id
    and item.get("chatSupported") is not False
]


def set_enabled(account_ids, enabled):
    for item_id in account_ids:
        status, _ = call(
            "PATCH",
            f"/api/accounts/{item_id}",
            {"enabled": enabled},
            timeout=30,
        )
        if status != 200:
            fail(f"account {'enable' if enabled else 'pause'} HTTP {status} for {item_id}")


def chat(prompt, timeout):
    status, outcome = call(
        "POST",
        "/v1/chat/completions",
        {
            "model": "auto",
            "accountId": account_id,
            "stream": True,
            "messages": [{"role": "user", "content": prompt}],
        },
        timeout=timeout,
        stream=True,
    )
    if status != 200:
        fail(f"chat HTTP {status}: {outcome}")
    if not outcome["done"] or outcome["finish"] not in {"stop", "length"} or not outcome["chars"]:
        fail(f"incomplete chat stream: {outcome}")
    return outcome


def expect_halo_route(started_at):
    deadline = time.monotonic() + 30
    last_entries = []
    while time.monotonic() < deadline:
        status, stats = call("GET", "/api/stats/requests?limit=80")
        if status != 200:
            fail(f"stats.requests HTTP {status}")
        entries = ((stats.get("data") or stats).get("entries") or [])
        last_entries = [
            item
            for item in entries
            if item.get("ts", 0) >= started_at - 2000
            and (item.get("clientEvidence") or {}).get("userAgent") == CLIENT_UA
        ]
        recent = next(
            (
                item
                for item in last_entries
                if item.get("provider") == PROVIDER and item.get("accountId") == account_id
            ),
            None,
        )
        if recent:
            if recent.get("providerLabel") != "Halo Cursor" or recent.get("status") != 200:
                fail(
                    f"smoke halo-cursor request status={recent.get('status')!r} "
                    f"label={recent.get('providerLabel')!r}"
                )
            client = (recent.get("client") or "").lower()
            client_label = recent.get("clientLabel") or ""
            if client != "cursor" and "cursor" not in client_label.lower():
                fail(f"client not matched as Cursor: {client!r} / {client_label!r}")
            return
        time.sleep(0.5)
    summary = [
        f"{item.get('provider')}:{item.get('status')}:{item.get('model')}"
        for item in last_entries[:8]
    ]
    fail(f"no halo-cursor request report found for this smoke request; seen={summary}")


set_enabled(paused, False)
ok(f"paused {len(paused)} other chat accounts for auto routing")
try:
    chat_started_at = int(time.time() * 1000)
    short = chat("只回：ready", timeout=180)
    expect_halo_route(chat_started_at)
    ok(f"short auto chat, TTFT={short['first_content']:.2f}s, {short['events']} content deltas")

    # One continuous RunSSE must stay open past the wall gate. RunSSE must not
    # pass host timeout_ms (hard download deadline); longevity is stream-idle.
    long_seconds = float(os.environ.get("HALO_CURSOR_AUTO_LONG_SECONDS", "40"))
    if not 0 < long_seconds < float("inf"):
        fail("HALO_CURSOR_AUTO_LONG_SECONDS must be finite and positive")
    long_started_at = int(time.time() * 1000)
    long_started = time.monotonic()
    outcome = chat(
        "用中文写一篇不少于2500字的长文，主题是流式 API 的可靠性："
        "从请求验证、首包、网络分帧、心跳、结束事件、超时、断线、限流、"
        "重试与观测写起，分小节展开，不要提纲，不要中途收束。",
        timeout=max(300, int(long_seconds) + 240),
    )
    expect_halo_route(long_started_at)
    elapsed = outcome.get("elapsed") or (time.monotonic() - long_started)
    if elapsed < long_seconds or outcome["events"] < 2:
        fail(
            f"auto long chat only reached {elapsed:.1f}s in one stream "
            f"(need >={long_seconds:.0f}s, events={outcome['events']})"
        )
    ok(
        f"auto long chat: {elapsed:.1f}s one stream, "
        f"{outcome['events']} content deltas, {outcome['chars']} chars"
    )
finally:
    for item_id in paused:
        status, _ = call(
            "PATCH",
            f"/api/accounts/{item_id}",
            {"enabled": True},
            timeout=30,
        )
        if status != 200:
            print(f"WARN: failed to restore account {item_id} (HTTP {status})", file=sys.stderr)

print("SMOKE PASS")
PY
