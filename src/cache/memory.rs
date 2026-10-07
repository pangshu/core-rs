//! 进程内缓存后端（feature = "cache-memory"，默认）：moka TTL 缓存，
//! 零外部依赖，单机部署与开发/测试的默认值。
//!
//! 计数（`incr`）不走路由 moka——读-改-写跨 await 会丢计数，改为专用
//! `Mutex<HashMap>` 计数表：锁内无 await，原子递增，窗口 TTL 首次计数时固定。

use std::collections::HashMap; // 引入哈希表，作为计数器的存储结构
use std::sync::Mutex; // 引入互斥锁，保护计数表实现原子递增
use std::time::{Duration, Instant}; // 引入时长与单调时钟，用于 TTL 与过期判定

use super::{Cache, CacheError}; // 引入缓存契约与错误类型
use crate::config::sections::CacheMemorySettings; // 引入内存缓存配置节

/// 内存后端条目：值 + 写入时指定的 ttl（供 Expiry 在创建/更新时计算过期）
#[derive(Debug, Clone)] // 派生调试与克隆
struct MemEntry { // moka 中存储的缓存条目
    value: String, // 缓存字符串值
    ttl: Option<Duration>, // 写入时指定的 TTL，None 表示用默认
}

/// 计数条目：原子递增的值 + 窗口到期时刻（None = 不过期，PERSIST）
#[derive(Debug, Clone)] // 派生调试与克隆
struct Counter { // incr 专用计数器条目
    value: i64, // 当前计数值
    expires_at: Option<Instant>, // 窗口到期时刻，None 表示不过期
}

/// 按「条目自身 ttl，缺省用配置的默认 ttl」计算过期；两者皆无则不过期
struct MemExpiry { // 自定义 moka 过期策略
    default_ttl: Option<Duration>, // 配置的默认 TTL
}

impl moka::Expiry<String, MemEntry> for MemExpiry { // 为 MemExpiry 实现 moka 过期 trait
    fn expire_after_create( // 新建条目时计算存活时长
        &self, // 策略自身引用
        _key: &String, // 键（此处未使用）
        entry: &MemEntry, // 新建的条目
        _created_at: std::time::Instant, // 创建时刻（此处未使用）
    ) -> Option<Duration> { // 返回过期时长
        entry.ttl.or(self.default_ttl) // 优先用条目自身 ttl，缺省回落到默认 ttl
    }

    fn expire_after_update( // 更新条目时计算存活时长
        &self, // 策略自身引用
        _key: &String, // 键（此处未使用）
        entry: &MemEntry, // 更新后的条目
        _updated_at: std::time::Instant, // 更新时刻（此处未使用）
        _current_duration: Option<Duration>, // 当前剩余时长（此处未使用）
    ) -> Option<Duration> { // 返回过期时长
        entry.ttl.or(self.default_ttl) // 与创建时一致：条目 ttl 优先，缺省用默认
    }
}

#[derive(Debug)] // 派生调试
pub struct MemoryCache { // 进程内缓存后端实现
    inner: moka::future::Cache<String, MemEntry>, // moka 异步缓存主体
    /// incr 专用计数表：锁内无 await 点，读-改-写原子（限流的正确性依赖于此）
    counters: Mutex<HashMap<String, Counter>>, // 计数器表，互斥保护
}

impl MemoryCache { // 内存缓存实现块
    pub fn new(settings: &CacheMemorySettings) -> Self { // 依据配置构造内存缓存
        let default_ttl = (settings.default_ttl_secs > 0) // 仅当配置值大于 0 时才设默认 TTL
            .then(|| Duration::from_secs(settings.default_ttl_secs)); // 把秒数转成 Duration，否则为 None
        Self { // 构造实例
            inner: moka::future::Cache::builder() // 开始构建 moka 缓存
                .max_capacity(settings.max_capacity.max(1)) // 容量下限取 1，避免配置 0 非法
                .expire_after(MemExpiry { default_ttl }) // 装载自定义过期策略
                .build(), // 完成构建
            counters: Mutex::new(HashMap::new()), // 初始化空的计数表
        }
    }

