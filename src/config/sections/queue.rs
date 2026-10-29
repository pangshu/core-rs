//! `[queue]` 配置段（feature `queue`）。
//!
//! 字段与默认值见 `docs/08-config-spec.md` §3 `[queue]`。

use std::time::Duration;

use serde::Deserialize;

use crate::config::sections::RetryBackoff;
use crate::config::sections::de::de_duration;

/// 队列后端。
///
/// 每个后端都需要对应 feature（`queue-redis` / `queue-amqp` / `queue-kafka`）；
/// 配了后端却没编进 feature 时**启动期**报错。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum QueueBackendKind {
    /// 进程内（默认；多实例时消息不出实例）。
    Memory,
    /// Redis。
    Redis,
    /// RabbitMQ。
    Amqp,
    /// Kafka。
    Kafka,
}

/// 消费失败重试配置。
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(default)]
#[non_exhaustive]
pub struct QueueRetrySection {
    /// **消费失败**重试次数上限（与 `[resilience].retry` 的**出站**重试语义不同）。
    pub max_attempts: u32,
    /// 退避策略。
    pub backoff: RetryBackoff,
}

impl Default for QueueRetrySection {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            backoff: RetryBackoff::Exponential,
        }
    }
}

/// Redis 连接配置。
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
#[non_exhaustive]
pub struct QueueRedisSection {
    /// 连接串（敏感项 🔒，只走环境变量 `AURORA_QUEUE__REDIS__URL`）。
    pub url: Option<String>,
    /// 是否允许明文 `redis://`（默认 `false`，生产应使用 `rediss://`）。
    pub allow_insecure: bool,
}

/// AMQP 连接配置。
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
#[non_exhaustive]
pub struct AmqpSection {
    /// 连接串（敏感项 🔒，只走环境变量 `AURORA_QUEUE__AMQP__URL`）。
    pub url: Option<String>,
    /// 是否允许明文 `amqp://`（默认 `false`，生产应使用 `amqps://`）。
    ///
    /// `amqp://` 会把用户名 / 密码与消息体明文送出网卡。与 `[cache].redis.allow_insecure`
    /// 同一套「显式确认」模式：不显式打开就**拒绝启动**。
    pub allow_insecure: bool,
}

/// Kafka 连接配置。
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
#[non_exhaustive]
pub struct KafkaSection {
    /// broker 列表。
    pub brokers: Vec<String>,
}

/// 死信队列配置。
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
#[non_exhaustive]
pub struct DlqSection {
    /// 重试耗尽后是否入死信队列。
    pub enabled: bool,
    /// 死信保留时长（同时用作消费去重键 TTL）。
    #[serde(deserialize_with = "de_duration")]
    pub retention: Duration,
}

impl Default for DlqSection {
    fn default() -> Self {
        Self {
            enabled: true,
            retention: Duration::from_secs(7 * 24 * 3600),
        }
    }
}

/// 消费幂等配置。
///
/// 去重键是**两阶段**的（见 `queue::service`）：`recv` 时写 `processing`（短租约），
/// `ack` 时改写 `done`（长保留）。租约到期即视为「上次处理已放弃」，重投可重新占位
/// —— 这样崩溃不会把消息永久标记成「已消费」。
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(default)]
#[non_exhaustive]
pub struct QueueIdempotencySection {
    /// 按 `message_id` 去重（去重键默认落 `cache-redis`；需 `cache` feature）。
    pub enabled: bool,
    /// 去重存储不可用时是否**拒绝消费**（默认 `false`：降级为不去重）。
    ///
    /// 资金 / 扣款类消费建议打开：宁可重投，也不在「无法判重」时执行业务。
    pub fail_closed: bool,
    /// `processing` 状态的租约时长（默认 `5m`）。
    ///
    /// **必须大于单条消息的最长处理时间**，否则处理中的消息会被判定为「已放弃」而被
    /// 并发重做。租约过期不等于消息被丢弃：重投会重新占位并再处理一次（at-least-once）。
    #[serde(deserialize_with = "de_duration")]
    pub lease: Duration,
    /// `done` 状态的保留时长（默认 `7d`）。
    ///
    /// 只在去重窗口内有意义：超过该时长后同 id 的消息会被重新执行。
    /// **不再复用 `dlq.retention`**（两者语义无关，且 `retention = 0` 会让去重直接失效）。
    #[serde(deserialize_with = "de_duration")]
    pub ttl: Duration,
}

impl Default for QueueIdempotencySection {
    fn default() -> Self {
        Self {
            enabled: false,
            fail_closed: false,
            lease: Duration::from_secs(5 * 60),
            ttl: Duration::from_secs(7 * 24 * 3600),
        }
    }
}

