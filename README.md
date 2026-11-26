# Halo Cursor

Agent2API WASM 插件仓库：https://github.com/postbiancheng/halo-cursor

用 Cursor 订阅账号转发对话、查询用量。Provider id：`halo-cursor`。

协议行为对照 Magpie 社区插件 [`@magpie-community/opencode-cursor-auth`](https://github.com/magpie-community/plugins/tree/main/packages/cursor)（MIT），Rust 干净重写，不搬 JS。

## 能力

| 能力 | 说明 |
|---|---|
| 登录 | 浏览器 Deep Control（PKCE + `/auth/poll`），或导入 Integrations API Key |
| 模型 | 登录后 `AiService/AvailableModels`；缓存约 10 分钟；失败回退内置默认 |
| 对话 | 固定 HTTP/1.1：`AgentService/RunSSE` + `BidiService/BidiAppend` |
| 用量 | 套餐窗口 + On-demand（`GetHardLimit`）+ 活动赠金 grants + Grok Bot Sand |
| 设置 | clientVersion、teamId、默认推理强度 |
| 错误 | classify / retry.decision：鉴权失效、额度/限流、内容策略 |

## 与 Magpie 的差异（稳定性）

- **不做 HTTP/2 优先**：WASM 宿主以 HTTP/1.1 为主，避免 H2/H1 探测切换导致的慢与失败。
- **模型目录强缓存**：避免每次对话先卡在 AvailableModels。
- **心跳 + stall 超时**：长时间无帧明确失败，便于宿主重试。
- **不读 Keychain / 不调 cursor-agent**：WASM 沙箱无本机凭据与子进程。

## 构建

```bash
./scripts/pack.sh
```

产物：`dist/agent2api-wasm-halo-cursor-<version>.zip`（含 `assets/logo.svg`）

## 冒烟

本机 Agent2API（默认 `127.0.0.1:3065`）已安装当前版本、且有可用 Cursor 账号时：

```bash
./scripts/smoke-local.sh
```

短对话 + `auto` 长测（默认 ≥40s，可用 `HALO_CURSOR_AUTO_LONG_SECONDS` 调整）。发版前应过烟测。

## 安装

在 Agent2API 中安装上述 zip，或对本机已运行的实例：

```bash
# 示例：通过本地 API 安装（需 AGENT2API_API_KEY）
python3 - <<'PY'
import base64, json, urllib.request, os
key = os.environ["AGENT2API_API_KEY"]
zip_path = "dist/agent2api-wasm-halo-cursor-0.1.2.zip"
body = json.dumps({"packB64": base64.b64encode(open(zip_path, "rb").read()).decode()}).encode()
req = urllib.request.Request(
    "http://127.0.0.1:3065/api/plugins/install",
    data=body,
    headers={"Authorization": f"Bearer {key}", "Content-Type": "application/json"},
    method="POST",
)
print(urllib.request.urlopen(req, timeout=60).read()[:200])
PY
```

## 版本摘要

- **0.1.2**：对话按 `legacySlug` / GetUsableModels 选稳定 variant；`cli-` 客户端版本；早发响应头与 Max Mode 重试；错误 details 解析；`auto` 冒烟脚本
- **0.1.1**：用量补齐 On-demand、活动赠金 grants、Grok Bot（`get-sand-usage-status`，aside）
- **0.1.0**：首版——Deep Control / API Key 登录、模型目录、用量、RunSSE 对话转发与错误分类
