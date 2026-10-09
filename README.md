# core-rs

Rust Web 应用框架：集成 axum / SeaORM 2 / 配置热更新 / 中间件 / 认证授权 / 可插拔缓存与队列。
一个依赖、一个 toml、十几行 main.rs 起服务；应用只写业务。

> 设计文档：`docs/02-Web应用框架目录结构设计.md`（本仓库即该设计的落地实现）。
> **使用指南：`docs/03-框架使用指南.md`** —— 各模块的用法、配置项与常见坑，配 `examples/demo` 食用。

## 总体思路

- **框架与应用分层**：框架不含任何业务词汇。判断标准一句话：**把项目名换掉、这段代码仍一字不改 → 进框架；代码里出现业务词 → 留在应用。**
- **框架只做零件和装配点，不强制约定**：中间件、响应封装、分页、事务助手全部可选，挂什么、挂哪层由应用的路由树决定。
- **外部依赖一律可插拔**：缓存、锁、队列都是 trait + 工厂，由配置选择后端，业务代码只依赖 trait。
- **认证与授权分离**：`auth/` 管「你是谁」（session / jwt / oauth2），`authz/` 管「能做什么」（Casbin RBAC）。
- **解耦点**：应用 `AppState` 内嵌框架 `CoreState`；框架中间件对状态只要求实现 `HasDb / HasCache / HasQueue / HasConfig / HasAuth` 等 trait。

## 快速开始

`main.rs`（完整可编译示例见 `examples/demo`）：

```rust
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    App::<AppState>::bootstrap()?      // APP_ENV → 多环境配置(含热更新) → tracing → 连接池 → 缓存 → 队列
        .consumer("email.send", |msg| async move { Ok(()) })  // 队列消费
        .task(Job::async_fn("cleanup", || async { /* ... */ }))    // 定时任务（scheduler feature）
        .mount(user_routes)            // 用户端路由树
        .mount(admin_routes)           // 管理端路由树
        .serve()                       // 优雅停机：Ctrl-C / SIGTERM
        .await
}
```

`AppState` 内嵌 `CoreState`（一行一个 trait）：

```rust
#[derive(Clone)]
pub struct AppState { pub core: CoreState }
impl From<CoreState> for AppState { fn from(core: CoreState) -> Self { Self { core } } }
impl HasDb for AppState { fn db(&self) -> Option<&DatabaseConnection> { self.core.db() } }
// HasCache / HasQueue / HasConfig / HasAuth / HasHealthChecks 同理
```

## 配置

- 目录 `config/`：`default.toml` → `{env}.toml`（`APP_ENV` = development/testing/staging/production）→ 环境变量（`APP_SERVER__PORT=9090` 覆盖 `server.port`）→（可选）配置中心（feature `config-remote`）。
- 所有子系统配置节集中在 `config::sections`；应用用 `#[serde(flatten)]` 追加业务节。
- 敏感项（DB 密码、JWT secret）**只走环境变量，不写入 toml**。
- 热更新：文件变更 → 重载校验 → `ArcSwap` 原子替换 → 订阅回调；**校验失败保留旧值**（fail-safe）。

## Feature 一览

