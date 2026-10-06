//! 雪花 id（Snowflake）：64 位趋势递增唯一 id。
//!
//! 布局（41 + 10 + 12）：`sign(1) | timestamp_ms(41) | machine(10) | seq(12)`。
//! 单实例内由内部 Mutex 串行保证不重号；多实例用 `machine_id`（0~1023）区分，
//! 时钟回拨超过容忍窗口时直接报错（宁可失败也不发重复 id）。

use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::error::AppError;

/// 自定义纪元：2026-01-01T00:00:00Z（毫秒）。41 位可用约 69 年。
const EPOCH_MS: u64 = 1_767_225_600_000;

/// 时钟回拨容忍窗口：回拨在此范围内自旋等待，超过则报错
const MAX_BACKWARD_MS: u64 = 5;

#[derive(Debug)]
pub struct Snowflake {
    machine_id: u64,
    state: Mutex<(i64 /* last_ms */, u16 /* seq */)>,
}

impl Snowflake {
    /// `machine_id` 取值 0~1023，多实例部署用配置区分
    pub fn new(machine_id: u16) -> Result<Self, AppError> {
        if machine_id > 1023 {
            return Err(AppError::internal(format!(
                "snowflake machine_id {machine_id} out of range 0..=1023"
            )));
        }
        Ok(Self {
            machine_id: machine_id as u64,
            state: Mutex::new((0, 0)),
        })
    }

    /// 生成下一个 id
    pub fn next(&self) -> Result<i64, AppError> {
        let mut st = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut now = now_ms()?;

        // 时钟被设到纪元之前（如误设到 2026 年前）：高位为负/垃圾，拒绝发号
        if now < EPOCH_MS as i64 {
            return Err(AppError::internal(format!(
                "snowflake: system clock ({now}ms) is before the custom epoch, refusing to emit ids"
            )));
        }

        // 时钟回拨：小回拨等待追平，大回拨拒绝发号
        if now < st.0 {
            let backward = (st.0 - now) as u64;
            if backward > MAX_BACKWARD_MS {
                return Err(AppError::internal(format!(
                    "snowflake clock moved backward {backward}ms"
                )));
            }
            now = spin_until(st.0)?;
        }

        if now == st.0 {
            st.1 = st.1.checked_add(1).ok_or_else(|| {
                AppError::internal("snowflake sequence overflow in same millisecond")
            })?;
            if st.1 > 4095 {
                now = spin_until(now + 1)?;
                st.0 = now;
                st.1 = 0;
            }
        } else {
            st.0 = now;
            st.1 = 0;
        }

        Ok(((now - EPOCH_MS as i64) << 22)
            | ((self.machine_id as i64) << 12)
            | st.1 as i64)
    }
}

fn now_ms() -> Result<i64, AppError> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| AppError::internal(format!("system clock before epoch: {e}")))?
        .as_millis() as i64)
}

/// 自旋等待到目标毫秒（回拨 / 同毫秒序列耗尽时）
fn spin_until(target_ms: i64) -> Result<i64, AppError> {
    loop {
        let now = now_ms()?;
        if now >= target_ms {
            return Ok(now);
        }
        std::thread::sleep(Duration::from_millis((target_ms - now).min(1) as u64));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_unique_and_ordered() {
        let sf = Snowflake::new(7).unwrap();
        let mut prev = 0;
        for _ in 0..10_000 {
            let id = sf.next().unwrap();
            assert!(id > prev, "ids must be strictly increasing");
            prev = id;
        }
    }

    #[test]
    fn machine_id_range() {
        assert!(Snowflake::new(1024).is_err());
        assert!(Snowflake::new(1023).is_ok());
    }
}
