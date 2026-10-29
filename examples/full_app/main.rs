//! full_app —— 完整应用示例（可直接作为项目起点，见 `docs/13` §3）。
//!
//! 运行：
//!
//! ```bash
//! cargo run --example full_app --features full,db-sqlite
//! ```
//!
//! 覆盖能力：config / db（SQLite）/ auth（JWT）/ cache / queue / realtime（SSE + WS）/
//! metrics / 中间件栈 / 健康探针。
//!
//! ```bash
//! # 登录拿 token
//! curl -s -X POST http://127.0.0.1:8080/api/v1/auth/login \
//!      -H 'content-type: application/json' \
//!      -d '{"username":"admin","password":"admin123"}'
//!
//! # 受保护接口
//! curl -s http://127.0.0.1:8080/api/v1/me -H "authorization: Bearer $TOKEN"
//!
//! # 创建待办（带幂等键，重复提交只创建一条）
//! curl -s -X POST http://127.0.0.1:8080/api/v1/todos \
//!      -H "authorization: Bearer $TOKEN" -H 'Idempotency-Key: demo-1' \
//!      -H 'content-type: application/json' -d '{"title":"写文档"}'
//!
//! # 列表（5 秒缓存）/ 指标 / 实时事件
//! curl -s http://127.0.0.1:8080/api/v1/todos
//! curl -s http://127.0.0.1:8080/metrics | head
//! curl -N http://127.0.0.1:8080/events?topic=todos
//! ```
//!
//! ⚠️ 示例账号 `admin/admin123` 与 SQLite 文件 `aurora-full-app.db` 均为演示用途。

mod features;
mod router;
mod state;

use std::sync::Arc;
use std::time::Duration;

use aurora::db::sea_orm::{ConnectionTrait, DatabaseBackend, Statement};
use aurora::prelude::*;

use crate::state::DemoCtx;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // 1. 配置：示例自包含（`docs/13` §2）——未提供数据库时回退到本地 SQLite
    let mut config = preset::load_env_var("AURORA_ENV", "config").unwrap_or_default();
    if config.database.url.trim().is_empty() {
        config.database.url = "sqlite://aurora-full-app.db?mode=rwc".to_string();
    }
    // 示例自包含：未提供 JWT 密钥时用占位值（生产必须走 AURORA_AUTH__JWT__SECRET）
    if config.auth.jwt.secret.trim().is_empty() {
        config.auth.jwt.secret = "full-app-demo-secret-change-me".to_string();
    }
    config.telemetry.metrics.enabled = true;
    // 演示幂等：写操作带 `Idempotency-Key` 时回放首次结果
    config.http.idempotency.enabled = true;
    config.validate()?;

    // 2. 日志 / 追踪
    init_tracing(&config.telemetry)?;

    // 3. 应用状态（连接 db / cache / queue 并 ping，注册 /ready 探针）
    let state = AppState::build(&config).await?;
    init_schema(&state).await?;

    // 3.1 生命周期钩子：启动末预热 / 关闭前清理（启动按注册顺序，关闭按逆序）
    // 注意：资源连接已由 AppState::build 完成，钩子只承载「资源就绪后的动作」。
    let warm_db = state.db.clone();
    state.lifecycle.on_startup("todos-warmup", move || {
        let db = warm_db.clone();
        async move {
            let total = db
                .query_one_raw(Statement::from_string(
                    DatabaseBackend::Sqlite,
                    "SELECT COUNT(*) AS total FROM todos",
                ))
                .await
                .map_err(|error| error.to_string())?
                .and_then(|row| row.try_get::<i64>("", "total").ok())
                .unwrap_or(0);
            tracing::info!(total, "启动钩子：待办总数已预热");
            Ok::<(), String>(())
        }
    });
    state
        .lifecycle
        .on_shutdown("todos-consumer-stop", || async {
            // 真实项目在这里停掉消费者 / 反注册服务发现；演示里只记一条日志
            tracing::info!("关闭钩子：消费者已停止");
            Ok::<(), String>(())
        });
    state.lifecycle.run_startup().await?;

    // 4. 业务上下文 + 队列消费者
    let ctx = Arc::new(DemoCtx::new());
    spawn_todo_consumer(state.clone());

    // 5. 路由 + 中间件栈（`/health`、`/ready`、`/metrics` 自动挂载）
    let app = build_router(router::api(ctx), state.clone());

    let addr = config.server.socket_addr()?;
    tracing::info!(
        %addr,
        metrics = config.telemetry.metrics.enabled,
        "full_app 已启动（/health · /ready · /metrics · /api/v1/* · /events · /ws/{{topic}}）"
    );

    // 6. 启动（`[tls].enabled = true` 时走 HTTPS）
    let served = if config.tls.enabled {
        let tls = TlsService::from_config(&config.tls)?;
        serve_tls(app, addr, tls).await
    } else {
        serve(app, addr).await
    };

    // 7. 关闭钩子：服务已停止接收新请求，逆序释放（单个失败只记日志）
    state.lifecycle.run_shutdown().await;
    served?;
    Ok(())
}

/// 建表（示例用裸 SQL；真实项目用 `db::migrate_on_start` + SeaORM 迁移）。
async fn init_schema(state: &AppState) -> anyhow::Result<()> {
    state
        .db
        .execute_raw(Statement::from_string(
            DatabaseBackend::Sqlite,
            "CREATE TABLE IF NOT EXISTS todos (
                 id         INTEGER PRIMARY KEY AUTOINCREMENT,
                 title      TEXT    NOT NULL,
                 done       INTEGER NOT NULL DEFAULT 0,
                 created_at TEXT    NOT NULL
             )",
        ))
        .await?;
    tracing::info!("todos 表已就绪");
    Ok(())
}

/// 后台消费 `todos.created`：演示队列的生产-消费与 ack。
fn spawn_todo_consumer(state: AppState) {
    tokio::spawn(async move {
        let consumer = state.queue.consumer("todos.created");
        tracing::info!(queue = consumer.queue(), "队列消费者已启动");
        loop {
            match consumer.recv().await {
                Ok(Some(message)) => {
                    let payload = String::from_utf8_lossy(&message.payload);
                    tracing::info!(id = %message.id, %payload, "消费到待办创建事件");
                    if let Err(error) = consumer.ack(&message).await {
                        tracing::warn!(%error, "ack 失败");
                    }
                }
                Ok(None) => tokio::time::sleep(Duration::from_millis(200)).await,
                Err(error) => {
                    tracing::warn!(%error, "消费失败，稍后重试");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        }
    });
}