/// 队列配置段。
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
#[non_exhaustive]
pub struct QueueSection {
    /// 后端。
    pub backend: QueueBackendKind,
    /// 重试策略。
    pub retry: QueueRetrySection,
    /// Redis 连接配置。
    pub redis: QueueRedisSection,
    /// AMQP 连接配置。
    pub amqp: AmqpSection,
    /// Kafka 连接配置。
    pub kafka: KafkaSection,
    /// 死信队列。
    pub dlq: DlqSection,
    /// 消费幂等。
    pub idempotency: QueueIdempotencySection,
    /// 单条消息体的字节上限（默认 256 KiB）。
    ///
    /// 投递与取出**双向**校验：超限的 `publish` 直接报 `10002`（`PAYLOAD_TOO_LARGE`），
    /// 超限的入站消息转死信而不是分配内存。不设上限时，一个畸形消息就能把消费端 OOM。
    pub max_payload_bytes: usize,
    /// in-flight 可见性超时（默认 `60s`）。
    ///
    /// 仅 `redis` 后端生效：消息被取走后超过该时长仍未 `ack`，视为消费者已失联，
    /// 自动移回 ready 重投（可能重复消费，因此消费端仍需幂等）。
    /// **必须大于单条消息的最长处理时间**。
    #[serde(deserialize_with = "de_duration")]
    pub visibility_timeout: Duration,
    /// `memory` 后端单队列深度上限（默认 `100000`）。
    ///
    /// 超限时 `enqueue` 报 `50003` 而不是继续吃内存。仅对 `memory` 后端生效。
    pub max_depth: usize,
}

impl Default for QueueSection {
    fn default() -> Self {
        Self {
            backend: QueueBackendKind::Memory,
            retry: QueueRetrySection::default(),
            redis: QueueRedisSection::default(),
            amqp: AmqpSection::default(),
            kafka: KafkaSection::default(),
            dlq: DlqSection::default(),
            idempotency: QueueIdempotencySection::default(),
            max_payload_bytes: 256 * 1024,
            visibility_timeout: Duration::from_secs(60),
            max_depth: 100_000,
        }
    }
}

