//! `[ratelimit]` 配置段（feature `ratelimit`）。

use std::time::Duration;

use serde::Deserialize;

use crate::config::sections::de::{de_duration, de_opt_duration};
use crate::ratelimit::store::DEFAULT_MAX_KEYS;

/// 限流维度。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RateLimitStrategy {
    /// 按客户端 IP。
    PerIp,
    /// 按**已验签**主体（JWT `sub`）；无法验证身份时回退到 IP。
    ///
    /// 注意：`per_key` 依赖 `auth` feature（需要验签才能拿到可信主体），
    /// 未启用 `auth` 时启动期直接报错——否则只能退化成「按客户端自带的
    /// 请求头限流」，而那个头是攻击者可以逐请求更换的（等于没有限流）。
    ///
    /// `per_key` 只认 **Bearer JWT**：cookie/session 认证的请求拿不到主体，
    /// 会静默退化成 `per_ip`（启动期会 warn，见 `RateLimitService::from_config`）。
    /// 需要「主体 + IP 双桶」时配置 [`RateLimitSection::ip_limit`]。
    PerKey,
}

/// 限流状态存储。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RateLimitStore {
    /// 进程内。
    Memory,
    /// Redis 共享 store（多实例共享计数）。
    Redis,
}

/// 单条按路由覆盖的限流规则（`[[ratelimit.rules]]`）。
///
/// 用于给**敏感端点**单独收紧配额——全局只有一组 `limit/window` 时，登录接口
/// 与静态资源共用同一个配额，等于没有对暴力破解做任何限制。
///
/// ```toml
/// [[ratelimit.rules]]
/// path = "/auth"              # 前缀匹配（最长前缀优先）
/// methods = ["POST"]          # 可选，空 = 不限方法
/// limit = 5
/// window = "60s"
/// strategy = "per_ip"         # 可选，缺省沿用全局
/// ```
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RateLimitRule {
    /// 路径前缀（必须以 `/` 开头）。
    pub path: String,
    /// 限定方法（大小写不敏感，如 `["POST"]`）；空表示全部方法。
    #[serde(default)]
    pub methods: Vec<String>,
    /// 限流维度；`None` 沿用 `[ratelimit].strategy`。
    #[serde(default)]
    pub strategy: Option<RateLimitStrategy>,
    /// 窗口内上限。
    pub limit: u32,
    /// 计数窗口；`None` 沿用 `[ratelimit].window`。
    #[serde(default, deserialize_with = "de_opt_duration")]
    pub window: Option<Duration>,
}

/// 限流配置段。
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
#[non_exhaustive]
pub struct RateLimitSection {
    /// 是否启用。
    pub enabled: bool,
    /// 限流维度。
    pub strategy: RateLimitStrategy,
    /// 窗口内上限。
    pub limit: u32,
    /// 计数窗口。
    #[serde(deserialize_with = "de_duration")]
    pub window: Duration,
    /// 状态存储。
    pub store: RateLimitStore,
    /// 不限流的路径前缀。
    ///
    /// **默认包含 `/health` 与 `/ready`**：它们与业务共用同一个桶时，攻击者
    /// 只要把配额打满，负载均衡的探活就会拿到 429 → 实例被摘除 → 全站雪崩。
    /// 指标端点（`[telemetry].metrics.path`）在启动时自动追加，无需手写。
    pub exclude_paths: Vec<String>,
    /// 进程内桶的键数量上限（超出后降级为全局兜底桶）。
    ///
    /// 键可以由攻击者控制（伪造 `X-Forwarded-For` 时逐请求换 IP），不设上限
    /// 等于把内存交给对方。
    pub max_keys: usize,
    /// `per_key` 主体哈希的盐（`HMAC-SHA256`）。
    ///
    /// 留空 = **每进程随机**：攻击者无法预计算碰撞，但多实例（`store = "redis"`）
    /// 之间桶不共享，实际配额会放大到 `limit × 副本数`。跨实例共享桶必须显式配置
    /// 同一个值（且应当作密钥管理，不要写进公开仓库）。
    pub key_salt: String,
    /// 来源不可知时的兜底配额（`0` = 不计数，仅告警）。
    ///
    /// 「来源不可知」= 链路里没有 `ConnectInfo`（自建 server / 嵌入别的服务），
    /// 此时按 IP 限流在语义上就不成立。用一个共享桶硬限，等于给攻击者一个
    /// 「一键封禁全站」的开关（含探活）；所以默认不计数、只告警，需要应急刹车
    /// 时再把它设成一个明显高于 `limit` 的值。
    pub unknown_limit: u32,
    /// `per_key` 时**叠加**的 IP 桶配额（`0` = 不叠加）。
    ///
    /// `per_key` 只按主体计数，所以「一个账号被盗 + 分布式 IP」或「一个账号
    /// 在单 IP 上刷」都没有 IP 维度的约束。设成明显高于 `limit` 的值（如
    /// `limit × 10`）即可同时获得「主体严格 + IP 宽松」双桶，任一触发即拒。
    pub ip_limit: u32,
    /// 按路由覆盖的规则（见 [`RateLimitRule`]）。
    pub rules: Vec<RateLimitRule>,
}

