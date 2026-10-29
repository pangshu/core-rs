//! `[realtime]` 配置段（feature `ws` / `sse`）。
//!
//! 字段与默认值见 `docs/08-config-spec.md` §3 `[realtime]`。
//! ⚠️ 心跳类参数**仅对新建连接生效**，存量连接需重连（见 `docs/08` §5）。

use std::time::Duration;

use byte_unit::Byte;
use serde::Deserialize;

use crate::config::sections::de::{de_byte, de_duration};

/// 实时通信配置段。
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
#[non_exhaustive]
pub struct RealtimeSection {
    /// 心跳间隔（WS 主动 Ping / SSE KeepAlive）。
    #[serde(deserialize_with = "de_duration")]
    pub heartbeat_interval: Duration,
    /// 心跳超时：超过该时长无任何入站帧即断开（WS）。
    #[serde(deserialize_with = "de_duration")]
    pub heartbeat_timeout: Duration,
    /// 优雅关闭时等待长连接退出的上限。
    #[serde(deserialize_with = "de_duration")]
    pub shutdown_timeout: Duration,
    /// 全局连接数上限。
    pub max_connections: u32,
    /// 每 IP 连接数上限。
    pub max_connections_per_ip: u32,
    /// 每认证主体（`Identity` 的 `sub`）连接数上限；`0` = 不限制。
    ///
    /// 防的是「一个账号从大量 IP 开连接」——每 IP 限制挡不住这种分布式占用。
    /// 匿名连接不参与该维度。
    pub max_connections_per_subject: u32,
    /// 单帧 / 单消息大小上限（**入站与出站都受它约束**）。
    #[serde(deserialize_with = "de_byte")]
    pub max_message_size: Byte,
    /// 跨实例经 `queue` 转发（需同时启用 `queue` + `cache`，见 `realtime::forward`）。
    pub forward_via_queue: bool,
    /// 上行消息队列容量（`0` = 同步直调处理器，不做排队）。
    ///
    /// 处理器在独立任务里消费，读循环不再被它阻塞——否则慢处理器会连心跳都耽误。
    pub inbound_buffer: usize,
    /// 队列满时的策略。
    pub inbound_overflow: InboundOverflow,
    /// 频道名长度上限（字节）。频道名来自 URL，无上限时可用随机长名撑爆频道表。
    pub max_topic_len: usize,
    /// 活跃频道数上限。订阅者归零的频道会被摘除，达到上限后先淘汰空闲频道，
    /// 仍无空位则拒绝新频道（`50005`）。
    pub max_topics: usize,
    /// 出站写超时：单帧发不出去（客户端只连不读）超过该时长即断开，回收连接名额。
    #[serde(deserialize_with = "de_duration")]
    pub write_timeout: Duration,
    /// 每连接上行速率上限（条/秒，突发量同值）；`0` = 不限制。
    pub inbound_rate_per_sec: u32,
    /// WS 升级时允许的 `Origin` 白名单（`scheme://host[:port]`，空 = 不校验）。
    ///
    /// 仅在请求**带了** `Origin` 头时校验——浏览器发起 WS 必然带 `Origin`，
    /// 非浏览器客户端不带，而跨站 WebSocket 劫持（CSWSH）只可能来自浏览器。
    pub allowed_origins: Vec<String>,
}

/// 上行队列满时的处理策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InboundOverflow {
    /// 断开连接（`1013 Try Again Later`）：客户端发得太快，明确告知而不是静默吞掉。
    Close,
    /// 丢弃新帧并计数告警：连接保持，适合「丢了也无所谓」的遥测类上行。
    Drop,
}

impl Default for RealtimeSection {
    fn default() -> Self {
        Self {
            heartbeat_interval: Duration::from_secs(30),
            heartbeat_timeout: Duration::from_secs(60),
            shutdown_timeout: Duration::from_secs(10),
            max_connections: 10_000,
            max_connections_per_ip: 100,
            max_connections_per_subject: 20,
            // 与 `de_byte` 解析 `"64KB"` 的结果一致（byte-unit 的 KB = 1000，KiB = 1024）
            max_message_size: Byte::from_u64(64 * 1000),
            forward_via_queue: false,
            inbound_buffer: 64,
            inbound_overflow: InboundOverflow::Close,
            max_topic_len: 128,
            max_topics: 10_000,
            write_timeout: Duration::from_secs(10),
            inbound_rate_per_sec: 50,
            allowed_origins: Vec::new(),
        }
    }
}

