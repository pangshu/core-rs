//! IP 地理定位服务与查询缓存。

use std::collections::{HashMap, VecDeque};
use std::net::IpAddr;
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};

use crate::config::GeoIpSection;
use crate::errors::AppResult;
use crate::geoip::model::GeoInfo;
use crate::geoip::reader::GeoReader;

/// 缓存分片数上限。
///
/// 每片一把锁：单把全局锁会让所有请求在地理查询这一热点上串行化（每请求两次
/// 加锁），高并发时直接成为吞吐天花板。分片后锁争用降到约 `1/N`。
const MAX_SHARDS: usize = 16;

/// 地理定位服务（`AppState.geoip`）。
///
/// 带**有界**查询缓存（`[geoip].cache_size`，含负结果）；`cache_size = 0` 关闭缓存。
/// 库文件缺失时**降级运行**（[`available`](Self::available) 为 `false`，查询一律 `None`），
/// 便于未持有 MaxMind 许可证的开发环境直接跑起来。
///
/// 缓存语义：**分片 + FIFO 逐条淘汰**。满时不整体清空（那会让一次扫描把全表
/// 有效条目清掉、命中率归零），只淘汰所在分片最旧的一条，容量恒定在
/// `cache_size` 以内。缓存可重建，故不追求 LRU 精度。
#[derive(Clone)]
pub struct GeoIp {
    reader: Option<Arc<GeoReader>>,
    cache_size: usize,
    cache: Option<Arc<GeoCache>>,
}

impl std::fmt::Debug for GeoIp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GeoIp")
            .field("available", &self.reader.is_some())
            .field("cache_size", &self.cache_size)
            .finish_non_exhaustive()
    }
}

impl GeoIp {
    /// 按 `[geoip]` 配置装配。
    ///
    /// - `city_db` 不存在 → **降级**（warn，不报错）
    /// - `city_db` 存在但打不开 → 返回错误（配置/文件损坏，启动期暴露）
    ///
    /// # Errors
    /// 城市库/ASN 库格式非法时返回 `50005`。
    pub fn from_config(config: &GeoIpSection) -> AppResult<Self> {
        let city_db = Path::new(&config.city_db);
        let reader = if city_db.is_file() {
            let asn_db = config.asn_db.as_deref().map(Path::new);
            Some(Arc::new(GeoReader::open(city_db, asn_db)?))
        } else {
            tracing::warn!(
                path = %city_db.display(),
                "GeoIP 城市库不存在，geoip 降级运行（查询一律返回 None）；\
                 可从 MaxMind 下载 GeoLite2-City.mmdb 后放到该路径"
            );
            None
        };

        Ok(Self {
            reader,
            cache_size: config.cache_size,
            cache: (config.cache_size > 0).then(|| Arc::new(GeoCache::new(config.cache_size))),
        })
    }

    /// 是否可用（库已加载）。
    #[must_use]
    pub fn available(&self) -> bool {
        self.reader.is_some()
    }

    /// 查询 IP 的地理信息；不可用或无记录时返回 `None`。
    ///
    /// 入参先归一化（`::ffff:a.b.c.d` → `a.b.c.d`）：不经 [`ClientIp`] 直接调用
    /// 本方法的代码也能与提取器共用同一份缓存条目。
    ///
    /// [`ClientIp`]: crate::utils::ClientIp
    #[must_use]
    pub fn lookup(&self, ip: IpAddr) -> Option<GeoInfo> {
        let Some(reader) = &self.reader else {
            return None;
        };
        let ip = ip.to_canonical();

        let Some(cache) = &self.cache else {
            return reader.lookup(ip);
        };

        if let Some(hit) = cache.get(ip) {
            return hit;
        }

        let info = reader.lookup(ip);
        cache.put(ip, info.clone());
        info
    }
}

/// 分片缓存。
struct GeoCache {
    shards: Box<[Mutex<Shard>]>,
}

impl GeoCache {
    /// 按总容量建缓存；`cache_size` 必须 > 0（`0` 由 [`GeoIp`] 解释为关闭）。
    ///
    /// 分片数不超过总容量，且每片容量 = `cache_size / 分片数`，保证
    /// **总条目数恒 ≤ `cache_size`**（各片容量向下取整会略低于上限）。
    fn new(cache_size: usize) -> Self {
        let count = cache_size.clamp(1, MAX_SHARDS);
        let per_shard = (cache_size / count).max(1);
        let shards = (0..count)
            .map(|_| Mutex::new(Shard::new(per_shard)))
            .collect::<Vec<_>>();
        Self {
            shards: shards.into_boxed_slice(),
        }
    }

    fn shard(&self, ip: IpAddr) -> &Mutex<Shard> {
        // `new` 保证至少 1 片，取模不会越界
        &self.shards[shard_index(ip, self.shards.len())]
    }

    /// 命中时返回 `Some(值)`；未命中返回 `None`（值本身也可能是 `None`）。
    fn get(&self, ip: IpAddr) -> Option<Option<GeoInfo>> {
        // 锁中毒不放弃缓存：数据本身仍然可用，静默跳过会让缓存**永久**失效
        let shard = self
            .shard(ip)
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        shard.get(ip).cloned()
    }

    fn put(&self, ip: IpAddr, info: Option<GeoInfo>) {
        let mut shard = self
            .shard(ip)
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        shard.put(ip, info);
    }

    /// 当前条目总数（仅测试用）。
    #[cfg(test)]
    fn total_len(&self) -> usize {
        self.shards
            .iter()
            .map(|shard| {
                shard
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .map
                    .len()
            })
            .sum()
    }
}

