//! 一站式导入：业务项目里 `use core_rs::prelude::*;` 即可拿到全部常用类型，
//! 包括 axum / sea_orm 等底层 crate 的常用项（下游无需直接依赖它们）。

// 框架核心
pub use crate::app::{App, FromCore};
pub use crate::error::{AppError, AppResult};
pub use crate::state::CoreState;
pub use crate::traits::{HasAuth, HasCache, HasConfig, HasDb, HasHealthChecks, HasQueue};
#[cfg(any(feature = "ws", feature = "sse"))]
pub use crate::traits::HasRealtime;

// 配置
pub use crate::config::{
    self, ConfigHandle, Environment, LoadOptions, OnChange, Settings,
};

// web 层
pub use crate::error::ValidationItem;
pub use crate::web::{
    ApiResult, ApiResponse, ClientIp, CurrentUser, Page, PageQuery, RequestContext,
    ValidatedJson, CODE_OK,
};

// db
pub use crate::db::{
    cursor::{CursorPage, CursorQuery},
    paginate::{PageParams, Paginated},
    Crud, CrudExt, PkOf,
};

// cache
pub use crate::cache::{Cache, CacheError, CacheExt, CacheHandle};

// queue
pub use crate::queue::{Message as QueueMessage, Queue, QueueError, QueueHandle};

// observability
pub use crate::observability::{HealthCheck, HealthStatus};

// utils
pub use crate::utils::snowflake::Snowflake;
pub use crate::utils::time;

// 底层 crate 常用项（下游无需直接依赖）
pub use axum::{
    extract::{ConnectInfo, Path, Query, State},
    routing::{delete, get, post, put},
    Json, Router,
};
#[cfg(feature = "ws")]
pub use axum::extract::ws::WebSocketUpgrade;
pub use chrono;
pub use sea_orm::{
    ActiveModelTrait, ActiveValue, ColumnTrait, ConnectionTrait, ConnectOptions, Database,
    DatabaseConnection, DbErr, DeriveEntityModel, EntityTrait, PaginatorTrait, QueryFilter,
    QueryOrder, Set, TransactionTrait,
};
pub use serde::{Deserialize, Serialize};
pub use serde_json;
pub use tracing;
