//! 配置反序列化辅助：时长（`humantime`）与大小（`byte-unit`）。
//!
//! 两者均接受**字符串**（如 `"10s"` / `"2MB"`）或**裸数字**（duration = 秒，
//! size = 字节），见 `docs/08-config-spec.md` §2.8。

use std::fmt;
use std::time::Duration;

use byte_unit::Byte;
use serde::de::{self, Unexpected, Visitor};

/// 反序列化时长：`"10s"` / `"5m"` / `"1h"` / `"7d"` 或整数秒。
///
/// **负数一律报错**：此前 `-1` 会被静默折成 `0`（`Duration::ZERO`），
/// 于是 `request_timeout = -1` 变成「立即超时」、`body_limit = -1` 变成
/// 「所有请求体都被拒」——配置写错却没有任何提示。
pub(crate) fn de_duration<'de, D>(deserializer: D) -> Result<Duration, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct DurationVisitor;

    impl Visitor<'_> for DurationVisitor {
        type Value = Duration;

        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("时长字符串（如 \"10s\"）或非负整数秒")
        }

        fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
            humantime::parse_duration(v).map_err(de::Error::custom)
        }

        fn visit_u64<E: de::Error>(self, v: u64) -> Result<Self::Value, E> {
            Ok(Duration::from_secs(v))
        }

        fn visit_i64<E: de::Error>(self, v: i64) -> Result<Self::Value, E> {
            let secs = u64::try_from(v)
                .map_err(|_| de::Error::invalid_value(Unexpected::Signed(v), &"非负整数（秒）"))?;
            Ok(Duration::from_secs(secs))
        }
    }

    deserializer.deserialize_any(DurationVisitor)
}

/// 反序列化**可选**时长：字段缺省时保持 `None`，出现时走 [`de_duration`]。
///
/// 用于「缺省沿用上层配置」的字段（如 `[[ratelimit.rules]].window`）。
/// 注意必须配合 `#[serde(default)]`：没有 `default` 时字段缺失会直接报错，
/// 而不会走到这里。
///
/// 目前只有 `[ratelimit]` 用到；未启用该 feature 时按 dead_code 豁免，
/// 否则单 feature 构建（`-D warnings`）会失败。
#[cfg_attr(not(feature = "ratelimit"), allow(dead_code))]
pub(crate) fn de_opt_duration<'de, D>(deserializer: D) -> Result<Option<Duration>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct OptionalDurationVisitor;

    impl<'de> Visitor<'de> for OptionalDurationVisitor {
        type Value = Option<Duration>;

        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("时长字符串（如 \"10s\"）、非负整数秒，或 null")
        }

        fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_some<D2: serde::Deserializer<'de>>(self, d: D2) -> Result<Self::Value, D2::Error> {
            de_duration(d).map(Some)
        }
    }

    deserializer.deserialize_option(OptionalDurationVisitor)
}

/// 反序列化大小：`"2MB"` / `"50MB"` 或整数字节。
///
/// 负数同样报错（见 [`de_duration`] 的说明）。
pub(crate) fn de_byte<'de, D>(deserializer: D) -> Result<Byte, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct ByteVisitor;

    impl Visitor<'_> for ByteVisitor {
        type Value = Byte;

        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("大小字符串（如 \"2MB\"）或非负整数字节")
        }

        fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
            Byte::parse_str(v, true).map_err(de::Error::custom)
        }

        fn visit_u64<E: de::Error>(self, v: u64) -> Result<Self::Value, E> {
            Ok(Byte::from_u64(v))
        }

        fn visit_i64<E: de::Error>(self, v: i64) -> Result<Self::Value, E> {
            let bytes = u64::try_from(v).map_err(|_| {
                de::Error::invalid_value(Unexpected::Signed(v), &"非负整数（字节）")
            })?;
            Ok(Byte::from_u64(bytes))
        }
    }

    deserializer.deserialize_any(ByteVisitor)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Deserialize)]
    struct Holder {
        #[serde(deserialize_with = "de_duration")]
        timeout: Duration,
        #[serde(deserialize_with = "de_byte")]
        limit: Byte,
    }

    #[test]
    fn should_parse_positive_values() {
        let h: Holder = serde_json::from_str(r#"{ "timeout": "10s", "limit": "2MB" }"#).unwrap();
        assert_eq!(h.timeout, Duration::from_secs(10));
        assert_eq!(h.limit.as_u64(), 2 * 1000 * 1000);

        let h: Holder = serde_json::from_str(r#"{ "timeout": 5, "limit": 1024 }"#).unwrap();
        assert_eq!(h.timeout, Duration::from_secs(5));
        assert_eq!(h.limit.as_u64(), 1024);
    }

    #[test]
    fn should_reject_negative_values() {
        // 负数此前会被静默折成 0，现在必须报错
        assert!(serde_json::from_str::<Holder>(r#"{ "timeout": -1, "limit": 1024 }"#).is_err());
        assert!(serde_json::from_str::<Holder>(r#"{ "timeout": 1, "limit": -1 }"#).is_err());
    }
}
