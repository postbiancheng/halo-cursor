#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
export HALO_CURSOR_EXPECTED_VERSION="${HALO_CURSOR_EXPECTED_VERSION:-$VERSION}"
export AGENT2API_HOST="${AGENT2API_HOST:-http://127.0.0.1:3065}"

python3 - <<'PY'
import json, os, pathlib, sys, time, urllib.error, urllib.request

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
    "cursor-agent/2026.09.26 (Halo-Cursor-smoke; darwin)",
)
H = {
    "Authorization": f"Bearer {key}",
    "Content-Type": "application/json",
    "User-Agent": CLIENT_UA,
}


def call(method, path, body=None, timeout=120, stream=False, headers=None):
    data = None if body is None else json.dumps(body).encode()
    req_headers = dict(H)
    if headers:
        req_headers.update(headers)
    req = urllib.request.Request(
        f"{HOST}{path}", data=data, headers=req_headers, method=method
    )
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            if stream:
                chunks = []
                while True:
                    line = resp.readline()
                    if not line:
                        break
                    text = line.decode("utf-8", "replace")
                    chunks.append(text)
                    if text.strip() == "data: [DONE]":
                        break
                return resp.status, "".join(chunks)
            raw = resp.read().decode()
            return resp.status, json.loads(raw) if raw else None
    except urllib.error.HTTPError as err:
        raw = err.read().decode("utf-8", "replace")
        try:
            payload = json.loads(raw) if raw else raw
        except json.JSONDecodeError:
            payload = raw
        return err.code, payload


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

status, refresh = call(
    "POST", "/api/models/refresh", {"provider": PROVIDER, "force": True}, timeout=90
)
if status != 200:
    fail(f"models.refresh HTTP {status}: {refresh}")
ok(f"models.refresh => {json.dumps(refresh, ensure_ascii=False)[:180]}")

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
# Best-effort credential refresh so account.view can surface email as the label.
# Host may 502 with "未产生变化" when tokens are still fresh — that is fine.
status, refreshed = call("POST", "/api/accounts/refresh", {"id": account_id}, timeout=90)
if status == 200:
    status, accounts = call("GET", "/api/accounts?includeHidden=true")
    if status != 200:
        fail(f"accounts recheck HTTP {status}: {accounts}")
    account = next(
        (
            item
            for item in ((accounts.get("data") or accounts).get("accounts") or [])
            if item.get("id") == account_id
        ),
        None,
    )
    if not account:
        fail("halo-cursor account missing after refresh")
elif not (
    isinstance(refreshed, dict)
    and "未产生变化" in str(refreshed.get("error") or refreshed)
):
    fail(f"account.refresh HTTP {status}: {refreshed}")
account_name = (
    account.get("displayName")
    or account.get("name")
    or account.get("email")
    or account.get("loginName")
    or ""
)
account_email = (account.get("email") or account.get("accountId") or "").strip()
if "@" not in account_email and "@" not in account_name:
    fail(f"account has no email: name={account_name!r} email={account_email!r}")
ok(
    f"account {account_id} email={account_email or account_name} "
    f"label={account_name!r} canRefresh={account.get('canRefresh')}"
)

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
        last.get("windows")
        or last.get("credits")
        or last.get("subscription")
        or last.get("quotaSources")
        or last.get("plan")
        or last.get("available") is not None
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

status, stream = call(
    "POST",
    "/v1/chat/completions",
    {
        "model": "auto",
        "stream": True,
        "messages": [{"role": "user", "content": "只回：ready"}],
        "max_tokens": 16,
    },
    timeout=180,
    stream=True,
)
if status != 200:
    fail(f"chat HTTP {status}: {stream}")
if "data: " not in stream:
    fail(f"chat stream empty: {stream[:400]}")
if "error" in stream.lower() and "ready" not in stream.lower() and '"content"' not in stream:
    fail(f"chat stream looks like error: {stream[:500]}")
ok(f"chat stream on auto ({len(stream)} chars)")


