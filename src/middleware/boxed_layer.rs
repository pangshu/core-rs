//! 自组装用中间件层的统一返回类型 [`BoxedLayer`]。

/// 自组装用中间件层的统一返回类型：装箱屏蔽 `from_fn` 的具体类型
/// （含匿名 async fn，无法跨 crate 命名），使 `auth::layer()` 等构造器
/// 可在业务 crate 直接 `Router::layer()` 使用。用 `BoxCloneSyncServiceLayer`
/// 而非 `BoxLayer`：后者产出的 `BoxService` 不实现 Clone，过不了
/// `Router::layer` 的约束。axum 0.8 自身启用 `tower/util`，恒可用；
/// `Route` 内部本就是动态派发，开销可忽略。
pub type BoxedLayer = tower::util::BoxCloneSyncServiceLayer< // 装箱层类型别名
    axum::routing::Route, // 内层服务：axum 路由（Router::layer 的绑定目标）
    axum::extract::Request, // 请求类型
    axum::response::Response, // 响应类型
    core::convert::Infallible, // axum 路由服务不变出错
>;