    fn counters(&self) -> std::sync::MutexGuard<'_, HashMap<String, Counter>> { // 获取计数表锁守卫的辅助方法
        self.counters // 访问内部计数表
            .lock() // 获取互斥锁
            .unwrap_or_else(std::sync::PoisonError::into_inner) // 锁被 poison 时取回内部值，避免连锁 panic
    }

    /// 清扫过期计数条目（高基数窗口 key 用完即忘，不清扫则无上界增长）
    fn sweep_expired(map: &mut HashMap<String, Counter>) { // 按阈值顺带清理过期计数条目
        if map.len() <= 1024 { // 条目未超阈值时无需清扫
            return; // 直接返回
        }
        let now = Instant::now(); // 取当前时刻，供批量比较
        map.retain(|_, c| c.expires_at.map(|e| e > now).unwrap_or(true)); // 仅保留未过期（或不过期）的条目
    }

    /// 计数条目的窗口操作；key 不在计数表时返回 None（走 moka 路径）
    fn expire_counter(&self, key: &str, ttl: Option<Duration>) -> Option<bool> { // 同步处理计数条目的 expire
        let mut map = self.counters(); // 获取计数表锁
        let c = map.get_mut(key)?; // 取计数条目，不存在则返回 None 交由 moka 路径
        if c.expires_at.map(|e| e <= Instant::now()).unwrap_or(false) { // 若窗口已过期
            return Some(false); // 视为键不存在，返回 false
        }
        c.expires_at = match ttl { // 依据 ttl 重设窗口
            Some(d) if d > Duration::ZERO => Some(Instant::now() + d), // 正时长则设为当前时刻 + d
            // PERSIST：None / ZERO 清除窗口
            _ => None, // 其余情况清除窗口，即永不过期
        };
        Some(true) // 重设成功返回 true
    }
}

#[async_trait::async_trait] // 启用 async_trait 以实现异步 trait
impl Cache for MemoryCache { // 为内存缓存实现 Cache 契约
    async fn get(&self, key: &str) -> Result<Option<String>, CacheError> { // 读取缓存值
        // 计数表优先：incr 写入的值对普通 get 可见
        {
            let map = self.counters(); // 获取计数表锁
            if let Some(c) = map.get(key) { // 计数表中存在该键
                let expired = c.expires_at.map(|e| e <= Instant::now()).unwrap_or(false); // 判断计数窗口是否已过期
                if !expired { // 未过期则直接返回计数值
                    return Ok(Some(c.value.to_string())); // 把计数值转字符串返回
                }
            }
        }
        Ok(self.inner.get(key).await.map(|e| e.value)) // 回落 moka 缓存，命中取 value
    }

    async fn set(&self, key: &str, value: &str, ttl: Option<Duration>) -> Result<(), CacheError> { // 写入缓存值
        // 同 key 的计数被显式 set 覆盖：移除计数条目（last write wins）
        self.counters().remove(key); // 显式 set 时移除同键计数条目
        self.inner // 操作 moka 缓存
            .insert(key.to_string(), MemEntry { // 插入新条目
                value: value.to_string(), // 存储字符串值
                ttl, // 记录写入时指定的 TTL
            })
            .await; // 等待异步插入完成
        Ok(()) // 返回成功
    }

    async fn del(&self, key: &str) -> Result<(), CacheError> { // 删除缓存键
        self.counters().remove(key); // 同步移除计数条目
        self.inner.invalidate(key).await; // 使 moka 缓存条目失效
        Ok(()) // 返回成功
    }

    /// 原子递增（锁内无 await 点）：首次计数后由调用方 `expire` 设窗口 TTL。
    /// 多进程部署仍需 redis 后端——本原语只保证**进程内**原子。
    async fn incr(&self, key: &str, delta: i64) -> Result<i64, CacheError> { // 对计数键原子累加
        let mut map = self.counters(); // 获取计数表锁（后续无 await）
        Self::sweep_expired(&mut map); // 超阈值时顺带清扫过期条目
        let now = Instant::now(); // 取当前时刻供过期判断
        let entry = map.entry(key.to_string()).or_insert(Counter { // 取或新建计数条目
            value: 0, // 新条目从 0 起算
            expires_at: None, // 新建无窗口 TTL
        });
        // 已过期的窗口从 0 重新计数
        if entry.expires_at.map(|e| e <= now).unwrap_or(false) { // 若旧窗口已过期
            entry.value = 0; // 计数归零重新开始
            entry.expires_at = None; // 清除窗口
        }
        entry.value += delta; // 累加增量
        Ok(entry.value) // 返回累加后的值
    }

    /// 计数条目：Some(ZERO/非正) = PERSIST（清除窗口）；Some(>0) = 设窗口。
    /// moka 条目 TTL 写入时固定：实现为「读出 → 带 TTL 重写」，对原子语义
    /// 敏感的场景请用 redis 后端。
    async fn expire(&self, key: &str, ttl: Option<Duration>) -> Result<bool, CacheError> { // 重设键的 TTL
        // 计数表路径在同步函数里完成（锁卫绝不跨 await）
        if let Some(result) = self.expire_counter(key, ttl) { // 命中计数表则直接返回结果
            return Ok(result); // 返回同步路径结果
        }
        let Some(value) = self.get(key).await? else { // moka 路径：先读出当前值
            return Ok(false); // 键不存在返回 false
        };
        self.set(key, &value, ttl.filter(|d| *d > Duration::ZERO)).await?; // 过滤零值后带 TTL 重写实现续期
        Ok(true) // 重写成功返回 true
    }

    /// 进程内缓存无独立组件，进程在即可用
    async fn ping(&self) -> Result<(), CacheError> { // 探活内存后端
        Ok(()) // 进程存活即视为健康
    }
}