impl RealtimeSection {
    /// 单消息上限（字节），用于 `WebSocketUpgrade::max_message_size`。
    #[must_use]
    pub fn max_message_size_bytes(&self) -> usize {
        usize::try_from(self.max_message_size.as_u64()).unwrap_or(usize::MAX)
    }

    /// 校验：心跳超时必须大于间隔，上限非零。
    ///
    /// # Errors
    /// 参数不自洽时返回错误信息。
    pub fn validate(&self) -> Result<(), String> {
        if self.heartbeat_interval.is_zero() {
            return Err("realtime.heartbeat_interval 必须大于 0".to_string());
        }
        if self.heartbeat_timeout <= self.heartbeat_interval {
            return Err(
                "realtime.heartbeat_timeout 必须大于 heartbeat_interval（否则连接会被误判超时）"
                    .to_string(),
            );
        }
        if self.max_connections == 0 || self.max_connections_per_ip == 0 {
            return Err("realtime.max_connections / max_connections_per_ip 必须大于 0".to_string());
        }
        if self.max_message_size.as_u64() == 0 {
            return Err("realtime.max_message_size 必须大于 0".to_string());
        }
        // 上限防止把内存当队列用；0 是合法值（= 同步直调）
        if self.inbound_buffer > 65_536 {
            return Err("realtime.inbound_buffer 不能超过 65536".to_string());
        }
        // 上行队列的内存是「每连接」的：`inbound_buffer × max_message_size` 才是单连接
        // 最坏占用。二者此前各自独立校验，乘起来可以轻易到 GB 级（如 65536 × 1MB）。
        let per_conn = (self.inbound_buffer as u64).saturating_mul(self.max_message_size.as_u64());
        if per_conn > MAX_INBOUND_BYTES_PER_CONN {
            return Err(format!(
                "realtime.inbound_buffer × max_message_size = {per_conn} 字节，超过单连接上限 \
                 {MAX_INBOUND_BYTES_PER_CONN} 字节（调小其中一个）"
            ));
        }
        if self.max_topic_len == 0 || self.max_topic_len > 1024 {
            return Err("realtime.max_topic_len 必须在 1..=1024 之间".to_string());
        }
        if self.max_topics == 0 {
            return Err("realtime.max_topics 必须大于 0".to_string());
        }
        if self.write_timeout.is_zero() {
            return Err("realtime.write_timeout 必须大于 0（否则慢客户端永不被回收）".to_string());
        }
        for origin in &self.allowed_origins {
            validate_origin(origin)?;
        }
        Ok(())
    }
}

/// 单连接上行队列的内存上限（64 MiB）。
pub const MAX_INBOUND_BYTES_PER_CONN: u64 = 64 * 1024 * 1024;

