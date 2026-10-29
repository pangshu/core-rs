//! `[cache]` 配置段（feature `cache`）。
//!
//! 字段与默认值见 `docs/08-config-spec.md` §3 `[cache]`。

use std::time::Duration;

use serde::Deserialize;

use crate::cache::key::{MAX_KEY_BYTES, validate_namespace};
use crate::config::sections::de::de_duration;

/// 缓存后端。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CacheBackend {
    /// 进程内（moka）；单实例默认。
    Memory,
    /// Redis；多实例必须（否则各实例缓存不一致）。
    Redis,
}

/// Redis 连接配置。
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
#[non_exhaustive]
pub struct RedisSection {
    /// 连接串（敏感项 🔒，只走环境变量 `AURORA_CACHE__REDIS__URL`）。
    pub url: Option<String>,
    /// 连接池大小。
    pub pool_size: u32,
    /// 是否允许明文 `redis://`（默认 `false`，生产应使用 `rediss://`）。
    ///
    /// 打开后启动日志会打 `warn!`；仅用于"Redis 在可信内网"的混合部署。
    pub allow_insecure: bool,
}

/// 分布式锁配置。
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
#[non_exhaustive]
pub struct LockSection {
    /// 锁默认 TTL（必须大于 0）。
    #[serde(deserialize_with = "de_duration")]
    pub default_ttl: Duration,
}

impl Default for LockSection {
    fn default() -> Self {
        Self {
            default_ttl: Duration::from_secs(10),
        }
    }
}

/// singleflight（防击穿）配置。
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
#[non_exhaustive]
pub struct SingleflightSection {
    /// 是否合并同一 key 的并发回源。
    pub enabled: bool,
    /// 是否把回源**错误**也共享给等待者（默认 `false`：一次瞬时故障不应放大成整批失败）。
    pub share_errors: bool,
}

impl Default for SingleflightSection {
    fn default() -> Self {
        Self {
            enabled: true,
            share_errors: false,
        }
    }
}

/// 资源上限。
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
#[non_exhaustive]
pub struct LimitsSection {
    /// 键的最大字节数（不超过 [`MAX_KEY_BYTES`]）。
    pub max_key_bytes: usize,
    /// 单个值（或幂等响应体）的最大字节数。
    pub max_value_bytes: usize,
    /// 进程内后端的总字节容量（仅 `backend = memory` 生效）。
    pub max_memory_bytes: u64,
}

impl Default for LimitsSection {
    fn default() -> Self {
        Self {
            max_key_bytes: MAX_KEY_BYTES,
            max_value_bytes: 1024 * 1024,
            max_memory_bytes: 64 * 1024 * 1024,
        }
    }
}

/// 缓存配置段。
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
#[non_exhaustive]
pub struct CacheSection {
    /// 后端。
    pub backend: CacheBackend,
    /// 键前缀（非空，仅 `[A-Za-z0-9_.-]`）。
    pub namespace: String,
    /// 默认过期时长（`set` 未显式给 TTL 时使用；必须大于 0）。
    #[serde(deserialize_with = "de_duration")]
    pub default_ttl: Duration,
    /// Redis 连接配置。
    pub redis: RedisSection,
    /// 分布式锁配置。
    pub lock: LockSection,
    /// singleflight（防击穿）。
    pub singleflight: SingleflightSection,
    /// 资源上限。
    pub limits: LimitsSection,
    /// TTL 抖动上限（防雪崩；`0` = 关闭）。
    #[serde(deserialize_with = "de_duration")]
    pub ttl_jitter: Duration,
    /// 空值缓存 TTL（防穿透；`0` = 关闭）。
    #[serde(deserialize_with = "de_duration")]
    pub null_cache_ttl: Duration,
}

impl Default for CacheSection {
    fn default() -> Self {
        Self {
            backend: CacheBackend::Memory,
            namespace: "aurora".to_string(),
            default_ttl: Duration::from_secs(300),
            redis: RedisSection {
                url: None,
                pool_size: 10,
                allow_insecure: false,
            },
            lock: LockSection::default(),
            singleflight: SingleflightSection::default(),
            limits: LimitsSection::default(),
            ttl_jitter: Duration::ZERO,
            null_cache_ttl: Duration::ZERO,
        }
    }
}

