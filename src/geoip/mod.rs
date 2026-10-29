//! IP 地理定位（feature `geoip`，MaxMind MMDB）。
//!
//! - [`GeoIp`]：装配入口（`AppState.geoip`），带**有界查询缓存**（分片 + FIFO 逐条淘汰）
//! - [`GeoInfo`]：地理信息模型（字段全可空）
//! - [`ClientGeo`]：axum 提取器（按客户端 IP 注入地理信息，**四态显式**）
//! - [`GeoIpHealth`]：`/ready` 探针（未显式接受降级时缺库即判失败）
//! - `reader`：MMDB 读取（城市库 + 可选 ASN 库）
//!
//! **降级策略**：`[geoip].city_db` 不存在时**不报错**，只 warn 并让查询恒为 `None`
//! —— 便于未持有 MaxMind 许可证的开发环境直接运行；库存在但损坏则启动失败。
//! **生产环境**缺库会被 `[geoip].allow_degraded` 拦住（默认拒绝启动）。
//! 降级状态下 [`ClientGeo`] 返回 [`ClientGeo::Unavailable`] 而非 `None`，调用方
//! 必须显式处理，不得把"判定不了"当成"无风险"。
//!
//! **IP 归一化**：`::ffff:a.b.c.d` 一律按 `a.b.c.d` 处理（与 `ClientIp` 一致）。
//!
//! **不做**：不做 IP 归属地之外的画像/风控（见 `docs/03` §4.18）。

pub mod extractor;
pub mod health;
pub mod lookup;
pub mod model;
pub mod reader;

pub use extractor::ClientGeo;
pub use health::GeoIpHealth;
pub use lookup::GeoIp;
pub use model::GeoInfo;
pub use reader::GeoReader;