/// `allowed_origins` 的单项校验：`scheme://host[:port]`，不带路径 / 尾部斜杠。
///
/// 与 `[http].cors.allowed_origins` 同一套规则——浏览器发来的 `Origin` 就是
/// `scheme://host[:port]` 形态，带路径或尾斜杠的值永远匹配不上，属于静默失效。
fn validate_origin(origin: &str) -> Result<(), String> {
    let trimmed = origin.trim();
    if trimmed != origin || origin.is_empty() {
        return Err("realtime.allowed_origins 不能含首尾空白或空串".to_string());
    }
    if origin.contains(|c: char| c.is_control()) {
        return Err("realtime.allowed_origins 不能含控制字符".to_string());
    }
    let Some((scheme, rest)) = origin.split_once("://") else {
        return Err(format!(
            "realtime.allowed_origins 项 {origin} 缺少 scheme（应为 scheme://host[:port]）"
        ));
    };
    if scheme.is_empty() || rest.is_empty() {
        return Err(format!(
            "realtime.allowed_origins 项 {origin} 不是合法 origin"
        ));
    }
    if rest.contains('/') || rest.contains('?') || rest.contains('#') {
        return Err(format!(
            "realtime.allowed_origins 项 {origin} 不能带路径 / 查询 / 尾斜杠"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_default_to_documented_values() {
        let s = RealtimeSection::default();
        assert_eq!(s.heartbeat_interval, Duration::from_secs(30));
        assert_eq!(s.heartbeat_timeout, Duration::from_secs(60));
        assert_eq!(s.shutdown_timeout, Duration::from_secs(10));
        assert_eq!(s.max_connections, 10_000);
        assert_eq!(s.max_connections_per_ip, 100);
        assert_eq!(s.max_connections_per_subject, 20);
        assert_eq!(s.max_message_size_bytes(), 64 * 1000);
        assert!(!s.forward_via_queue);
        assert_eq!(s.inbound_buffer, 64);
        assert_eq!(s.inbound_overflow, InboundOverflow::Close);
        assert_eq!(s.max_topic_len, 128);
        assert_eq!(s.max_topics, 10_000);
        assert_eq!(s.write_timeout, Duration::from_secs(10));
        assert_eq!(s.inbound_rate_per_sec, 50);
        assert!(
            s.allowed_origins.is_empty(),
            "默认不校验 Origin（保持兼容）"
        );
        assert!(s.validate().is_ok());
    }

    #[test]
    fn should_reject_inbound_memory_blowup() {
        // 64 × 1MB = 64MB 恰好在上限；再大一档就拒绝
        let s = RealtimeSection {
            inbound_buffer: 64,
            max_message_size: Byte::from_u64(1024 * 1024),
            ..RealtimeSection::default()
        };
        assert!(s.validate().is_ok(), "64MiB 应被接受");

        let s = RealtimeSection {
            inbound_buffer: 65,
            max_message_size: Byte::from_u64(1024 * 1024),
            ..RealtimeSection::default()
        };
        assert!(s.validate().is_err(), "超过 64MiB 应拒绝");
    }

    #[test]
    fn should_reject_bad_topic_and_origin_settings() {
        let s = RealtimeSection {
            max_topic_len: 0,
            ..RealtimeSection::default()
        };
        assert!(s.validate().is_err());

        let s = RealtimeSection {
            max_topics: 0,
            ..RealtimeSection::default()
        };
        assert!(s.validate().is_err());

        let s = RealtimeSection {
            write_timeout: Duration::ZERO,
            ..RealtimeSection::default()
        };
        assert!(s.validate().is_err());

        for bad in [
            "https://a.com/",
            "a.com",
            "https://a.com?x=1",
            " https://a.com",
        ] {
            let s = RealtimeSection {
                allowed_origins: vec![bad.to_string()],
                ..RealtimeSection::default()
            };
            assert!(s.validate().is_err(), "{bad} 应被拒");
        }

        let s = RealtimeSection {
            allowed_origins: vec![
                "https://a.com".to_string(),
                "http://127.0.0.1:8080".to_string(),
            ],
            ..RealtimeSection::default()
        };
        assert!(s.validate().is_ok());
    }

    #[test]
    fn should_allow_zero_inbound_buffer() {
        // 0 = 同步直调处理器（旧行为），合法
        let s = RealtimeSection {
            inbound_buffer: 0,
            ..RealtimeSection::default()
        };
        assert!(s.validate().is_ok());
    }

    #[test]
    fn should_reject_oversized_inbound_buffer() {
        let s = RealtimeSection {
            inbound_buffer: 100_000,
            ..RealtimeSection::default()
        };
        assert!(s.validate().is_err());
    }

    #[test]
    fn should_reject_timeout_not_greater_than_interval() {
        let s = RealtimeSection {
            heartbeat_timeout: Duration::from_secs(30),
            ..RealtimeSection::default()
        };
        assert!(s.validate().is_err(), "超时 == 间隔 应被拒绝");
    }

    #[test]
    fn should_reject_zero_limits() {
        let s = RealtimeSection {
            max_connections: 0,
            ..RealtimeSection::default()
        };
        assert!(s.validate().is_err());
    }

    #[test]
    fn should_parse_durations_and_size() {
        let json = r#"
            {
                "heartbeat_interval": "10s",
                "heartbeat_timeout": "20s",
                "max_message_size": "1MB",
                "max_connections": 500
            }
        "#;
        let s: RealtimeSection = serde_json::from_str(json).unwrap();
        assert_eq!(s.heartbeat_interval, Duration::from_secs(10));
        assert_eq!(s.heartbeat_timeout, Duration::from_secs(20));
        assert_eq!(
            s.max_message_size_bytes(),
            1_000_000,
            "KB/MB 走 1000 进制（KiB 才是 1024）"
        );
        assert_eq!(s.max_connections, 500);
    }
}
