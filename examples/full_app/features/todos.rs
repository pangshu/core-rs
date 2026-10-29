//! 待办 feature：DB 持久化 + 缓存 + 队列投递 + 实时广播。
//!
//! 一条创建请求会串起四个能力：
//! `INSERT（db）→ 失效缓存（cache）→ 投递消息（queue）→ 广播 SSE（realtime）`。

use std::time::Duration;

use axum::Extension;
use axum::extract::{Path, State};
use serde::{Deserialize, Serialize};

use aurora::db::sea_orm::{ConnectionTrait, DatabaseBackend, Statement, Value};
use aurora::http::Json;
use aurora::prelude::*;

use crate::state::DemoCtx;

/// 列表缓存键与 TTL。
const LIST_CACHE_KEY: &str = "todos:list";
const LIST_TTL: Duration = Duration::from_secs(5);
/// 队列名：待办创建事件。
const QUEUE: &str = "todos.created";
/// 实时频道名。
const TOPIC: &str = "todos";

/// 待办。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Todo {
    /// 主键。
    pub id: i64,
    /// 标题。
    pub title: String,
    /// 是否完成。
    pub done: bool,
    /// 创建时间（RFC 3339）。
    pub created_at: String,
}

/// 创建请求。
#[derive(Debug, Deserialize)]
pub struct CreateTodo {
    /// 标题。
    pub title: String,
}

/// `GET /api/v1/todos` —— 列表（5 秒缓存）。
pub async fn list(State(state): State<AppState>) -> AppResult<ApiResponse<Vec<Todo>>> {
    // 缓存命中直接返回；缓存故障时降级为查库
    if let Ok(Some(bytes)) = state.cache.get(LIST_CACHE_KEY).await
        && let Ok(todos) = serde_json::from_slice::<Vec<Todo>>(&bytes)
    {
        return Ok(ApiResponse::ok(todos));
    }

    let rows = state
        .db
        .query_all_raw(Statement::from_string(
            DatabaseBackend::Sqlite,
            "SELECT id, title, done, created_at FROM todos ORDER BY id",
        ))
        .await
        .map_err(db_error)?;

    let todos = rows
        .iter()
        .map(row_to_todo)
        .collect::<AppResult<Vec<_>>>()?;

    if let Ok(bytes) = serde_json::to_vec(&todos)
        && let Err(error) = state
            .cache
            .set(LIST_CACHE_KEY, &bytes, Some(LIST_TTL))
            .await
    {
        tracing::warn!(%error, "写入列表缓存失败（忽略）");
    }

    Ok(ApiResponse::ok(todos))
}

/// `GET /api/v1/todos/{id}` —— 单条。
pub async fn get_one(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> AppResult<ApiResponse<Todo>> {
    let row = state
        .db
        .query_one_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "SELECT id, title, done, created_at FROM todos WHERE id = ?",
            [Value::from(id)],
        ))
        .await
        .map_err(db_error)?
        .ok_or_else(|| AppError::not_found(format!("todo {id} 不存在")))?;

    Ok(ApiResponse::ok(row_to_todo(&row)?))
}

/// `POST /api/v1/todos` —— 创建（需认证；带 `Idempotency-Key` 时自动幂等）。
pub async fn create(
    State(state): State<AppState>,
    Extension(ctx): Extension<std::sync::Arc<DemoCtx>>,
    _principal: Principal,
    Json(request): Json<CreateTodo>,
) -> AppResult<ApiResponse<Todo>> {
    let title = request.title.trim();
    if title.is_empty() {
        return Err(AppError::biz(ErrorCode::INVALID_PARAM, "title 不能为空"));
    }

    let created_at = now_utc().to_rfc3339();
    // `RETURNING id` 而不是 `last_insert_rowid()`：后者是**连接级**状态，
    // 连接池可能把查询派给另一条连接，拿到的会是 0。
    let id = state
        .db
        .query_one_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "INSERT INTO todos (title, done, created_at) VALUES (?, 0, ?) RETURNING id",
            [Value::from(title), Value::from(created_at.clone())],
        ))
        .await
        .map_err(db_error)?
        .ok_or_else(|| AppError::internal(anyhow::anyhow!("INSERT ... RETURNING 无返回")))?
        .try_get::<i64>("", "id")
        .map_err(db_error)?;

    let todo = Todo {
        id,
        title: title.to_string(),
        done: false,
        created_at,
    };

    // 失效列表缓存
    if let Err(error) = state.cache.del(LIST_CACHE_KEY).await {
        tracing::warn!(%error, "失效列表缓存失败（忽略）");
    }

    // 投递到队列（失败不影响主流程）
    if let Ok(payload) = serde_json::to_vec(&todo)
        && let Err(error) = state.queue.producer(QUEUE).publish(payload).await
    {
        tracing::warn!(%error, "投递队列失败（忽略）");
    }

    // 广播到实时频道（SSE / WS 订阅者都能收到）
    match RealtimeMessage::json(&todo) {
        Ok(message) => {
            let _ = ctx.hub.publish(TOPIC, message.with_event("todo.created"));
        }
        Err(error) => tracing::warn!(%error, "实时消息序列化失败（忽略）"),
    }

    Ok(ApiResponse::ok(todo))
}

/// `DELETE /api/v1/todos/{id}` —— 删除（需认证）。
pub async fn delete(
    State(state): State<AppState>,
    _principal: Principal,
    Path(id): Path<i64>,
) -> AppResult<ApiResponse<()>> {
    let result = state
        .db
        .execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "DELETE FROM todos WHERE id = ?",
            [Value::from(id)],
        ))
        .await
        .map_err(db_error)?;

    if result.rows_affected() == 0 {
        return Err(AppError::not_found(format!("todo {id} 不存在")));
    }

    if let Err(error) = state.cache.del(LIST_CACHE_KEY).await {
        tracing::warn!(%error, "失效列表缓存失败（忽略）");
    }
    Ok(ApiResponse::ok(()))
}

fn row_to_todo(row: &aurora::db::sea_orm::QueryResult) -> AppResult<Todo> {
    Ok(Todo {
        id: row.try_get("", "id").map_err(db_error)?,
        title: row.try_get("", "title").map_err(db_error)?,
        done: row.try_get::<i64>("", "done").map_err(db_error)? != 0,
        created_at: row.try_get("", "created_at").map_err(db_error)?,
    })
}

fn db_error(error: impl std::fmt::Display) -> AppError {
    AppError::internal(anyhow::anyhow!("db error: {error}"))
}
