//! config_multi_env —— 环境由 `AURORA_ENV` 决定，一份环境一份配置 + 热更新。
//!
//! 运行：`cargo run --example config_multi_env --features watch`
//!
//! ```bash
//! # 当前生效的配置（每次请求都重新 load，所以热更新立即可见）
//! curl -s http://127.0.0.1:8091/config
//!
//! # 改一行 examples/config_multi_env/config/development.toml 的 app.name
//! # 等 ~300ms（防抖）后再请求，值就变了——不需要重启
//!
//! # 也可以手动触发一次重载
//! curl -s -X POST http://127.0.0.1:8091/config/reload
//! ```
//!
//! # 规则（见 `docs/08` §2）
//!
//! - 环境**只能**由 `AURORA_ENV` 指定，取值只有 4 个：
//!   `development` / `test` / `staging` / `production`（别名一律拒绝启动）；
//! - 只读 `config/{env}.{toml|yaml|yml|json}` **一个文件**，没有 `default.*` 叠加层；
//! - 配置文件里**没有** `[app].env`：环境不是配置项，写进去会直接报错；
//! - 优先级（低 → 高）：代码默认值 → `config/{env}.*` → 环境变量 `AURORA_*`。
//!
//! 本示例额外用 `AURORA_SERVER__TRUST_PROXY=true` 演示**环境变量优先级最高**。

use std::sync::Arc;

use axum::Extension;
use axum::extract::State;
use serde_json::json;

use aurora::prelude::*;

/// 配置目录（相对仓库根）。
const CONFIG_DIR: &str = "examples/config_multi_env/config";

/// `GET /config` —— 返回**当前**配置快照。
async fn show(State(state): State<AppState>) -> ApiResponse<serde_json::Value> {
    // 关键：每个请求重新 `load()`，才能看到热更新后的值
    let config = state.config.load();
    ApiResponse::ok(json!({
        "app.name": config.app.name,
        "app.env": config.env.as_str(),
        "server.addr": format!("{}:{}", config.server.host, config.server.port),
        "server.trust_proxy": config.server.trust_proxy,
        "telemetry.level": config.telemetry.level,
        "telemetry.format": format!("{:?}", config.telemetry.format),
    }))
}

/// `POST /config/reload` —— 手动触发一次重载（不依赖文件监听）。
async fn reload(
    Extension(watcher): Extension<Arc<ConfigWatcher>>,
) -> AppResult<ApiResponse<&'static str>> {
    watcher.reload_now()?;
    Ok(ApiResponse::ok("reloaded"))
}

fn main() -> anyhow::Result<()> {
    // 必须在启动任何线程前设置（edition 2024 下 `set_var` 是 unsafe）：
    // 1) 选择环境 → `preset` 据此读 `{CONFIG_DIR}/development.toml`，
    //    同时决定生产护栏是否生效（框架不知道目录，是下面 `preset::from_env_var` 给的）
    // 2) 演示环境变量覆盖：`AURORA_{SECTION}__{KEY}` 优先级最高
    // 想看生产那份配置：把下面改成 production 并补上
    //   AURORA_AUTH__JWT__SECRET=<≥32 字节>
    // 否则会因生产护栏（JWT 密钥必填）拒绝启动——这正是"环境决定护栏"的体现。
    unsafe {
        std::env::set_var("AURORA_ENV", "development");
        std::env::set_var("AURORA_SERVER__TRUST_PROXY", "true");
    }

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(run())
}

async fn run() -> anyhow::Result<()> {
    // 1. 构造配置来源：**框架不认识 `config/` 这个路径**，是这里显式给的。
    //    `preset` 只是可选的 4 环境约定（{env}.toml），想换成别的布局就自己
    //    `ConfigSource::new(env).file(...)`。
    let source = preset::from_env_var("AURORA_ENV", CONFIG_DIR)?;
    let config = source.load()?;
    init_tracing(&config.telemetry)?;

    // 2. 打印生效来源与取值（便于确认「哪一层赢了」）
    let file = format!("{CONFIG_DIR}/{}.toml", config.env.as_str());
    println!("配置目录：{CONFIG_DIR}（由本示例显式指定，框架不知道它）");
    println!(
        "  AURORA_ENV         = {} → {} {}",
        config.env.as_str(),
        file,
        if std::path::Path::new(&file).exists() {
            "已加载"
        } else {
            "缺失（仅用代码默认值 + 环境变量）"
        }
    );
    println!("生效值：");
    println!("  app.name           = {}", config.app.name);
    println!("  app.env            = {}", config.env);
    println!(
        "  server.addr        = {}:{}",
        config.server.host, config.server.port
    );
    println!(
        "  server.trust_proxy = {}（来自 AURORA_SERVER__TRUST_PROXY）",
        config.server.trust_proxy
    );
    println!("  telemetry.level    = {}", config.telemetry.level);

    // 3. 应用状态 + 路由
    //    热更新重放**同一个 source**：不会出现"启动读 A 文件、热更新读 B 文件"。
    let state = AppState::build(&config).await?;
    let watcher = Arc::new(ConfigWatcher::start(state.config.clone(), source)?);

    let routes = Router::new()
        .route("/config", get(show))
        .route("/config/reload", post(reload))
        .layer(Extension(Arc::clone(&watcher)));

    let app = build_router(AppRoutes::all_public(routes), state);

    let addr = config.server.socket_addr()?;
    println!("\n监听 {addr}；改 {file} 后无需重启（防抖 300ms）");
    serve(app, addr).await?;

    // 显式 drop，停止监听任务（`serve` 返回即进程退出，这里只是示范生命周期）
    drop(watcher);
    Ok(())
}
