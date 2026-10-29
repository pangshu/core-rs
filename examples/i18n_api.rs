//! i18n_api —— M5 示例：语言协商 + Fluent 翻译 + 时区/货币格式化。
//!
//! 运行：`cargo run --example i18n_api --features i18n`
//!
//! ```bash
//! curl -s http://127.0.0.1:8080/hello                                   # 默认 zh-CN
//! curl -s -H 'Accept-Language: en-US' http://127.0.0.1:8080/hello       # Hello, world
//! curl -s -H 'Accept-Language: fr-FR' http://127.0.0.1:8080/hello       # 回退 zh-CN
//! curl -s http://127.0.0.1:8080/format                                  # 时区 / 货币
//! ```
//!
//! 语言资源在 `examples/locales/`（`{locale}.ftl`），文件名即受支持语言。

use axum::extract::State;
use axum::routing::get;

use aurora::app::AppState;
use aurora::i18n::Locale;
use aurora::prelude::*;

async fn hello(State(state): State<AppState>, locale: Locale) -> AppResult<ApiResponse<String>> {
    // 用协商出的语言取翻译；缺 key 时返回 key 本身（便于发现漏翻）。
    Ok(ApiResponse::ok(state.i18n.t(&locale, "hello")))
}

async fn format(State(state): State<AppState>) -> AppResult<ApiResponse<Vec<String>>> {
    let now = now_utc();
    Ok(ApiResponse::ok(vec![
        state.i18n.datetime(now), // 按 [i18n].timezone
        state.i18n.date(now),
        state.i18n.money(1234.5), // 按 [i18n].currency
    ]))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut config = preset::load_env_var("AURORA_ENV", "config")?;
    // 示例自带的语言资源目录（真实项目用 `[i18n].resource_dir`）
    config.i18n.resource_dir = "examples/locales".to_string();

    init_tracing(&config.telemetry)?;
    let state = AppState::build(&config).await?;

    let supported: Vec<&str> = state.i18n.supported().iter().map(Locale::as_str).collect();
    tracing::info!(?supported, default = %state.i18n.default_locale(), "i18n 就绪");

    let routes = Router::new()
        .route("/hello", get(hello))
        .route("/format", get(format));
    let app = build_router(AppRoutes::all_public(routes), state);

    let addr = config.server.socket_addr()?;
    tracing::info!(%addr, "i18n_api example starting");
    serve(app, addr).await?;
    Ok(())
}
