# core-rs

Rust 后端快速开发基础框架（starter 型）：**一个依赖、一个 yml、十几行 main.rs 起服务**。

薄封装 [axum](https://crates.io/crates/axum) + [SeaORM](https://crates.io/crates/sea-orm) + Redis，只封装"初始化与胶水"，底层类型原样透出，随时可绕过封装直接使用原生 API。

> 路线图见 [ROADMAP.md](ROADMAP.md)。当前进度：**v0.1（骨架）+ v0.2（数据层）+ v0.3（安全与生态）+ v0.4（队列/热更新/搜索）已完成**。

## 快速开始

```toml
[dependencies]
core-rs = { path = "../core-rs" }   # 或 git 依赖
```

`app.yml`：

```yaml
server:
  port: 8080
datasource:
  url: "sqlite://data.db?mode=rwc"   # 或 postgres:// / mysql://
```

`main.rs`：

```rust
use core_rs::prelude::*;

#[tokio::main]
async fn main() -> AppResult<()> {
    Application::builder()
        .routes(user::routes())
        .run()      // 配置 → 日志 → DB池 → Redis池 → 中间件 → 优雅停机
        .await
}
```

业务模块（完整示例见 `examples/demo/`）：

```rust
pub fn routes() -> Router<AppState> {
    Router::new().route("/users", get(page).post(create))
}

// 分页：?page=1&size=10 自动解析
async fn page(db: Db, q: PageQuery) -> ApiResult<Page<Model>> {
    let paginator = Entity::find().paginate(&db.0, q.limit());
    let total = paginator.num_items().await?;
    let records = paginator.fetch_page(q.page_index()).await?;
    Ok(ApiResponse::ok(Page::new(records, total, &q)))
}

// JSON + validator 校验一步到位，失败自动 400
async fn create(db: Db, ValidJson(dto): ValidJson<CreateUser>) -> ApiResult<Model> {
    ActiveModel { username: Set(dto.username), ..Default::default() }
        .insert(&db.0)
        .await
        .map(ApiResponse::ok)
}
```

跑 demo：`cargo run --example demo`，然后 `curl http://127.0.0.1:8080/health`。

## 内置能力（v0.1 + v0.2）

| 模块 | 内容 |
|---|---|
| `app` | `Application::builder()`：config → logging → DB/Redis 池 → **迁移** → 路由 → 中间件 → 优雅停机（Ctrl+C / SIGTERM） |
| `config` | `app.yml` + profile 叠加（`CORE_PROFILE=prod` → `app-prod.yml`）+ 环境变量覆盖（`CORE_` 前缀，`__` 表示层级）+ 内置默认值 + 未知键启动告警；零配置可跑 |
| `web` | 统一响应 `{ code, msg, data }`（成功 code=0）；统一错误 `AppError` 自动映射 HTTP 状态码；内置中间件：**panic 捕获(统一500)** / RequestId / Trace / 超时(408) / 请求体限制 / CORS |
| `web::health` | `/health` 存活探针、`/ready` 就绪探针（含 db/redis 组件状态，任一已配置组件 down 时返回 503） |
| `orm` | SeaORM 连接池 + `Db` 提取器 + `Page<T>` 统一分页 + **`Crud` 泛型增删改查** + **迁移集成** + **`orm::tx` 事务封装**（Ok 提交 / Err 回滚，事务内可复用 Crud） |
| `cache` | deadpool-redis 池 + `Cache` 提取器 + JSON 便捷读写 + **`get_or_load` cache-aside**（缓存故障降级为 miss 不传染，进程内按 key 串行防击穿） |
| `prelude` | 一站式导入，并转发 re-export axum / sea_orm / serde / serde_json / tracing / validator |
| `app` 构建器 | 还可叠加：`.openapi()`（swagger）、`.cron_job()` / `.cron_job_distributed()`（scheduler） |

### Crud 泛型增删改查（v0.2）

对标 MyBatis-Plus BaseMapper，**零 derive 宏、零注册**——按列名约定自动生效：

| 约定列 | 效果 |
|---|---|
| `created_at` | insert 时自动填充当前时间（未显式赋值时） |
| `updated_at` | insert / update 时自动填充 |
| `deleted` | bool 逻辑删除：查询自动过滤、delete 转置位更新、update 拒绝修改已删行；无此列的实体则物理删除 |

```rust
// handler 里一行搞定分页（自动过滤已删行、size 限幅 1..=100）
async fn page(db: Db, q: PageQuery) -> ApiResult<Page<Model>> {
    let page = Entity::crud().page(&db.0, &q).await?;
    Ok(ApiResponse::ok(page))
}

// 插入时 created_at/updated_at/deleted 全部自动填充
let mut am = <ActiveModel as ActiveModelTrait>::default();
am.username = Set(dto.username);
let model = Entity::crud().insert(&db.0, am).await?;
```

需要绕过约定时直接用 sea-orm 原生 `Entity::find()` / `ActiveModel`。

### 迁移集成（v0.2，feature = "migration"，默认开启）

```rust
Application::builder()
    .migrations(Migrator)   // 启动时自动执行未应用的迁移
    .run().await
```

迁移文件写法见 `examples/demo/migration.rs`。手动执行：在项目里加一个 bin
（约 5 行）调用 `core_rs::orm::migrate::up::<Migrator>(&db).await`，
`cargo run --bin migrate` 即可；`down` / `fresh` 同理。

### 可选能力（v0.3，全部 feature 门控、默认关闭）

| feature | 能力 | 一句话用法 |
|---|---|---|
| `jwt` | HS256 签发/校验 + `CurrentUser` 提取器（自动 401）+ **roles 角色与自定义 claims** + argon2 密码哈希 + **token 刷新（orig_iat 预算，过期返回 code 6401）** | `st.jwt.as_ref().unwrap().sign_with_roles("uid", roles)`；`user.require_any_role(&["admin"])?`；`jwt.refresh(&token)?` |
| `swagger` | OpenAPI 3.1 + swagger-ui（/swagger-ui、/api-docs/openapi.json） | `.openapi(api_doc())`，handler 加 `#[utoipa::path]` |
| `metrics` | /metrics + 请求计数/耗时直方图（method/path/status 维度） | 启用即生效，零代码 |
| `otel` | OpenTelemetry OTLP/gRPC 链路导出 | app.yml 配 `[otel].endpoint` |
| `http-client` | reqwest 封装（默认超时 + JSON 便捷方法） | `HttpClient::new(timeout)?.get_json::<T>(url)` |
| `scheduler` | cron 定时任务（秒开头 6/7 段表达式）+ **多实例分布式互斥**（配合 dist-lock + redis） | `.cron_job(...)`；多实例用 `.cron_job_distributed(...)` |
| `rate-limit` | 按客户端 IP 令牌桶（超限 429），支持反代后取真实 IP | app.yml 配 `[server.rate_limit]`，反代部署加 `key: proxy_headers` |
| `cache-memory` | 缓存内存后端（moka TTL 缓存），配合 `[cache].type` 使用 | app.yml 配 `[cache]`，详见「缓存双后端」 |
| `dist-lock` | redis 分布式锁（SET NX EX + token 校验释放 + 续期） | `cache.try_lock("key", ttl).await?` |
| `websocket` | 打通 axum ws（/ws 示例见 demo） | 直接用 `axum::extract::ws` |
| `upload` | multipart 流式落盘（uuid 重命名/限流/防穿越）+ /static 静态资源 | `save_multipart(&mut mp, dir, max).await` |
| `cli` | 命令行：`core-rs version`、`core-rs hash <pwd>`（argon2；省略 `<pwd>` 时从 stdin 读取，避免明文留在 shell history） | `cargo run --bin core-rs --features cli,jwt -- hash xxx` 或 `... -- hash < pwd.txt` |
| `test-util` | 测试辅助：`core_rs::test::memory_db_state()` 一行拿测试态 AppState（sqlite 内存库） | dev-dependencies 中启用，详见「测试辅助」 |

### v0.4 新增能力

| feature | 能力 | 说明 |
|---|---|---|
| `log-file` | **日志文件轮转**（[rotate-rs](https://github.com/pangshu/rotate-rs)） | 时间/大小/混合切割 + gz 压缩 + 非阻塞写入；app.yml 配 `[log.file]`，或环境变量 `RUST_LOG_FILE=1` |
| `watch`（默认开启） | **配置热更新** | 监听 `app.yml`（及 profile 文件）变更自动重载并原子切换 `state.config.load()`；`.on_config_change(\|cfg\| ...)` 注册回调；重载失败保留旧配置 |
| `queue` | **消息队列**（对标 go-admin-core storage/Queue） | `[queue] type: auto/memory/redis` 选择后端；memory = 进程内有界队列 + 失败重试退避；redis = **Redis Streams 消费组**（at-least-once、多实例分摊、宕机接管、死信 pending）。`.queue_task("topic", handler)` 注册，`state.queue.publish(...)` 发布 |
| （内置） | **通用搜索 DSL** | `?username__contains=li&age__gte=18&sort=-created_at`，`SearchQuery` 提取器 + `Entity::find().apply_search(&q)?` / `Entity::crud().page_searched(...)`；列白名单防注入、值按列类型解析 |
| （内置） | **缓存 incr / expire** | `cache.incr("key", 1)?`（redis 原生 INCRBY / memory 按 key 串行）、`cache.expire("key", Some(ttl))?`（None = PERSIST） |
| （内置） | **JWT 刷新** | token 携带 `orig_iat`；过期但仍在 `[jwt].max_refresh_hours` 预算内可 `jwt.refresh(&token)` 换新（不重置起点，防无限续期）；过期响应 HTTP 401 + code **6401**（前端据此触发刷新） |

搜索 DSL 示例：

```rust
// GET /users?username__contains=li&age__gte=18&status__in=1,2&sort=-created_at,name
async fn page(db: Db, q: PageQuery, s: SearchQuery) -> ApiResult<Page<Model>> {
    let page = Entity::crud().page_searched(&db.0, &q, &s).await?;
    Ok(ApiResponse::ok(page))
}
```

操作符：`exact/eq`（默认）、`ne`、`contains/icontains`、`startswith/istartswith`、`endswith/iendswith`、
`gt/gte/lt/lte`、`in`（逗号分隔）、`isnull`（true/false）。列名必须匹配实体真实列（白名单），
未知列静默忽略；未知操作符或值与列类型不符返回 400。

队列示例：

```rust
Application::builder()
    .queue_task("email.send", |msg| {
        Box::pin(async move {
            // msg.values 为 JSON 载荷；memory 后端失败重试 max_attempts 次（退避 1s/2s/3s），
            // redis 后端失败不 ACK、由 pending 列表接管重投（at-least-once，消费须幂等）
            send_email(&msg.values).await
        })
    })
    .run().await
```

按需组合，例如：

```toml
core-rs = { path = "../core-rs", features = ["jwt", "metrics", "rate-limit"] }
```

demo 一次性体验全部能力：

```bash
cargo run --example demo --features "jwt,swagger,metrics,scheduler,websocket,upload,queue"
# /auth/login(admin/secret123) → /me(Bearer token) → /swagger-ui → /metrics → /upload
# → /users?username__contains=x&sort=-id（搜索）→ /notify（队列）
```

JWT 注意事项（jsonwebtoken 11）：框架已启用 `rust_crypto` 纯 Rust 加密后端，
Windows 下无需 cmake/NASM。demo 的 `#[axum::debug_handler]` 依赖 axum 的
`macros` feature（框架已默认启用）。

### 统一响应约定

```json
{ "code": 0, "msg": "ok", "data": { } }
```

- 成功：`code = 0`；失败：`code` 等于对应 HTTP 状态码（400/401/403/404/500）。
- 500 类错误（数据库/缓存/内部）细节只记日志，对外统一返回 `internal server error`。

### 配置参考

加载优先级（后者覆盖前者）：内置默认值 → `app.yml` → `app-{profile}.yml` → `CORE_` 环境变量。
profile 由 `CORE_PROFILE=prod` 或 `.profile("prod")` 指定，用于叠加生产覆盖文件。

```yaml
server:
  host: 0.0.0.0
  port: 8080
  request_timeout_secs: 30
  body_limit: 2097152          # 字节
  shutdown_timeout_secs: 30    # 优雅停机最长等待；0 = 一直等在途请求
  compression:
    enabled: true              # 响应体 gzip 压缩
  cors:
    enabled: true
    allow_origins: ["*"]       # 或具体来源列表；带凭证时必须显式列表
    allow_methods: ["*"]       # 或 ["GET", "POST", "PUT", "DELETE"]
    allow_headers: ["*"]       # 或 ["Authorization", "Content-Type"]
    expose_headers: ["X-Request-Id"]
    allow_credentials: false
    max_age_secs: 3600         # 预检缓存；0 = 不发送

datasource:
  url: "postgres://user:pass@localhost:5432/app"
  max_connections: 10
  min_connections: 2           # 预热连接
  connect_timeout_secs: 5      # 池获取超时；0 = 底层默认 30s
  idle_timeout_secs: 300
  max_lifetime_secs: 1800
  sql_logging: true            # 以 Debug 级别打印每条 SQL（需 log.level 含 debug）
  slow_query_ms: 500           # 慢 SQL 阈值，超过以 Warn 记录；0 关闭

cache:
  type: auto                   # auto | redis | memory；auto = redis.url 非空走 redis 否则内存
  memory:
    max_capacity: 10000        # 条目数上限
    default_ttl_secs: 300      # 未显式传 ttl 时的默认过期；0 = 不过期

redis:
  url: "redis://127.0.0.1:6379"
  pool_size: 16                # 连接池上限，0 或缺省用 deadpool 默认值（CPU 数 × 4）

log:
  level: info                  # RUST_LOG 优先
  format: console              # console | json
  stdout: true                 # 是否输出到控制台（文件输出并存时二者兼得）
  service_name: "order-api"    # 可选：多服务日志聚合时的来源标识，缺省回落 otel.service_name
  file:                        # --features log-file：滚动文件输出
    enabled: true
    dir: logs
    name: app
    rotation: hybrid           # size | time | hybrid
    max_size_mb: 100
    interval_secs: 86400
    max_backups: 30            # 0 = 不清理
    compress: false            # 轮转旧文件 gzip 压缩
    non_blocking: true         # 后台线程落盘
    overflow: block            # block（背压）| drop（丢弃并计数）

watch:
  enabled: true                # --features watch（默认开启）：配置文件变更自动重载

queue:                         # --features queue
  type: auto                   # auto | memory | redis；auto = redis.url 非空走 redis 否则 memory
  memory:
    buffer: 1024               # 每 topic 缓冲条数，满则 publish 报错
    max_attempts: 3            # 失败重试次数（退避 1s/2s/3s）
  redis:                       # Redis Streams 消费组
    group: core-rs             # 同组分摊消费，异组各收一份
    consumer: ""               # 留空自动生成
    key_prefix: "core-rs:queue:"
    max_attempts: 3            # 超过留在 pending 列表（死信），人工 XACK/XDEL
    block_secs: 1
    claim_min_idle_secs: 30    # 宕机实例未 ACK 消息的接管阈值
    batch: 16

jwt:                           # --features jwt
  secret: "change-me"
  expire_hours: 24
  issuer: "core-rs"
  max_refresh_hours: 168       # 刷新预算：自首签起该窗口内可 refresh；0 = 禁用

app:                           # 应用自定义段：框架原样保留，业务代码按结构取出
  my_feature: true
  webhook: "https://example.com/hook"
```

环境变量覆盖示例：`CORE_SERVER__PORT=9090`、`CORE_DATASOURCE__URL=...`、`CORE_LOG__LEVEL=debug`、`CORE_APP__MY_FEATURE=false`。

### 缓存双后端

`Cache` 提取器与方法（`get_json` / `set_json` / `set_string` 等）对业务代码完全一致，
后端由 `[cache].type` 决定，应用只需在 yml 里切换，handler 零改动：

| type | 后端 | 说明 |
|---|---|---|
| `auto`（默认） | redis 或内存 | `redis.url` 非空走 redis，否则进程内存 |
| `redis` | deadpool-redis | 分布式锁（`dist-lock`）等 redis 专属能力可用 |
| `memory` | moka 进程内 TTL 缓存 | 需启用 `cache-memory` feature |

内存后端按条目 LRU + TTL 淘汰；仅 redis 支持的操作（如分布式锁）在 memory 后端报错而非静默降级。

### 配置热更新（v0.4，feature = "watch"，默认开启）

框架监听 `app.yml` 与 `app-{profile}.yml` 的变更（监听父目录按文件名过滤，编辑器
保存/原子替换都不会丢事件；300ms 防抖合并连发事件）：

- 重载成功 → 记录变化的顶层段（info）→ **原子切换**配置快照 → handler 里
  `state.config.load()` 拿到的总是当前生效配置 → 依次触发 `.on_config_change` 回调；
- 重载失败（yml 写坏）→ **保留旧配置**并记 error 日志，服务不受影响；
- `CORE_` 环境变量覆盖在每次重载时按原优先级重新应用；
- **已初始化的资源（DB/Redis 连接池、JWT 密钥等）不会自动重建**——避免生产连接被
  配置误改意外替换；需要响应变更时在回调里自行处理（回调在监听线程同步执行，请保持轻量）。

```rust
Application::builder()
    .on_config_change(|cfg| {
        tracing::info!(level = %cfg.log.level, "config reloaded");
    })
    .run().await
```

关闭方式：`.watch(false)` 构建器参数或 app.yml 配 `[watch] enabled = false`。

### 应用自定义配置段

各应用自己的配置写到 `[app]` 段，框架不解析、原样保留，环境变量覆盖同样生效：

```rust
#[derive(Deserialize, Default)]
struct MyAppCfg { order_timeout_secs: u64 }

let cfg: MyAppCfg = state.config.app_section::<MyAppCfg>()?.unwrap_or_default();
```

### trace_id 与 request_id

框架为每个请求维护两个相互独立的标识（缺失时自动生成，上游传入则沿用，响应头回传）：

| 标识 | 请求头 | 面向 | 用途 |
|---|---|---|---|
| `request_id` | `x-request-id` | 单次请求 | 幂等关联、客户端报障凭据 |
| `trace_id` | `x-trace-id` | 整条调用链 | **日志检索聚合**：多实例/多服务的日志按调用链串起来 |

两者作为 span 字段注入，**请求内的每条日志自动携带**（json 格式输出为 span 上下文）：

```json
{"level":"INFO","message":"user loaded","span":{"path":"/users/42","request_id":"...","trace_id":"...","service":"order-api"}}
```

分布式配置：

- `[log].service_name`（可选）：多服务日志进同一个采集后端时区分来源，回退链
  `[log].service_name` → `[otel].service_name` → `core-rs`；
- 多服务串联：上游服务把收到的 `x-trace-id` 传给下游即可延续同一条链；
- `otel` feature 部署：上游按 W3C 规范传 `traceparent` 头时，框架自动取其 trace-id
  段作为本请求的 `trace_id`，本地日志与导出的 OTel trace 直接互查。

### 生产韧性

针对小团队常见的生产事故点，框架内建以下防护（多数零配置生效）：

**缓存故障不传染**——`get_or_load` cache-aside 一站式读取，redis 抖动自动退化为直连 DB，
热门 key 并发 miss 时进程内合并为一次回源（防击穿）：

```rust
let user: User = cache
    .get_or_load("user:42", Some(Duration::from_secs(300)), || async {
        load_user(&db.0, 42).await   // miss（或缓存故障）时执行，结果自动回填
    })
    .await?;
```

**定时任务多实例不重复跑**——多实例部署时每个实例都在跑 cron？分布式互斥一开就好
（需 `dist-lock` + redis；抢不到锁的实例本轮跳过，redis 故障时宁可跳过也不重复执行）：

```rust
Application::builder()
    .cron_job_distributed("nightly-settle", "0 0 2 * * *", || Box::pin(async { /* ... */ }))
    .run().await
```

**其余内建防护**：

- handler panic 统一映射为 500 JSON（而非连接重置），细节只进日志；
- handler 用 `ConnectInfo<SocketAddr>` 可拿客户端地址（审计日志用，`prelude` 已导出）；
- 部署在 nginx/网关后时限流加 `key: proxy_headers`，按 X-Forwarded-For 等头取真实 IP，
  否则全站共享代理 IP 一个桶；
- 配置拼写错误（未知顶层键）启动时 stderr 告警，避免静默失效。

### 事务封装

`Ok` 提交、`Err` 回滚；Crud 方法在事务内直接复用（Rust 1.85+ async 闭包，免 Box::pin）：

```rust
orm::tx(&db, async |txn| {
    order::Entity::crud().insert(txn, order_am).await?;
    stock::Entity::crud().update(txn, stock_am).await?;
    Ok(())
}).await?;
```

### 测试辅助

业务项目在 `dev-dependencies` 启用 `test-util` feature，一行拿测试态 AppState：

```toml
[dev-dependencies.core-rs]
features = ["test-util"]
```

```rust
let state = core_rs::test::memory_db_state().await;   // sqlite 内存库 + 内存缓存
let app = Router::new().merge(user::routes()).with_state(state);
// 配合 tower::ServiceExt::oneshot 断言响应
```

需要表的用例先自行跑迁移或 `execute_unprepared` 建表。

### 数据库 feature

```toml
core-rs = { path = "../core-rs", default-features = false, features = ["postgres"] }
```

`default = ["sqlite"]`；可选 `sqlite` / `mysql` / `postgres`（可与 SeaORM 原生 feature 组合）。

## 设计铁律

1. **薄封装**：`Router` 还是 axum 的 `Router`，连接就是 `DatabaseConnection`，不造新抽象。
2. **约定优于配置**：`Application::builder().run()` 零配置可跑。
3. **一切可绕过**：`Db`/`Cache` 提取器拿到的就是原生连接/池。
4. **错误边界**：框架**启动期**可以 fail-fast（返回 Err 中止或对不变量 panic）；**运行期**框架自身只返回错误、不 panic，一切依赖输入/外部状态的失败都走 `Result`。应用代码是否 panic 由应用层决定——框架的 panic 捕获层只负责把 panic 兜底成统一 500，不改变应用的选择。

## 开发

```bash
cargo check --all-targets   # 编译检查
cargo test                  # 集成测试
cargo run --example demo    # 跑示例
```

MSRV：Rust 1.85（`Cargo.toml` 的 `rust-version` 已声明；CI 矩阵见 `.github/workflows/ci.yml`）。
