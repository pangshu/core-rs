//! crud_repository —— M2 示例：SQLite CRUD + 分页（Repository 抽象）。
//!
//! 运行：`cargo run --example crud_repository --features db,db-sqlite`
//!
//! 演示：
//! - 应用侧声明实体与 `Migrator`（迁移在启动期执行，`[database].migrate_on_start`）
//! - 业务侧 `Repository` trait + SeaORM 实现（可被测试替身替换）
//! - 框架 `Repository` 默认实现提供 Offset 分页；`cursor::paginate` 提供游标分页
//! - `AppState.db` 经 `State<DbPool>` 直接注入
//!
//! 数据库连接串来自 `AURORA_DATABASE__URL`（敏感项只走环境变量，见 `docs/08` §5）；
//! 未设置时示例回退到本地 `sqlite://aurora-dev.db?mode=rwc`。
//!
//! 表前缀来自 `AURORA_DATABASE__TABLE_PREFIX`（默认空，即不启用）：
//! 设成 `book` 时物理表变成 `book_users` / `book_order_items`，迁移记录表变成
//! `book_seaql_migrations`——同一份代码换个前缀就能和别的应用共用一个数据库。
//!
//! ```bash
//! curl -s -X POST -H 'Content-Type: application/json' \
//!      -d '{"email":"a@b.com","name":"Alice"}' http://127.0.0.1:8080/users
//! curl -s http://127.0.0.1:8080/users
//! curl -s 'http://127.0.0.1:8080/users/cursor?size=2'
//! curl -s http://127.0.0.1:8080/users/1
//! curl -s -X DELETE http://127.0.0.1:8080/users/1
//! ```

use axum::extract::State;
use sea_orm::ActiveModelTrait;
use sea_orm::ActiveValue::Set;
use sea_orm::EntityTrait;

use aurora::db::{Cursor, DbPool, Repository, cursor, require_found};
use aurora::errors::{AppError, AppResult};
use aurora::http::{ApiResponse, Json, Path, Query};
use aurora::prelude::*;
use aurora::utils::pagination::{CursorPage, Page};

/// 业务实体（由业务侧声明；框架不内置实体）。
pub mod user {
    use aurora::db::aurora_entity;
    use sea_orm::ActiveModelBehavior;
    use sea_orm::entity::prelude::*;
    use serde::Serialize;

    /// 逻辑表 `users`：物理表名 = `{table_prefix}_users`（前缀为空时即 `users`）。
    ///
    /// `aurora_entity` 替代 `DeriveEntityModel` + `table_name`：表名不再固化进
    /// 编译期，而是交给 `aurora::db::prefix` 在运行时解析。
    #[aurora_entity(table = "users")]
    #[derive(Clone, Debug, PartialEq, Eq, Serialize)]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub id: i64,
        pub email: String,
        pub name: String,
        pub created_at: DateTimeUtc,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

/// 第二张表，演示迁移中的外键约束。
pub mod order_item {
    use aurora::db::aurora_entity;
    use sea_orm::ActiveModelBehavior;
    use sea_orm::entity::prelude::*;

    #[aurora_entity(table = "order_items")]
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub id: i64,
        pub user_id: i64,
        pub title: String,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {
        #[sea_orm(
            belongs_to = "super::user::Entity",
            from = "Column::UserId",
            to = "super::user::Column::Id"
        )]
        User,
    }

    impl ActiveModelBehavior for ActiveModel {}
}

/// 应用侧迁移（见 `docs/10` §3）。
///
/// 注意：`DeriveMigrationName` 取的是**源文件名**（`file!()` 的 stem），因此真实
/// 项目应把每个迁移放在独立文件 `m<UTC时间戳>_<动作>_<对象>.rs` 中并 derive。
/// 本示例为单文件，故显式实现 [`MigrationName`] 以给出互不相同的名称。
mod migration {
    use aurora::db::prefix;
    use sea_orm_migration::{prelude::*, schema::*};

    /// 汇总全部迁移（时间序）。
    pub struct Migrator;

    impl MigratorTrait for Migrator {
        fn migrations() -> Vec<Box<dyn MigrationTrait>> {
            vec![
                Box::new(M20260101000001CreateUsers),
                Box::new(M20260101000002CreateOrderItems),
            ]
        }

        /// 迁移记录表也带前缀：多应用共用一个数据库时，各自维护自己的版本记录。
        fn migration_table_name() -> DynIden {
            prefix::migration_table()
        }
    }

    pub struct M20260101000001CreateUsers;

