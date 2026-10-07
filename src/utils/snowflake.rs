//! 雪花 id（Snowflake）：64 位趋势递增唯一 id。
//!
//! 布局（41 + 10 + 12）：`sign(1) | timestamp_ms(41) | machine(10) | seq(12)`。
//! 单实例内由内部 Mutex 串行保证不重号；多实例用 `machine_id`（0~1023）区分，
//! 时钟回拨超过容忍窗口时直接报错（宁可失败也不发重复 id）。

use std::sync::Mutex; // 引入互斥锁，串行化发号过程
use std::time::{Duration, SystemTime, UNIX_EPOCH}; // 引入时长、系统时间与 Unix 纪元常量

use crate::error::AppError; // 引入框架统一错误类型

/// 自定义纪元：2026-01-01T00:00:00Z（毫秒）。41 位可用约 69 年。
const EPOCH_MS: u64 = 1_767_225_600_000; // 自定义纪元时间戳（毫秒）

/// 时钟回拨容忍窗口：回拨在此范围内自旋等待，超过则报错
const MAX_BACKWARD_MS: u64 = 5; // 允许的时钟回拨上限（毫秒）

#[derive(Debug)] // 派生调试输出
pub struct Snowflake { // 雪花 id 生成器
    machine_id: u64, // 机器/实例编号（0~1023）
    state: Mutex<(i64 /* last_ms */, u16 /* seq */)>, // 受锁保护的（上次毫秒时间戳, 同毫秒序列号）
}

impl Snowflake { // 雪花生成器方法集
    /// `machine_id` 取值 0~1023，多实例部署用配置区分
    pub fn new(machine_id: u16) -> Result<Self, AppError> { // 用机器号构造生成器
        if machine_id > 1023 { // 10 位机器号上限为 1023
            return Err(AppError::internal(format!( // 越界直接报内部错误
                "snowflake machine_id {machine_id} out of range 0..=1023"
            )));
        }
        Ok(Self { // 组装生成器实例
            machine_id: machine_id as u64, // 保存机器号
            state: Mutex::new((0, 0)), // 初始状态：无上次时间戳、序列号 0
        })
    }

    /// 生成下一个 id
    pub fn next(&self) -> Result<i64, AppError> { // 生成下一个趋势递增 id
        let mut st = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner); // 取状态锁（被 poison 则取内部值，避免连锁 panic）
        let mut now = now_ms()?; // 取当前毫秒时间戳

        // 时钟被设到纪元之前（如误设到 2026 年前）：高位为负/垃圾，拒绝发号
        if now < EPOCH_MS as i64 { // 当前时间早于自定义纪元
            return Err(AppError::internal(format!( // 拒绝发号并报错
                "snowflake: system clock ({now}ms) is before the custom epoch, refusing to emit ids"
            )));
        }

        // 时钟回拨：小回拨等待追平，大回拨拒绝发号
        if now < st.0 { // 当前时间小于上次发号时间，说明发生回拨
            let backward = (st.0 - now) as u64; // 计算回拨幅度
            if backward > MAX_BACKWARD_MS { // 回拨超过容忍窗口
                return Err(AppError::internal(format!( // 拒绝发号并报错
                    "snowflake clock moved backward {backward}ms"
                )));
            }
            now = spin_until(st.0)?; // 小回拨：自旋等待追平上次时间
        }

        if now == st.0 { // 与上次同一毫秒
            st.1 = st.1.checked_add(1).ok_or_else(|| { // 序列号自增（溢出则报错）
                AppError::internal("snowflake sequence overflow in same millisecond")
            })?;
            if st.1 > 4095 { // 12 位序列号耗尽（超过 4095）
                now = spin_until(now + 1)?; // 自旋等待进入下一毫秒
                st.0 = now; // 更新上次时间戳
                st.1 = 0; // 序列号归零
            }
        } else { // 进入新毫秒
            st.0 = now; // 更新上次时间戳
            st.1 = 0; // 序列号归零
        }

        Ok(((now - EPOCH_MS as i64) << 22) // 时间戳部分左移 22 位（10 机器位 + 12 序列位）
            | ((self.machine_id as i64) << 12) // 机器号部分左移 12 位
            | st.1 as i64) // 或上序列号，拼出最终 64 位 id
    }
}

fn now_ms() -> Result<i64, AppError> { // 取当前毫秒时间戳
    Ok(SystemTime::now() // 取系统当前时间
        .duration_since(UNIX_EPOCH) // 计算距 Unix 纪元时长
        .map_err(|e| AppError::internal(format!("system clock before epoch: {e}")))? // 时间早于纪元则报错
        .as_millis() as i64) // 转为毫秒 i64
}

/// 自旋等待到目标毫秒（回拨 / 同毫秒序列耗尽时）
fn spin_until(target_ms: i64) -> Result<i64, AppError> { // 自旋等待直到到达目标毫秒
    loop { // 循环直到时间达标
        let now = now_ms()?; // 取当前毫秒
        if now >= target_ms { // 已到达或超过目标
            return Ok(now); // 返回当前时间
        }
        std::thread::sleep(Duration::from_millis((target_ms - now).min(1) as u64)); // 睡眠至多 1ms 后重试，避免忙等
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
