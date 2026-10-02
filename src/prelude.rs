//! 一站式导入：业务项目里 `use core_rs::prelude::*;` 即可拿到全部常用类型，
//! 包括 axum / sea_orm / serde 等底层 crate 的常用项（下游无需直接依赖它们）。

pub use crate::app::{Application, ApplicationBuilder};
pub use crate::cache::Cache;
pub use crate::config::{
    AppConfig, CacheConfig, CompressionConfig, DatasourceConfig, LogConfig, MemoryConfig,
    RedisConfig, ServerConfig,
};
pub use crate::error::{AppError, AppResult};
pub use crate::orm::crud::{Crud, CrudExt, PkOf};
pub use crate::orm::page::Page;
pub use crate::orm::search::{SearchApply, SearchQuery};
pub use crate::orm::Db;
pub use crate::state::AppState;
pub use crate::web::extract::{PageQuery, ValidJson};
pub use crate::web::response::{ApiResult, ApiResponse, CODE_OK};

#[cfg(feature = "jwt")]
pub use crate::config::JwtConfig;
#[cfg(feature = "jwt")]
pub use crate::security::{Claims, CurrentUser, Jwt};
#[cfg(feature = "http-client")]
pub use crate::httpc::HttpClient;
#[cfg(feature = "otel")]
pub use crate::config::OtelConfig;
#[cfg(feature = "queue")]
pub use crate::queue::{Message as QueueMessage, Queue, QueueError, QueueHandle};

pub use axum::{
    extract::{ConnectInfo, Path, Query, State},
    routing::{delete, get, post, put},
    Json, Router,
};
pub use chrono;
pub use sea_orm::{
    ActiveModelTrait, ActiveValue, ConnectionTrait, ConnectOptions, Database, DatabaseConnection,
    DbErr, DeriveEntityModel, EntityTrait, PaginatorTrait, QueryFilter, Set,
};
pub use serde::{Deserialize, Serialize};
pub use serde_json;
pub use tracing;
pub use validator::Validate;
