# core-rs 整体规划

> Rust 后端快速开发基础框架（starter 型）。对标 Spring Boot + MyBatis-Plus 的省事模式：
> 一个依赖、一个 yml、十几行 main.rs 起服务。薄封装 axum + SeaORM + Redis，不重新造轮子。

## 一、定位与设计铁律

1. **薄封装**：只封装"初始化与胶水"，底层类型（`Router`、`DatabaseConnection`、redis 池）原样透出，随时可绕过。
2. **约定优于配置**：`Application::builder().run()` 零配置可跑（内置默认值），需要定制才写 yml；环境变量始终可覆盖。
3. **统一契约**：所有接口统一响应体、统一错误码、统一分页结构，由框架而非各项目决定。
4. **进框架标准**：只有"每个新项目都会原样复制一遍"的代码才进框架；一次性需求不进。热门能力用 feature 门控，不默认编译。

## 二、目标使用体验（DX 验收基准）

```toml
[dependencies]
core-rs = { git = "..." }   # 一个依赖
```

```rust
use core_rs::prelude::*;

#[tokio::main]
async fn main() -> AppResult<()> {
    Application::builder()
        .routes(user::routes())
        .run()      // 读 app.yml → 日志/DB池/Redis池 → 中间件 → 优雅停机 → 起服务
        .await
}
```

验收线：examples/demo 的 main.rs 保持 ≤ 20 行，clone 后零环境 `cargo run` 即通。

## 三、版本规划

### v0.1 骨架与 Web 基础 —— "跑起来"

| 模块 | 内容 |
|---|---|
| 工程骨架 | 单 crate 结构、prelude、re-export（axum/sea_orm/serde/validator） |
| `config` | app.yml 加载 + 环境变量覆盖 + 内置默认值 |
| `logging` | tracing 初始化，level/format 可配 |
| `app` | Application 构建器 + **优雅停机**（SIGTERM 后处理完在途请求再退出） |
| `web` | 统一响应 `ApiResponse<T>`、统一错误 `AppError`（含 validator 错误自动转 400）、内置中间件栈（Trace/CORS/超时/RequestId/请求体限制）、`ValidJson` 校验提取器、内置 `/health` `/ready` |
| `orm` | SeaORM 连接池 + `Db` 提取器 |
| `cache` | deadpool-redis 池 + `Cache` 提取器 + JSON 便捷读写 |

**验收**：demo（users CRUD + 健康检查）零环境跑通；无 JWT 依赖时 feature 编译干净。

### v0.2 数据层完备 —— "好用来"（第一档热门功能）

| 功能 | 说明 |
|---|---|
| `CrudTrait` | 泛型增删改查（对标 BaseMapper），纯泛型实现、无需 derive 宏 |
| `Page<T>` + `PageQuery` | 统一分页结构（records/total/pages）与请求参数提取 |
| **自动填充** | insert 填 `create_time`、update 填 `update_time`，CrudTrait 内做 |
| **逻辑删除** | `deleted` 字段约定，delete 默认转 update，查询自动过滤 |
| **迁移集成** | sea-orm-migration：启动可选自动执行 + CLI 子命令手动执行 |
| **SQL 日志** | dev 打印 SQL 与耗时、慢 SQL 阈值告警 |

**验收**：demo 展示分页/逻辑删除/自动填充/迁移全链路，达到"新项目模板"级别。

### v0.3 安全与生态 —— feature 门控（第二档热门功能）

| feature | 能力 | 依托 crate |
|---|---|---|
| `jwt` | 签发/校验/Claims + `CurrentUser` 提取器（自动 401）+ argon2 密码哈希 | jsonwebtoken, argon2 |
| `swagger` | 从 handler 注解生成 OpenAPI + swagger-ui | utoipa |
| `metrics` | `/metrics` 端点 + 请求计数/耗时直方图 | metrics |
| `otel` | OpenTelemetry 链路导出 | tracing-opentelemetry |
| `http-client` | reqwest 封装：默认超时/重试/trace | reqwest |
| `scheduler` | cron 定时任务，配置驱动注册 | tokio-cron-scheduler |
| `rate-limit` | 按 IP/路由令牌桶 | tower-governor |
| `dist-lock` | redis SET NX + 过期 + 看门狗极简分布式锁 | 基于 redis |
| `websocket` | 带鉴权接线示例（不重封装） | axum 内置 |
| `upload` | multipart 文件接收 helper + ServeDir 静态资源一行配置 | tower-http |
| `cli` | `core-rs migrate` 等子命令入口（src/bin） | clap |

**验收**：每个能力独立 feature + 独立文档段 + demo 开关验证；关闭全部 feature 时依赖树回落到 v0.2 状态。

### v0.4 队列 / 热更新 / 搜索（已完成）

