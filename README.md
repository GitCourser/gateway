# Gateway

基于 Rust（axum + tokio）的 OpenAI 兼容接口聚合网关。将客户端请求鉴权后路由到多个上游 OpenAI 兼容 API 提供商，支持模型映射与会话粘连。

## 功能特性

- **OpenAI 兼容接口**：`/v1/models`、`/v1/models/{id}`、`/health`，其余路径（如 `/v1/chat/completions`）原样透传至上游。
- **单文件 JSON 配置**：下游可见模型列表与上游提供商（含各自模型映射）集中在一个 JSON 文件中，启动时加载。
- **密钥隔离**：下游鉴权 key 存放在环境变量 `CLIENT_KEYS`；上游各提供商的 key 写在 JSON 配置中。
- **会话粘连路由**：同一会话（`session-id` 或任意 `*-session-id` 请求头）固定路由到同一上游。绑定保存在进程内存中，不设 TTL；容量由 `SESSION_MAX` 控制（默认 10），超出后先进先出逐出最旧绑定。
- **智能选号调度**：新会话并发请求各上游的 `GET /v1/account/select?model={model_id}`（10 秒超时），选取 `expire_at` 最早（最先过期）的有效账号，转发时附加 `x-account-id` 请求头。
- **流式透传**：完整支持 SSE 流式响应，响应体按流搬运不落盘。
- **CORS 全开放**：预检请求直接响应，所有响应附加 CORS 头。

## 配置文件

路径由环境变量 `CONFIG_FILE` 指定，默认 `./config.json`。参考 `config.example.json`：

```json
{
  "models": ["deepseek-v4.1-flash", "glm-5.3-flash", "qwen3.8-flash"],
  "providers": {
    "provider_a": {
      "url": "https://api.provider-a.com/v1",
      "key": "sk-upstream-a"
    },
    "provider_b": {
      "url": "https://api.provider-b.com",
      "key": "sk-upstream-b",
      "models": {
        "deepseek-v4.1-flash": "deepseek-flash"
      }
    }
  }
}
```

字段说明：

- `models`：允许向客户端暴露的模型列表（`/v1/models` 的数据来源）。
- `providers`：上游提供商字典，键为提供商 ID。
  - `url`：上游接口基础地址，支持带或不带 `/v1`，网关自动智能拼接路径。
  - `key`：向该上游请求时使用的 Bearer Token。
  - `models`（可选）：模型映射字典，键为下游请求的模型名，值为转发时实际替换的模型名。

配置在启动时一次性加载；文件缺失、JSON 非法或必填字段缺失时启动失败并打印明确错误。

## 环境变量

| 变量名 | 默认值 | 说明 |
| :--- | :--- | :--- |
| `CONFIG_FILE` | `config.json` | JSON 配置文件路径 |
| `CLIENT_KEYS` | 未设置时自动生成 | 下游客户端鉴权 key（单个）。未设置时启动自动生成随机 key 并打印到日志 |
| `SESSION_MAX` | `10` | 会话粘连绑定最大条数，超出后 FIFO 逐出最旧绑定 |
| `PORT` | `8080` | 监听端口 |
| `BIND` | `0.0.0.0` | 监听地址 |
| `RUST_LOG` | `info` | 日志级别（tracing 约定，如 `debug`、`gateway=trace`） |

## 构建与运行

```bash
cargo build --release
./target/release/gateway
```

典型部署：

```bash
export CONFIG_FILE=/etc/gateway/config.json
export CLIENT_KEYS=sk-my-gateway-key
export SESSION_MAX=50
export PORT=9000
./target/release/gateway
```

容器部署（容器内固定读取 /data/config.json，配置目录需挂载到 /data）：

```bash
docker run -d --name gateway -p 8080:8080 \
  -e CLIENT_KEYS=sk-my-gateway-key \
  -v /path/to/config-dir:/data \
  ghcr.io/your-org/gateway:latest
```

## 请求处理流程

1. **鉴权**：校验 `Authorization: Bearer <key>` 与 `CLIENT_KEYS` 一致，不一致返回 401。
2. **模型查询**：`GET /v1/models`、`GET /v1/models/{id}` 直接基于配置文件 `models` 响应，不请求上游。
3. **会话提取**：优先读取 `session-id` 请求头，其次查找任意以 `-session-id` 结尾的请求头（如 `x-session-id`）；无会话头则视为单次请求，每次重新选号且不落缓存。
4. **会话匹配**：提取到会话 ID 时先查内存绑定表；命中则直接复用（不改变 FIFO 位置）。
5. **新会话选号**：未命中时提取请求体中的 `model` 字段，向所有上游并行发起选号请求（超时 10 秒），上游需响应 `{ "account_id": 3, "expire_at": 1767225600 }`；选取 `expire_at` 最早的提供商并写入内存绑定表。同一会话的并发请求在锁内串行选号，保证同会话始终绑定同一上游。
6. **转发**：替换 `Authorization` 为上游 key、注入 `x-account-id`、剔除 `host` / `content-length` / `cf-*` 等头；命中模型映射时重写请求体中的 `model` 字段；响应状态码与流式 body 直接透传。

错误码约定：鉴权失败 401、请求体非法或缺 `model` 400、无可用上游或未配置提供商 503、上游请求失败 502。

## 测试

```bash
cargo test
```

单元测试覆盖：配置文件解析与错误路径、随机 key 格式、URL 智能拼接、模型映射、选号候选排序、会话 FIFO 逐出与并发去重、请求头过滤与模型重写。

## 部署限制

- 会话绑定保存在单进程内存中，重启后丢失（受影响的会话将重新选号）；不支持多实例部署——多实例各自维护内存绑定，无法保证会话粘连。
- 会话容量默认 10 条，可按内存预算通过 `SESSION_MAX` 调大。