impl QueueSection {
    /// 校验：非 `memory` 后端要求对应连接配置齐备。
    ///
    /// # Errors
    /// 连接配置缺失时返回错误信息。
    pub fn validate(&self) -> Result<(), String> {
        let missing = |key: &str| {
            format!(
                "queue.{key} is required when [queue].backend = \"{:?}\"",
                self.backend
            )
        };
        // 先校验 feature 是否编进来（否则运行期才报错，且错误信息不指向 feature）
        if self.backend == QueueBackendKind::Redis && !cfg!(feature = "queue-redis") {
            return Err("queue.backend = \"redis\" 需要启用 `queue-redis` feature".to_string());
        }
        if self.backend == QueueBackendKind::Amqp && !cfg!(feature = "queue-amqp") {
            return Err("queue.backend = \"amqp\" 需要启用 `queue-amqp` feature".to_string());
        }
        if self.backend == QueueBackendKind::Kafka && !cfg!(feature = "queue-kafka") {
            return Err("queue.backend = \"kafka\" 需要启用 `queue-kafka` feature".to_string());
        }

        // 数值护栏：这几项写 0 都会把「保护」变成「失效」，且失效方式很隐蔽
        // （去重键 TTL=0 → 直接报错；可见性超时=0 → 每条消息刚出队就被回收）。
        if self.max_payload_bytes == 0 {
            return Err("queue.max_payload_bytes 必须大于 0".to_string());
        }
        if self.max_depth == 0 {
            return Err("queue.max_depth 必须大于 0".to_string());
        }
        if self.visibility_timeout.is_zero() {
            return Err("queue.visibility_timeout 必须大于 0".to_string());
        }
        if self.idempotency.enabled {
            if self.idempotency.lease.is_zero() {
                return Err("queue.idempotency.lease 必须大于 0".to_string());
            }
            if self.idempotency.ttl.is_zero() {
                return Err("queue.idempotency.ttl 必须大于 0".to_string());
            }
            if self.idempotency.ttl < self.idempotency.lease {
                return Err(
                    "queue.idempotency.ttl 不能小于 queue.idempotency.lease（done 会比 processing 先过期）"
                        .to_string(),
                );
            }
        }

        match self.backend {
            QueueBackendKind::Memory => Ok(()),
            QueueBackendKind::Redis => {
                let Some(url) = self
                    .redis
                    .url
                    .as_deref()
                    .filter(|url| !url.trim().is_empty())
                else {
                    return Err(missing("redis.url"));
                };
                crate::utils::redis_url::validate(url, self.redis.allow_insecure)?;
                Ok(())
            }
            QueueBackendKind::Amqp => {
                let Some(url) = self
                    .amqp
                    .url
                    .as_deref()
                    .filter(|url| !url.trim().is_empty())
                else {
                    return Err(missing("amqp.url"));
                };
                crate::utils::amqp_url::validate(url, self.amqp.allow_insecure)?;
                Ok(())
            }
            QueueBackendKind::Kafka => {
                if self.kafka.brokers.is_empty() {
                    return Err(missing("kafka.brokers"));
                }
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_default_to_memory_with_dlq_enabled() {
        let s = QueueSection::default();
        assert_eq!(s.backend, QueueBackendKind::Memory);
        assert_eq!(s.retry.max_attempts, 3);
        assert_eq!(s.retry.backoff, RetryBackoff::Exponential);
        assert!(s.dlq.enabled);
        assert_eq!(s.dlq.retention, Duration::from_secs(7 * 24 * 3600));
        assert!(!s.idempotency.enabled);
        assert_eq!(s.idempotency.lease, Duration::from_secs(300));
        assert_eq!(s.idempotency.ttl, Duration::from_secs(7 * 24 * 3600));
        assert_eq!(s.max_payload_bytes, 256 * 1024);
        assert_eq!(s.visibility_timeout, Duration::from_secs(60));
        assert!(s.validate().is_ok());
    }

    #[test]
    fn should_reject_zeroed_guards() {
        let zero_payload = QueueSection {
            max_payload_bytes: 0,
            ..QueueSection::default()
        };
        assert!(
            zero_payload.validate().is_err(),
            "max_payload_bytes = 0 应拒绝"
        );

        let zero_visibility = QueueSection {
            visibility_timeout: Duration::ZERO,
            ..QueueSection::default()
        };
        assert!(
            zero_visibility.validate().is_err(),
            "visibility_timeout = 0 应拒绝"
        );

        let mut inverted = QueueSection::default();
        inverted.idempotency.enabled = true;
        inverted.idempotency.lease = Duration::from_secs(600);
        inverted.idempotency.ttl = Duration::from_secs(60);
        assert!(inverted.validate().is_err(), "ttl < lease 应拒绝");
    }

    #[test]
    fn should_require_connection_config_for_non_memory_backends() {
        let redis = QueueSection {
            backend: QueueBackendKind::Redis,
            ..QueueSection::default()
        };
        assert!(redis.validate().is_err());

        let amqp = QueueSection {
            backend: QueueBackendKind::Amqp,
            ..QueueSection::default()
        };
        assert!(amqp.validate().is_err());

        let kafka = QueueSection {
            backend: QueueBackendKind::Kafka,
            ..QueueSection::default()
        };
        assert!(kafka.validate().is_err());
    }

    /// 明文 `amqp://` 默认拒绝启动（凭据与消息体明文过网）。
    #[cfg(feature = "queue-amqp")]
    #[test]
    fn should_reject_plaintext_amqp_url_by_default() {
        let plaintext = QueueSection {
            backend: QueueBackendKind::Amqp,
            amqp: AmqpSection {
                url: Some("amqp://user:pass@h:5672/%2f".to_string()),
                allow_insecure: false,
            },
            ..QueueSection::default()
        };
        assert!(plaintext.validate().is_err(), "明文 amqp:// 应拒绝");

        let allowed = QueueSection {
            backend: QueueBackendKind::Amqp,
            amqp: AmqpSection {
                url: Some("amqp://user:pass@h:5672/%2f".to_string()),
                allow_insecure: true,
            },
            ..QueueSection::default()
        };
        assert!(allowed.validate().is_ok(), "显式 allow_insecure 应放行");

        let tls = QueueSection {
            backend: QueueBackendKind::Amqp,
            amqp: AmqpSection {
                url: Some("amqps://user:pass@h:5671/%2f".to_string()),
                allow_insecure: false,
            },
            ..QueueSection::default()
        };
        assert!(tls.validate().is_ok(), "amqps:// 应直接放行");
    }

    #[test]
    fn should_parse_nested_fields() {
        let json = r#"
            {
                "backend": "memory",
                "retry": { "max_attempts": 5, "backoff": "fixed" },
                "dlq": { "enabled": false, "retention": "1d" },
                "idempotency": { "enabled": true },
                "kafka": { "brokers": ["a:9092"] }
            }
        "#;
        let s: QueueSection = serde_json::from_str(json).unwrap();
        assert_eq!(s.retry.max_attempts, 5);
        assert_eq!(s.retry.backoff, RetryBackoff::Fixed);
        assert!(!s.dlq.enabled);
        assert_eq!(s.dlq.retention, Duration::from_secs(86_400));
        assert!(s.idempotency.enabled);
        assert_eq!(s.kafka.brokers, vec!["a:9092".to_string()]);
    }
}