def extract_assistant_text(raw: str) -> str:
    parts = []
    for line in raw.splitlines():
        line = line.strip()
        if not line.startswith("data:"):
            continue
        payload = line[5:].strip()
        if not payload or payload == "[DONE]":
            continue
        try:
            event = json.loads(payload)
        except json.JSONDecodeError:
            continue
        for choice in event.get("choices") or []:
            delta = choice.get("delta") or {}
            content = delta.get("content")
            if isinstance(content, str) and content:
                parts.append(content)
            message = choice.get("message") or {}
            content = message.get("content")
            if isinstance(content, str) and content:
                parts.append(content)
    return "".join(parts)


long_seconds = float(os.environ.get("HALO_CURSOR_AUTO_LONG_SECONDS", "40"))
long_timeout = max(180, int(long_seconds) + 120)
started = time.time()
messages = [
    {
        "role": "user",
        "content": (
            "请用中文详细讲解：什么是冒烟测试、它和回归测试的区别、"
            "以及在 Agent2API WASM 插件发版流程里为什么要做。请分点展开，"
            "每点至少写一段，尽量写充分，不要提前结束。"
        ),
    }
]
rounds = 0
chars = 0
while True:
    rounds += 1
    status, stream = call(
        "POST",
        "/v1/chat/completions",
        {
            "model": "auto",
            "stream": True,
            "messages": messages,
            "max_tokens": 1200,
        },
        timeout=long_timeout,
        stream=True,
    )
    if status != 200:
        fail(f"auto long chat HTTP {status}: {stream}")
    if "data: " not in stream:
        fail(f"auto long chat empty stream: {stream[:400]}")
    if "error" in stream.lower() and '"content"' not in stream:
        fail(f"auto long chat error stream: {stream[:500]}")
    text = extract_assistant_text(stream)
    chars += len(text)
    elapsed = time.time() - started
    if elapsed >= long_seconds:
        break
    if rounds >= 8:
        fail(
            f"auto long chat only reached {elapsed:.1f}s after {rounds} rounds "
            f"(need >={long_seconds:.0f}s)"
        )
    messages.append({"role": "assistant", "content": text or "（上轮无文本，请继续）"})
    messages.append(
        {
            "role": "user",
            "content": "请继续展开下一点，再写一段更细的说明，不要重复前面内容。",
        }
    )

elapsed = time.time() - started
if elapsed < long_seconds:
    fail(f"auto long chat too short: {elapsed:.1f}s < {long_seconds:.0f}s")
ok(f"auto long chat: {elapsed:.1f}s rounds={rounds} chars={chars}")

status, stats = call("GET", "/api/stats/requests?limit=30")
if status != 200:
    fail(f"stats.requests HTTP {status}: {stats}")
entries = ((stats.get("data") or stats).get("entries") or [])
recent = next(
    (
        item
        for item in entries
        if item.get("provider") == PROVIDER and item.get("accountId") == account_id
    ),
    None,
)
if not recent:
    fail("no recent halo-cursor request in /api/stats/requests")
provider_label = recent.get("providerLabel") or ""
ua = ((recent.get("clientEvidence") or {}).get("userAgent") or "")
logged_model = recent.get("model") or ""
logged_account = recent.get("accountName") or ""
client = (recent.get("client") or "").lower()
client_label = recent.get("clientLabel") or ""
if provider_label != "Halo Cursor":
    fail(f"providerLabel want Halo Cursor got {provider_label!r}")
if "Halo-Cursor-smoke" not in ua and "cursor-agent" not in ua:
    fail(f"request UA missing smoke marker: {ua!r}")
if client not in {"cursor"} and "cursor" not in client_label.lower():
    fail(
        f"client not matched as Cursor: client={client!r} label={client_label!r} ua={ua!r}"
    )
ok(
    f"request identity provider={provider_label} account={logged_account or account_name} "
    f"model={logged_model} client={client_label or client}"
)
print("SMOKE PASS")
PY