    impl MigrationName for M20260101000001CreateUsers {
        fn name(&self) -> &str {
            "m20260101000001_create_users"
        }
    }

    #[async_trait::async_trait]
    impl MigrationTrait for M20260101000001CreateUsers {
        async fn up(&self, m: &SchemaManager) -> Result<(), DbErr> {
            m.create_table(
                Table::create()
                    .table(prefix::ident("users"))
                    .col(
                        ColumnDef::new("id")
                            .big_integer()
                            .not_null()
                            .auto_increment()
                            .primary_key()
                            .take(),
                    )
                    .col(string("email"))
                    .col(string("name"))
                    .col(timestamp_with_time_zone("created_at"))
                    .to_owned(),
            )
            .await?;

            m.create_index(
                Index::create()
                    // 索引名在 PostgreSQL 里是 schema 级唯一，共库时必须带前缀
                    .name(prefix::index("uq_users_email"))
                    .table(prefix::ident("users"))
                    .col("email")
                    .unique()
                    .to_owned(),
            )
            .await
        }

        async fn down(&self, m: &SchemaManager) -> Result<(), DbErr> {
            m.drop_table(Table::drop().table(prefix::ident("users")).to_owned())
                .await
        }
    }

    pub struct M20260101000002CreateOrderItems;

    impl MigrationName for M20260101000002CreateOrderItems {
        fn name(&self) -> &str {
            "m20260101000002_create_order_items"
        }
    }

    #[async_trait::async_trait]
    impl MigrationTrait for M20260101000002CreateOrderItems {
        async fn up(&self, m: &SchemaManager) -> Result<(), DbErr> {
            m.create_table(
                Table::create()
                    .table(prefix::ident("order_items"))
                    .col(
                        ColumnDef::new("id")
                            .big_integer()
                            .not_null()
                            .auto_increment()
                            .primary_key()
                            .take(),
                    )
                    .col(big_integer("user_id"))
                    .col(string("title"))
                    .foreign_key(
                        ForeignKey::create()
                            .name(prefix::index("fk_order_items_users"))
                            .from(prefix::ident("order_items"), "user_id")
                            .to(prefix::ident("users"), "id"),
                    )
                    .to_owned(),
            )
            .await
        }

        async fn down(&self, m: &SchemaManager) -> Result<(), DbErr> {
            m.drop_table(Table::drop().table(prefix::ident("order_items")).to_owned())
                .await
        }
    }
}

/// 业务侧 Repository trait（见 `docs/10` §4）。
#[async_trait::async_trait]
pub trait UserRepository: Send + Sync {
    /// 按主键查询；「未找到」返回 `Ok(None)`。
    async fn find_by_id(&self, id: i64) -> AppResult<Option<user::Model>>;
    /// 新增用户。
    async fn insert(&self, new: NewUser) -> AppResult<user::Model>;
    /// Offset 分页（复用框架 `Repository` 默认实现）。
    async fn list_page(&self, page: u64, size: u64) -> AppResult<Page<user::Model>>;
    /// 游标分页。
    async fn list_cursor(
        &self,
        after: Option<i64>,
        size: u64,
    ) -> AppResult<CursorPage<user::Model>>;
}

/// 新建用户入参。
#[derive(Debug, serde::Deserialize)]
pub struct NewUser {
    pub email: String,
    pub name: String,
}

/// SeaORM 实现的 Repository（测试可换成内存替身）。
pub struct SeaOrmUserRepository {
    db: DbPool,
}

impl SeaOrmUserRepository {
    /// 由连接池构造。
    pub fn new(db: DbPool) -> Self {
        Self { db }
    }
}

#[async_trait::async_trait]
impl UserRepository for SeaOrmUserRepository {
    async fn find_by_id(&self, id: i64) -> AppResult<Option<user::Model>> {
        Ok(user::Entity::find_by_id(id).one(&self.db).await?)
    }

    async fn insert(&self, new: NewUser) -> AppResult<user::Model> {
        let model = user::ActiveModel {
            email: Set(new.email),
            name: Set(new.name),
            created_at: Set(chrono::Utc::now()),
            ..Default::default()
        };
        Ok(model.insert(&self.db).await?)
    }

    async fn list_page(&self, page: u64, size: u64) -> AppResult<Page<user::Model>> {
        let page: Page<user::Model> =
            <DbPool as Repository<user::Entity>>::page(&self.db, page, size).await?;
        Ok(page)
    }

