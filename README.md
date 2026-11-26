# Halo Cursor

Agent2API WASM 插件仓库：https://github.com/postbiancheng/halo-cursor

用 Cursor 订阅账号转发对话、查询用量。Provider id：`halo-cursor`。

协议行为对照 Magpie 社区插件 [`@magpie-community/opencode-cursor-auth`](https://github.com/magpie-community/plugins/tree/main/packages/cursor)（MIT），Rust 干净重写，不搬 JS。

## 能力

| 能力 | 说明 |
|---|---|
| 登录 | 浏览器 Deep Control（PKCE + `/auth/poll`），或导入 Integrations API Key |
| 模型 | 登录后 `AiService/AvailableModels`；常规刷新间隔 10 分钟，保留最近一次成功目录最多 7 天 |
| 对话 | `AgentService/RunSSE` + `BidiService/BidiAppend`，走 WASM 宿主 HTTP |
| 用量 | 套餐窗口 + On-demand（`GetHardLimit`）+ 活动赠金 grants + Grok Bot Sand |
| 设置 | clientVersion、teamId、默认推理强度 |
| 错误 | 区分鉴权、额度耗尽、瞬时限流、上游容量及中途断流 |

## 转发行为

当前使用 Cursor 的 HTTP 兼容路径，上行发给 `BidiAppend`，下行读 `RunSSE`。Cursor 的原生 `AgentService/Run` 使用 HTTP/2 双向 Connect 流，接入它需要宿主在同一请求中持续写入和读取。

- 对话优先读取账号缓存，模型发现由 `models.refresh` 执行。目录暂时不可用时仍可尝试最近一次成功的模型映射。
- 文本流按 Connect 结束帧判定成功。工具调用根据 Cursor 声明的批次收尾，调用方在下一次请求附上工具结果。流已经输出正文、推理或工具调用时发生错误，按部分输出失败处理。
- 支持 gzip Connect 帧和分片交付。请求正文限制为 4 MiB，图片等非文本内容返回请求错误。`tool_choice: none` 禁用本轮工具。`max_tokens`、`temperature`、`parallel_tool_calls`、指定工具及强制工具调用目前没有 Cursor 参数映射。
- RunSSE 不向宿主传 `timeout_ms`（该字段会给上游下载加硬性总期限，长流会被提前掐断）；BidiAppend 仍用 60 秒总期限。也不向 Cursor 发送 `connect-timeout-ms`。空块读取会短暂退避，避免无上游进展时忙等耗尽 WASM 燃料窗口。长流依赖宿主 stream-idle 与插件自身的首包（60 秒）/空闲（90 秒）检查。
- 宿主 reqwest/rustls 访问 `RunSSE` 时，上游常在约 30 秒切断连接（同账号下 Magpie/Node fetch 无此现象）。插件在已有正文、无工具调用时会自动开新的 RunSSE 续写，最多 16 次，客户端仍看到同一条流。
- API Key 在到期前可自动兑换新令牌。浏览器登录到期后需要重新授权。

## 构建

```bash
./scripts/pack.sh
```

产物：`dist/agent2api-wasm-halo-cursor-<version>.zip`（含 `assets/logo.svg`），另有 `.sha256` 校验文件和 `.build-info` 构建记录。

## 冒烟

本机 Agent2API（默认 `127.0.0.1:3065`）已安装当前版本、且有可用 Cursor 账号时：

```bash
./scripts/smoke-local.sh
```

在能将 `auto` 路由到 Halo Cursor 的实例上运行。脚本通过请求记录分别核对短测和长测的提供商。两条流都必须收到正文、结束原因和 `[DONE]`。长流从第一条到最后一条正文增量默认至少 40 秒，可用 `HALO_CURSOR_AUTO_LONG_SECONDS` 调整。脚本记录首个正文的耗时。

离线回归可用 `cargo test --locked`，构建 WASM 后运行 `node --test tests/forward-wasm.mjs`。

## 安装

在 Agent2API 中安装上述 zip，或对本机已运行的实例：

```bash
# 示例：通过本地 API 安装（需 AGENT2API_API_KEY）
python3 - <<'PY'
import base64, json, urllib.request, os
key = os.environ["AGENT2API_API_KEY"]
zip_path = "dist/agent2api-wasm-halo-cursor-0.1.6.zip"
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

- **0.1.6**：RunSSE 空块读取增加短暂退避，避免忙等耗尽 WASM 燃料窗口。
- **0.1.5**：模型目录明确声明不支持图片，与文本请求校验保持一致，避免图片请求被错误路由到 Cursor。
- **0.1.4**：目录顶层声明 `supportsToolCall` 与 token 上限，避免路由组因「工具调用能力未声明」剔空成员。
- **0.1.3**：修复流结束判定和 Max Mode 重试，支持跨帧工具批次与 gzip，移出对话中的网络目录刷新；RunSSE 传输层约 30 秒切断时自动续写；并加强重试、冒烟和构建校验。

- **0.1.2**：对话按 `legacySlug` / GetUsableModels 选稳定 variant；`cli-` 客户端版本；早发响应头与 Max Mode 重试；错误 details 解析；`auto` 冒烟脚本
- **0.1.1**：用量补齐 On-demand、活动赠金 grants、Grok Bot（`get-sand-usage-status`，aside）
- **0.1.0**：首版——Deep Control / API Key 登录、模型目录、用量、RunSSE 对话转发与错误分类