/// 单分片：`HashMap` + 插入顺序队列，满时淘汰最旧一条。
struct Shard {
    map: HashMap<IpAddr, Option<GeoInfo>>,
    order: VecDeque<IpAddr>,
    cap: usize,
}

impl Shard {
    fn new(cap: usize) -> Self {
        Self {
            map: HashMap::new(),
            order: VecDeque::new(),
            cap,
        }
    }

    fn get(&self, ip: IpAddr) -> Option<&Option<GeoInfo>> {
        self.map.get(&ip)
    }

    fn put(&mut self, ip: IpAddr, info: Option<GeoInfo>) {
        // 已有键：原地覆盖，不改变 FIFO 位置（避免热点 IP 被反复重排）
        if let Some(slot) = self.map.get_mut(&ip) {
            *slot = info;
            return;
        }
        while self.map.len() >= self.cap {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            self.map.remove(&oldest);
        }
        self.map.insert(ip, info);
        self.order.push_back(ip);
    }
}

/// 选择分片：乘法散列取高位后取模。
///
/// 不用 `DefaultHasher`（SipHash，每次查询要构造 hasher）；也不用低位取模——
/// IPv4 尾段 / IPv6 接口标识的低位分布不均，直接取模会让热点网段挤在同一片。
fn shard_index(ip: IpAddr, shards: usize) -> usize {
    let raw = match ip {
        IpAddr::V4(v4) => u64::from(u32::from(v4)),
        IpAddr::V6(v6) => {
            let o = v6.octets();
            u64::from_be_bytes([o[8], o[9], o[10], o[11], o[12], o[13], o[14], o[15]])
        }
    };
    (raw.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 32) as usize % shards
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use super::*;

    fn config(city_db: &str) -> GeoIpSection {
        GeoIpSection {
            city_db: city_db.to_string(),
            ..GeoIpSection::default()
        }
    }

    fn v4(last: u32) -> IpAddr {
        IpAddr::V4(Ipv4Addr::from(0x0A00_0000 + last))
    }

    #[test]
    fn should_degrade_when_city_db_missing() {
        let service = GeoIp::from_config(&config("data/not-here.mmdb")).unwrap();
        assert!(!service.available(), "缺库时应降级");
        assert!(
            service.lookup("8.8.8.8".parse().unwrap()).is_none(),
            "降级后查询应返回 None"
        );
    }

    #[test]
    fn should_report_documented_defaults() {
        let section = GeoIpSection::default();
        assert_eq!(section.city_db, "data/GeoLite2-City.mmdb");
        assert!(section.asn_db.is_none());
        assert_eq!(section.cache_size, 10_000);
    }

    #[test]
    fn should_disable_cache_when_size_is_zero() {
        let mut section = config("data/not-here.mmdb");
        section.cache_size = 0;
        let service = GeoIp::from_config(&section).unwrap();
        assert!(service.cache.is_none(), "cache_size = 0 应关闭缓存");
    }

    #[test]
    fn should_keep_total_entries_within_cache_size() {
        for size in [1_usize, 4, 10, 100, 10_000] {
            let cache = GeoCache::new(size);
            // 写满并溢出，验证分片容量之和不超过配置上限
            for i in 0..(size as u32 + 64) {
                cache.put(v4(i), None);
            }
            assert!(
                cache.total_len() <= size,
                "cache_size = {size} 时实际条目 {} 超限",
                cache.total_len()
            );
        }
    }

    #[test]
    fn should_evict_one_entry_at_a_time_instead_of_clearing() {
        // 旧实现：满了 `cache.clear()`，一次插入会把整表清空（64 → 1）
        let cache = GeoCache::new(64);
        for i in 0..64 {
            cache.put(v4(i), None);
        }
        let before = cache.total_len();
        cache.put(v4(9999), None);
        let after = cache.total_len();

        assert!(after <= 64, "容量不得超过 cache_size，实际 {after}");
        assert!(
            after >= before,
            "单次插入最多淘汰 1 条，实际从 {before} 掉到 {after}（旧实现会清空整表）"
        );
    }

    #[test]
    fn should_cache_negative_result() {
        let cache = GeoCache::new(16);
        let ip = v4(1);
        assert!(cache.get(ip).is_none(), "未写入时应未命中");
        cache.put(ip, None);
        assert_eq!(cache.get(ip), Some(None), "负结果也应被缓存（防穿透）");
    }

    #[test]
    fn should_overwrite_existing_key_in_place() {
        let cache = GeoCache::new(16);
        let ip = v4(7);
        let info = GeoInfo {
            country_code: Some("CN".to_string()),
            ..GeoInfo::default()
        };
        cache.put(ip, None);
        cache.put(ip, Some(info.clone()));
        assert_eq!(cache.get(ip), Some(Some(info)), "同键覆盖不应产生重复条目");
        assert_eq!(cache.total_len(), 1);
    }

    #[test]
    fn should_recover_from_poisoned_shard() {
        let cache = GeoCache::new(16);
        let ip = v4(42);

        // 持锁 panic 制造中毒
        let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = cache.shard(ip).lock().unwrap();
            panic!("制造锁中毒");
        }));
        assert!(poisoned.is_err());

        // 中毒后缓存仍应可用，而不是永久静默跳过
        cache.put(ip, None);
        assert!(
            cache.get(ip).is_some(),
            "锁中毒后应恢复（PoisonError::into_inner），而非永久禁用缓存"
        );
    }
}