    async fn list_cursor(
        &self,
        after: Option<i64>,
        size: u64,
    ) -> AppResult<CursorPage<user::Model>> {
        Ok(cursor::paginate(
            &self.db,
            user::Entity::find(),
            user::Column::Id,
            after,
            size,
        )
        .await?)
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // 1. 加载配置
    // `database.url` 属敏感项，按 `docs/08` §5 **只走环境变量**
    // （`AURORA_DATABASE__URL`），不写入 `config/default.toml`。
    // 示例要求自包含（`docs/13` §2），故未设置环境变量时回退到本地 sqlite 文件。
    let mut config = preset::load_env_var("AURORA_ENV", "config").unwrap_or_default();
    if config.database.url.trim().is_empty() {
        config.database.url = "sqlite://aurora-dev.db?mode=rwc".to_string();
    }
    // 示例自包含：启动即执行迁移（生产应按 `[database].migrate_on_start` 控制，
    // 且多实例部署下仅单实例开启）
    config.database.migrate_on_start = true;
    config.validate()?;
    // 2. 初始化日志与追踪
    init_tracing(&config.telemetry)?;
    // 3. 构建应用状态（连接池 + ping），并按配置执行迁移
    let state = AppState::build_with_migrator::<migration::Migrator>(&config).await?;
    // 4. 业务路由（/health、/ready 由 build_router 自动挂载）
    let routes = Router::new()
        .route("/users", get(list_users).post(create_user))
        .route("/users/cursor", get(list_users_cursor))
        .route("/users/{id}", get(get_user).delete(delete_user));
    // 5. 组装 Router（套中间件栈）
    let app = build_router(AppRoutes::all_public(routes), state);
    // 6. 起服务 + 优雅关闭
    let addr = config.server.socket_addr()?;
    tracing::info!(%addr, "crud_repository example starting");
    serve(app, addr).await?;

    tracing::info!("server shut down gracefully");
    Ok(())
}

#[derive(Debug, serde::Deserialize)]
struct ListQuery {
    page: Option<u64>,
    size: Option<u64>,
}

#[derive(Debug, serde::Deserialize)]
struct CursorQuery {
    after: Option<String>,
    size: Option<u64>,
}

#[derive(Debug, serde::Deserialize)]
struct IdPath {
    id: i64,
}

/// Offset 分页列表。
async fn list_users(
    State(state): State<AppState>,
    Query(q): Query<ListQuery>,
) -> AppResult<ApiResponse<Page<user::Model>>> {
    let repo = SeaOrmUserRepository::new(state.db.clone());
    let page = repo
        .list_page(q.page.unwrap_or(1), q.size.unwrap_or(20))
        .await?;
    Ok(ApiResponse::ok(page))
}

/// 游标分页列表（`after` 为不透明游标）。
async fn list_users_cursor(
    State(state): State<AppState>,
    Query(q): Query<CursorQuery>,
) -> AppResult<ApiResponse<CursorPage<user::Model>>> {
    let repo = SeaOrmUserRepository::new(state.db.clone());
    let after = match q.after.as_deref() {
        Some(value) => Some(Cursor::parse(value)?.decode_id()?),
        None => None,
    };
    let page = repo.list_cursor(after, q.size.unwrap_or(20)).await?;
    Ok(ApiResponse::ok(page))
}

/// 按主键查询；不存在返回 `30001`。
async fn get_user(
    State(state): State<AppState>,
    Path(path): Path<IdPath>,
) -> AppResult<ApiResponse<user::Model>> {
    let repo = SeaOrmUserRepository::new(state.db.clone());
    let user = require_found(repo.find_by_id(path.id).await?, format!("user {}", path.id))?;
    Ok(ApiResponse::ok(user))
}

/// 新增用户。
async fn create_user(
    State(state): State<AppState>,
    Json(new): Json<NewUser>,
) -> AppResult<ApiResponse<user::Model>> {
    let repo = SeaOrmUserRepository::new(state.db.clone());
    let user = repo.insert(new).await?;
    Ok(ApiResponse::ok(user))
}

/// 删除用户；不存在返回 `30001`。
async fn delete_user(
    State(state): State<AppState>,
    Path(path): Path<IdPath>,
) -> AppResult<ApiResponse<()>> {
    let repo = SeaOrmUserRepository::new(state.db.clone());
    let user = require_found(repo.find_by_id(path.id).await?, format!("user {}", path.id))?;
    user::Entity::delete_by_id(user.id).exec(&state.db).await?;
    Ok(ApiResponse::ok(()))
}

/// 业务错误映射示例（见 `docs/10` §4 / `docs/17` §5）。
#[allow(dead_code)]
fn map_user_error() -> AppError {
    AppError::conflict("email exists")
}
