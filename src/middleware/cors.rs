//! CORS 层（tower-http CorsLayer 的配置包装）；实际装配见 web/router.rs。

pub use crate::web::router::build_cors as layer; // 重导出 CORS 构造器并起别名 layer