| feature | 说明 | 默认 |
|---|---|---|
| `cache-memory` / `cache-redis` | 进程内缓存+进程内锁 / deadpool-redis + SET NX PX 分布式锁 | memory |
| `queue-memory` / `queue-redis` / `queue-rabbitmq` / `queue-kafka` / `queue-nats` | 队列后端（tokio mpsc / Redis Stream 消费组 / lapin / rdkafka / JetStream） | memory |
| `sqlite` / `mysql` / `postgres` | sea-orm 运行时后端 | sqlite |
| `watch` | 配置热更新（notify 监听 + 防抖） | ✓ |
| `session` / `jwt` / `oauth2` | 认证方式（可并存：`[auth].mode = "jwt,session"`） | — |
| `casbin` | RBAC 授权（模型/策略配置集中于 `[authz]`） | — |
| `ws` / `sse` | WebSocket / Server-Sent Events（hub 频道广播，可经 queue 跨实例转发） | — |
| `scheduler` | cron 定时任务（tokio-cron-scheduler，多实例经 cache/lock 选主） | — |
| `i18n` | Fluent 多语言 / 时区 / 货币（`{dir}/{locale}.ftl`） | — |
| `time` | 展示时区（`[time].timezone` 可选；日志/cron/展示解析链） | 常驻 |
| `metrics` / `otel` | Prometheus `/metrics` / OTLP gRPC 链路导出（`OTEL_EXPORTER_OTLP_ENDPOINT`） | — |
| `rate-limit` / `csrf` | 固定窗口限流（阈值热更新） / 双提交 Cookie CSRF | — |
| `config-remote` | 配置中心来源 | — |
| `log-file` | rotate-rs 滚动文件日志（大小/时间/混合切割 + gz） | — |
| `testing` | `TestApp::new()` 测试装配器 | — |

`cargo check --no-default-features --features "cache-memory,queue-memory,sqlite,…" ` 按需组合。

## 内置中间件与推荐装配顺序

`App::serve` 按 `ServerSettings` 自动装配（由外到内）：

```text
panic → request_id → trace → locale → access_log → security_headers
      → body_limit → timeout → cors → ip_filter → rate_limit
      → idempotency → csrf → auth → authz
```

- `request_id`：允许前端带入（`X-Request-Id`），跨层/跨系统关联；
- `trace_id`：**始终服务端生成**，不信任外部传入（otel feature 下采纳合法 W3C traceparent）；
- 组合权留应用：需要时在路由树上自行 `.layer(...)`。
- **路由保护由应用决定**：`App::mount` 只做合并、不套鉴权层；需要登录态的子树自行
  `.layer(auth::require_identity_layer())`（框架不做「公开 / 受保护」分类）。

## 内置端点

`/health`（liveness）、`/ready`（readiness：db / cache / queue + 自定义探针）、
`/metrics`（metrics feature）—— 由框架自动挂载，不在应用路由树里出现。

## 目录

```text
src/
├── app.rs / state.rs / traits.rs   # App 构建器 · CoreState · Has* 解耦点
├── config/                         # 多环境 + 集中解析 + 热更新（sections×11）
├── web/                            # ApiResponse / AppError / 提取器 / garde / RequestContext
├── middleware/                     # 13 件中间件（推荐顺序见上）
├── db/                             # pool / with_txn / Crud 审计软删约定 / 分页 / 游标 / 迁移
├── auth/ authz/                    # session·jwt·oauth2·password / Casbin RBAC
├── cache/ queue/                   # 可插拔后端（trait + 工厂）
├── task/ resilience/ realtime/     # cron 调度 · 熔断重试降级舱壁 · WS/SSE
├── i18n/ observability/ security/  # 多语言 / 日志·追踪·健康·指标 / XSS·SQL·加密
└── testing/                        # TestApp 测试装配器
examples/demo/                      # 使用说明书：CRUD + 登录 + 队列 + 四环境配置（公开树/受保护树示范）
tests/                              # 框架集成测试
```

## 开发

```bash
cargo test                          # 单元测试（默认 feature）
cargo test --features testing      # + App 级集成测试
cd examples/demo && cargo run       # 示例应用（自带演示用建表，无需手动迁移）
```

demo 的路由保护示范：公开树 `/login` `/register` 不套层；受保护树 `/users` `/me` 自挂
`auth::require_identity_layer()`。跑起来后先 `POST /register` → `POST /login` 拿 token，
再带 `Authorization: Bearer <token>` 访问受保护接口（匿名会得到 401）。

## 后续演进

- `storage/`（S3 兼容对象存储）预留，待影音项目转码/上传回调需求落地。
- 0.x 阶段框架与应用同机迭代（path 依赖）；API 稳定后发 crates.io 或 git tag 锁版本。
- 模块再膨胀时把 `db/`、`cache/`、`auth/` 拆成 `core-rs-core`，门面 crate 只做 re-export。