impl CacheSection {
    /// 校验缓存配置的自洽性。
    ///
    /// # Errors
    /// 命名空间非法、TTL 为 0、上限非法，或 Redis URL 不满足传输安全要求时返回错误信息。
    pub fn validate(&self) -> Result<(), String> {
        validate_namespace(&self.namespace)?;

        if self.default_ttl.is_zero() {
            return Err("[cache].default_ttl 必须大于 0（0 会在 Redis 侧产生永久键）".to_string());
        }
        if self.lock.default_ttl.is_zero() {
            return Err("[cache].lock.default_ttl 必须大于 0".to_string());
        }
        if self.limits.max_key_bytes == 0 || self.limits.max_key_bytes > MAX_KEY_BYTES {
            return Err(format!(
                "[cache].limits.max_key_bytes 必须在 1..={MAX_KEY_BYTES} 之间"
            ));
        }
        if self.limits.max_value_bytes == 0 {
            return Err("[cache].limits.max_value_bytes 必须大于 0".to_string());
        }
        if self.limits.max_memory_bytes == 0 {
            return Err("[cache].limits.max_memory_bytes 必须大于 0".to_string());
        }

        match self.redis.url.as_deref() {
            Some(url) if !url.trim().is_empty() => {
                crate::utils::redis_url::validate(url, self.redis.allow_insecure)?;
                if self.redis.allow_insecure && url.trim_start().starts_with("redis://") {
                    tracing::warn!(
                        "[cache].redis.allow_insecure = true：Redis 使用明文传输，\
                         凭据与会话数据可被网络中间人读取"
                    );
                }
            }
            _ => {
                if self.backend == CacheBackend::Redis {
                    return Err(
                        "cache.redis.url is required when [cache].backend = \"redis\" \
                         (set AURORA_CACHE__REDIS__URL)"
                            .to_string(),
                    );
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
    fn should_default_to_memory_with_guards_off() {
        let s = CacheSection::default();
        assert_eq!(s.backend, CacheBackend::Memory);
        assert_eq!(s.namespace, "aurora");
        assert_eq!(s.default_ttl, Duration::from_secs(300));
        assert!(s.singleflight.enabled);
        assert!(!s.singleflight.share_errors);
        assert_eq!(s.lock.default_ttl, Duration::from_secs(10));
        assert!(s.ttl_jitter.is_zero());
        assert!(s.null_cache_ttl.is_zero());
        assert_eq!(s.limits.max_value_bytes, 1024 * 1024);
        assert!(s.validate().is_ok());
    }

    #[test]
    fn should_require_redis_url_when_backend_is_redis() {
        let s = CacheSection {
            backend: CacheBackend::Redis,
            ..CacheSection::default()
        };
        assert!(s.validate().is_err());

        let ok = CacheSection {
            backend: CacheBackend::Redis,
            redis: RedisSection {
                url: Some("rediss://127.0.0.1:6379".to_string()),
                ..RedisSection::default()
            },
            ..CacheSection::default()
        };
        assert!(ok.validate().is_ok());
    }

    #[test]
    fn should_reject_plaintext_redis_unless_allowed() {
        let plain = CacheSection {
            backend: CacheBackend::Redis,
            redis: RedisSection {
                url: Some("redis://127.0.0.1:6379".to_string()),
                ..RedisSection::default()
            },
            ..CacheSection::default()
        };
        assert!(plain.validate().is_err(), "默认必须拒绝明文 redis://");

        let allowed = CacheSection {
            redis: RedisSection {
                allow_insecure: true,
                ..plain.redis.clone()
            },
            ..plain.clone()
        };
        assert!(allowed.validate().is_ok(), "显式放行后应通过");
    }

    #[test]
    fn should_reject_zero_ttl_and_bad_namespace() {
        let zero_ttl = CacheSection {
            default_ttl: Duration::ZERO,
            ..CacheSection::default()
        };
        assert!(zero_ttl.validate().is_err());

        let bad_ns = CacheSection {
            namespace: "a b".to_string(),
            ..CacheSection::default()
        };
        assert!(bad_ns.validate().is_err());

        let empty_ns = CacheSection {
            namespace: String::new(),
            ..CacheSection::default()
        };
        assert!(empty_ns.validate().is_err());
    }

    #[test]
    fn should_parse_durations_and_nested_toggles() {
        let json = r#"
            {
                "backend": "redis",
                "default_ttl": "60s",
                "ttl_jitter": "5s",
                "null_cache_ttl": "10s",
                "singleflight": { "enabled": false, "share_errors": true },
                "lock": { "default_ttl": "3s" },
                "limits": { "max_key_bytes": 128, "max_value_bytes": 2048 },
                "redis": { "url": "rediss://h:6379", "pool_size": 4 }
            }
        "#;
        let s: CacheSection = serde_json::from_str(json).unwrap();
        assert_eq!(s.backend, CacheBackend::Redis);
        assert_eq!(s.default_ttl, Duration::from_secs(60));
        assert_eq!(s.ttl_jitter, Duration::from_secs(5));
        assert_eq!(s.null_cache_ttl, Duration::from_secs(10));
        assert!(!s.singleflight.enabled);
        assert!(s.singleflight.share_errors);
        assert_eq!(s.lock.default_ttl, Duration::from_secs(3));
        assert_eq!(s.limits.max_key_bytes, 128);
        assert_eq!(s.limits.max_value_bytes, 2048);
        assert_eq!(s.redis.pool_size, 4);
    }
}