impl Default for RateLimitSection {
    fn default() -> Self {
        Self {
            enabled: false,
            strategy: RateLimitStrategy::PerIp,
            limit: 100,
            window: Duration::from_secs(60),
            store: RateLimitStore::Memory,
            exclude_paths: vec!["/health".to_string(), "/ready".to_string()],
            max_keys: DEFAULT_MAX_KEYS,
            key_salt: String::new(),
            unknown_limit: 0,
            ip_limit: 0,
            rules: Vec::new(),
        }
    }
}

impl RateLimitSection {
    /// 启动期校验（fail-fast）。
    ///
    /// # Errors
    /// `store = redis` 但未启用 `ratelimit-redis` feature、`strategy = per_key`
    /// 但未启用 `auth` feature、`limit = 0`、`window` 为 0、规则字段非法时返回错误。
    pub fn validate(&self) -> Result<(), String> {
        // 先看 feature 有没有编进来：错误信息要指向 feature，而不是等到运行期才报
        if self.store == RateLimitStore::Redis && !cfg!(feature = "ratelimit-redis") {
            return Err(
                "[ratelimit].store = \"redis\" 需要启用 `ratelimit-redis` feature".to_string(),
            );
        }
        // `per_key` 的键必须是「已验签主体」，验签能力来自 `auth`。缺了它只剩
        // 「按客户端自带的 Authorization 头」这一条路，而那个头逐请求可换，
        // 限流会被完全绕过——宁可在启动期拒绝，也不要静默降级成无效配置。
        if self.strategy == RateLimitStrategy::PerKey && !cfg!(feature = "auth") {
            return Err("[ratelimit].strategy = \"per_key\" 需要启用 `auth` feature\
                 （按主体限流依赖 JWT 验签；否则客户端换个请求头就能绕过限流）"
                .to_string());
        }
        if self.limit == 0 {
            return Err("[ratelimit].limit 必须大于 0".to_string());
        }
        if self.window.is_zero() {
            return Err("[ratelimit].window 必须大于 0".to_string());
        }
        if self.max_keys == 0 {
            return Err("[ratelimit].max_keys 必须大于 0".to_string());
        }
        for path in &self.exclude_paths {
            if !path.starts_with('/') {
                return Err(format!(
                    "[ratelimit].exclude_paths 的 `{path}` 必须以 `/` 开头（按路径前缀匹配）"
                ));
            }
        }
        // 规则里 `per_key` 同样依赖 auth
        if !cfg!(feature = "auth")
            && self
                .rules
                .iter()
                .any(|rule| rule.strategy == Some(RateLimitStrategy::PerKey))
        {
            return Err(
                "[ratelimit].rules 里的 `strategy = \"per_key\"` 需要启用 `auth` feature"
                    .to_string(),
            );
        }
        for rule in &self.rules {
            if !rule.path.starts_with('/') {
                return Err(format!(
                    "[ratelimit].rules 的 `path = \"{}\"` 必须以 `/` 开头（按路径前缀匹配）",
                    rule.path
                ));
            }
            if rule.limit == 0 {
                return Err(format!(
                    "[ratelimit].rules 的 `path = \"{}\"` 的 limit 必须大于 0",
                    rule.path
                ));
            }
            if rule.window.is_some_and(|window| window.is_zero()) {
                return Err(format!(
                    "[ratelimit].rules 的 `path = \"{}\"` 的 window 必须大于 0",
                    rule.path
                ));
            }
            for method in &rule.methods {
                if axum::http::Method::from_bytes(method.trim().to_uppercase().as_bytes()).is_err()
                {
                    return Err(format!(
                        "[ratelimit].rules 的 `path = \"{}\"` 含非法 HTTP 方法 `{method}`",
                        rule.path
                    ));
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_default_to_disabled_per_ip() {
        let s = RateLimitSection::default();
        assert!(!s.enabled);
        assert_eq!(s.strategy, RateLimitStrategy::PerIp);
        assert_eq!(s.limit, 100);
        assert_eq!(s.window, Duration::from_secs(60));
        assert_eq!(s.store, RateLimitStore::Memory);
        assert!(s.validate().is_ok());
    }

    /// 健康检查必须**默认**豁免：否则探活与业务共用配额，配额打满即摘实例。
    #[test]
    fn should_exclude_health_endpoints_by_default() {
        let s = RateLimitSection::default();
        assert!(s.exclude_paths.iter().any(|p| p == "/health"));
        assert!(s.exclude_paths.iter().any(|p| p == "/ready"));
    }

    #[test]
    fn should_require_ratelimit_redis_feature_for_redis_store() {
        let s = RateLimitSection {
            store: RateLimitStore::Redis,
            ..RateLimitSection::default()
        };
        if cfg!(feature = "ratelimit-redis") {
            assert!(
                s.validate().is_ok(),
                "启用 feature 后 redis store 应通过校验"
            );
        } else {
            let error = s.validate().unwrap_err();
            assert!(error.contains("ratelimit-redis"), "实际：{error}");
        }
    }

    #[test]
    fn should_reject_zero_limit_and_window() {
        let zero_limit = RateLimitSection {
            limit: 0,
            ..RateLimitSection::default()
        };
        assert!(zero_limit.validate().is_err());

        let zero_window = RateLimitSection {
            window: Duration::ZERO,
            ..RateLimitSection::default()
        };
        assert!(zero_window.validate().is_err());

        let zero_keys = RateLimitSection {
            max_keys: 0,
            ..RateLimitSection::default()
        };
        assert!(zero_keys.validate().is_err());
    }

    #[test]
    fn should_parse_strategy_and_window() {
        let json = r#"{ "enabled": true, "strategy": "per_key", "limit": 10, "window": "1s" }"#;
        let s: RateLimitSection = serde_json::from_str(json).unwrap();
        assert!(s.enabled);
        assert_eq!(s.strategy, RateLimitStrategy::PerKey);
        assert_eq!(s.limit, 10);
        assert_eq!(s.window, Duration::from_secs(1));
    }

    #[test]
    fn should_require_auth_feature_for_per_key_strategy() {
        let s = RateLimitSection {
            strategy: RateLimitStrategy::PerKey,
            ..RateLimitSection::default()
        };
        if cfg!(feature = "auth") {
            assert!(s.validate().is_ok(), "启用 auth 后 per_key 应通过校验");
        } else {
            let error = s.validate().unwrap_err();
            assert!(error.contains("per_key"), "实际：{error}");
            assert!(error.contains("auth"), "实际：{error}");
        }
    }

    #[test]
    fn should_parse_rules() {
        let json = r#"{
            "enabled": true,
            "rules": [
                { "path": "/auth", "methods": ["POST"], "limit": 5, "window": "60s" },
                { "path": "/export", "limit": 2 }
            ]
        }"#;
        let s: RateLimitSection = serde_json::from_str(json).unwrap();
        assert_eq!(s.rules.len(), 2);
        assert_eq!(s.rules[0].path, "/auth");
        assert_eq!(s.rules[0].methods, vec!["POST".to_string()]);
        assert_eq!(s.rules[0].limit, 5);
        assert_eq!(s.rules[0].window, Some(Duration::from_secs(60)));
        assert!(s.rules[1].window.is_none(), "缺省窗口应沿用全局");
        assert!(s.validate().is_ok());
    }

    #[test]
    fn should_reject_invalid_rules() {
        // 路径必须带前导 `/`：否则 `auth` 会前缀匹配到 `/authorize` 之类的路由
        let bad_path = RateLimitSection {
            rules: vec![RateLimitRule {
                path: "auth".to_string(),
                methods: vec![],
                strategy: None,
                limit: 5,
                window: None,
            }],
            ..RateLimitSection::default()
        };
        assert!(bad_path.validate().unwrap_err().contains("必须以 `/` 开头"));

        // limit = 0 等于「全拒」，几乎必然是笔误
        let zero = RateLimitSection {
            rules: vec![RateLimitRule {
                path: "/auth".to_string(),
                methods: vec![],
                strategy: None,
                limit: 0,
                window: None,
            }],
            ..RateLimitSection::default()
        };
        assert!(zero.validate().is_err());

        // 非法方法名
        let bad_method = RateLimitSection {
            rules: vec![RateLimitRule {
                path: "/auth".to_string(),
                methods: vec!["POST TYPO".to_string()],
                strategy: None,
                limit: 5,
                window: None,
            }],
            ..RateLimitSection::default()
        };
        assert!(
            bad_method
                .validate()
                .unwrap_err()
                .contains("非法 HTTP 方法")
        );
    }

    /// 拼错的字段必须报错，而不是被静默忽略（否则「配了 5 次/分钟」实际没生效）。
    #[test]
    fn should_reject_unknown_rule_fields() {
        let json = r#"{ "rules": [ { "path": "/auth", "limt": 5 } ] }"#;
        assert!(serde_json::from_str::<RateLimitSection>(json).is_err());
    }
}