| 功能 | 说明 |
|---|---|
| `queue` | 消息队列抽象（对标 go-admin-core storage/Queue）：memory（有界 + 失败重试退避）/ redis（**Streams 消费组**：at-least-once、同组分摊、宕机 XCLAIM 接管、超限死信留 pending），`[queue].type` 配置选择；其他 MQ 实现 `Queue` trait 即可接入 |
| `watch`（默认开启） | 配置热更新：notify 监听 app.yml / profile 文件（父目录 + 文件名过滤 + 防抖），ArcSwap 原子切换 + OnChange 回调；重载失败保留旧配置；连接池等已初始化资源不自动重建 |
| `log-file` | 日志文件轮转（[rotate-rs](https://github.com/pangshu/rotate-rs)）：时间/大小/混合切割 + gz 压缩 + 非阻塞写入（block/drop 溢出策略） |
| 通用搜索 DSL | Django 风格 `字段__操作符=值` 查询参数 → SeaORM 条件：exact/ne/contains/icontains/startswith/endswith(含 i 变体)/gt/gte/lt/lte/in/isnull + sort 排序；列名白名单（实体真实列）防注入、值按列类型解析、未知操作符/非法值 400 |
| 缓存 incr/expire | `incr`（redis INCRBY / memory 按 key 串行）、`expire`（None = PERSIST；memory 重写条目实现） |
| JWT 刷新 | `orig_iat` 首签时间 + `max_refresh_hours` 预算；刷新不重置起点；过期判定 leeway=0（对齐 Go jwt-go）；过期响应 HTTP 401 + code 6401 |

**验收**：六项能力全部带集成/单元测试；默认 feature 集与 --all-features 均零警告编译、全部通过。

### v0.5+ 按需演进 backlog（第三档，有真实项目需要再做）

图形验证码 captcha（对接 cache 存储一次性校验）· Excel 导入导出（rust_xlsxwriter/calamine）·
对象存储 S3/MinIO · 其他 MQ 后端（rabbitmq/kafka，实现 `Queue` trait 即接入）·
多数据源/读写分离 · 多租户运行时 · casbin 权限/数据权限 · snowflake 分布式 ID ·
testcontainers 测试脚手架 · `cargo generate` 项目模板

## 四、最终目录形态（v0.3 全量，标注 feature）

```text
core-rs/
├── Cargo.toml                  # features: postgres/mysql/sqlite、jwt、swagger、metrics、otel、
│                               #           http-client、scheduler、rate-limit、dist-lock、upload
├── ROADMAP.md / README.md
├── src/
│   ├── bin/core.rs             # feature=cli：migrate 等子命令
│   ├── lib.rs / prelude.rs     # prelude 按 feature 条件导出
│   ├── app.rs / state.rs / error.rs / logging.rs
│   ├── config/                 # mod + server/datasource/redis/log（后续 jwt/scheduler 配置段）
│   ├── web/                    # mod/response/extract/middleware/health
│   ├── orm/                    # mod/pool/page/crud
│   ├── cache/                  # mod/pool
│   ├── security/               # feature=jwt：jwt/extractor（含 argon2）
│   ├── docs/                   # feature=swagger：utoipa 装配
│   ├── observe/                # feature=metrics、otel
│   ├── httpc/                  # feature=http-client
│   ├── scheduler/              # feature=scheduler
│   └── extra/                  # feature=rate-limit、dist-lock、upload
├── examples/demo/              # main.rs / user.rs / dto.rs / app.yml
└── tests/                      # 统一响应/错误映射/分页解析等集成测试
```

## 五、技术选型清单

| 用途 | crate | 备注 |
|---|---|---|
| Web | axum 0.8 + tower-http | 事实标准，tower 生态 |
| ORM | sea-orm 2.0（+ sea-orm-migration） | 基于 SQLx，保留 SQLx 逃生通道 |
| Redis | deadpool-redis | 池化 + 便捷封装 |
| 配置 | config | yml + env 覆盖 |
| 日志 | tracing + tracing-subscriber | 全生态事实标准 |
| 校验 | validator | derive 声明式 |
| 错误 | thiserror（框架内） | 应用层可用 anyhow |
| CLI | clap | migrate/new 子命令 |
| 备选 ORM | rbatis 4 | 若团队偏好 MyBatis 风格再切换，接口设计保持 ORM 无关性可留余地 |

## 六、待定决策（v0.1 动工前需确认）

1. **包名**：`core_rs` 或其他前缀。
2. **响应体约定**：`{ code, msg, data }` 成功 code 取 `0` 还是 `200`；错误码分段规则。
3. **默认数据库 feature**：default 是否含 `sqlite`（利于 demo 零环境跑）还是不含默认库。

## 七、参考项目

- [spring-rs](https://crates.io/crates/spring)：插件与配置绑定设计值得借鉴
- [loco.rs](https://loco.rs)：一体化开发体验与脚手架思路
