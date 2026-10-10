//! 证书仓库与 SNI 解析：把业务交来的 PEM 解析为 rustls `CertifiedKey`，
//! 并提供一个读取 `ArcSwap<CertStore>` 的动态 resolver（握手时按 SNI 选证书）。
//!
//! **本模块只做机械解析，不碰任何存储**：证书字节全部来自业务 [`CertProvider`]。
//! 支持**精确域名**与**通配符域名**（`*.example.com`，只匹配一层子域），匹配为 O(1)。
//!
//! [`CertProvider`]: super::CertProvider

use std::collections::HashMap; // 引入哈希表，存储 域名 -> 证书
use std::sync::Arc; // 引入 Arc，跨线程共享证书与仓库

use arc_swap::ArcSwap; // 引入 ArcSwap，证书仓库的无锁原子替换
use rustls::pki_types::{CertificateDer, PrivateKeyDer}; // 引入 rustls 的证书/私钥 DER 类型
use rustls::server::{ClientHello, ResolvesServerCert}; // 引入 SNI 解析 trait 与握手信息
use rustls::sign::CertifiedKey; // 引入「证书链 + 签名私钥」组合类型

use super::{CertEntry, TlsError}; // 引入业务条目 DTO 与错误类型

/// 域名 -> 证书。支持**精确域名**与**通配符域名**（`*.example.com`）。
/// 同时记录到期时间，供到期探针读取。
pub struct CertStore { // 证书仓库
    /// 全部条目（归一化域名或 `*.` 模式 -> 证书），作为数据所有者
    map: HashMap<String, Arc<CertifiedKey>>, // "api.example.com" | "*.example.com" -> 证书
    /// 通配索引：`模式去掉 `*.` 后的后缀` -> 该模式在 `map` 中的键
    wildcard_index: HashMap<String, String>, // "example.com" -> "*.example.com"
    /// 归一化域名 / 模式 -> notAfter（Unix 秒）
    not_after: HashMap<String, i64>, // 键与 map 一致
}

impl Default for CertStore { // 为仓库实现默认值（空仓库）
    fn default() -> Self { // 实现 default
        Self { // 全部空表
            map: HashMap::new(), // 空证书表
            wildcard_index: HashMap::new(), // 空通配索引
            not_after: HashMap::new(), // 空到期表
        }
    }
}

impl std::fmt::Debug for CertStore { // 手写 Debug：只输出域名，绝不输出证书/私钥内容
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { // 实现格式化
        f.debug_struct("CertStore") // 结构体名
            .field("domains", &self.map.keys().collect::<Vec<_>>()) // 仅列出域名
            .finish() // 结束
    }
}

impl CertStore { // 为证书仓库实现方法
    /// 按 SNI 取证书：**精确优先 → 通配索引兜底**。
    /// **契约：`sni` 需已归一化**（小写、去尾点，见 [`DynamicResolver::resolve`]）。
    /// 通配匹配为 O(1)（查预计算索引），不做线性遍历。
    pub fn lookup(&self, sni: &str) -> Option<Arc<CertifiedKey>> { // 查找证书
        // 1) 精确优先：sni 不含 `*`，直接查 map 即精确匹配
        if let Some(k) = self.map.get(sni) { // 精确命中
            return Some(k.clone()); // 返回证书
        }
        // 2) 通配兜底：`a.example.com` -> 后缀 `example.com` -> 索引 -> 回查证书（只吃一层 label）
        if let Some((_, rest)) = sni.split_once('.') { // 取首个点之后的后缀（单 label 时为 None）
            if let Some(pattern) = self.wildcard_index.get(rest) { // 后缀命中通配索引
                return self.map.get(pattern).cloned(); // 回查证书
            }
        }
        None // 未命中
    }

    /// 到期时间表（域名 / 模式 -> Unix 秒），供到期探针读取
    pub fn not_after(&self) -> &HashMap<String, i64> { // 返回到期表引用
        &self.not_after // 借用内部字段
    }

    /// 由业务条目构建仓库。
    /// - **空域名**：整体失败（`TlsError::Missing`，与 v1 一致）；
    /// - **非法通配模式**：**跳过该条 + 告警**（不拖垮整批，见文档 09 §5.1）；
    /// - **PEM 解析失败**：整体失败（保留旧仓库，fail-safe）。
    pub fn build(entries: &[CertEntry]) -> Result<Self, TlsError> { // 构建证书仓库
        let mut map = HashMap::new(); // 待填充的域名->证书表
        let mut wildcard_index = HashMap::new(); // 待填充的通配索引
        let mut not_after = HashMap::new(); // 待填充的到期表
        for entry in entries { // 遍历全部条目
            let domain = normalize_domain(&entry.domain); // 归一化域名
            if domain.is_empty() { // 域名为空视为非法
                return Err(TlsError::Missing(entry.domain.clone())); // 返回缺失错误
            }
            if domain.contains('*') && !is_valid_wildcard(&domain) { // 非法通配模式
                tracing::warn!(domain = %domain, "skip invalid wildcard domain"); // 告警并跳过
                continue; // 不影响其余条目
            }
            let (key, expiry) = parse_certified_key(entry)?; // 解析证书链与私钥（失败即整体失败）
            if map.contains_key(&domain) { // 同域名重复
                tracing::warn!(domain = %domain, "duplicate tls domain, later entry wins"); // 告警：后者覆盖
            }
            if let Some(suffix) = domain.strip_prefix("*.") { // 通配条目
                wildcard_index.insert(suffix.to_string(), domain.clone()); // 记录「后缀 -> 模式键」
            }
            map.insert(domain.clone(), key); // 写入证书
            if let Some(ts) = expiry { // 若取到到期时间
                not_after.insert(domain, ts); // 写入到期表
            }
        }
        warn_redundant_exact(&map, &wildcard_index); // 整批入库后统一检查冗余精确条目
        Ok(Self { map, wildcard_index, not_after }) // 返回构建好的仓库
    }
}

/// 冗余告警：若某条**精确条目**已被某条**通配条目**覆盖，提示可能冗余（纯提示，不改行为）。
/// 在整批入库后统一检查，避免条目顺序导致漏判。
fn warn_redundant_exact(map: &HashMap<String, Arc<CertifiedKey>>, wildcard_index: &HashMap<String, String>) { // 检查冗余
    for exact in map.keys().filter(|k| !k.contains('*')) { // 遍历全部精确域名
        if let Some((_, rest)) = exact.split_once('.') { // 取首个点之后的后缀
            if let Some(wildcard) = wildcard_index.get(rest) { // 该后缀已被通配覆盖
                tracing::warn!( // 提示冗余
                    domain = %exact, // 精确域名
                    wildcard = %wildcard, // 覆盖它的通配模式
                    "tls exact cert entry already covered by wildcard, check if redundant" // 提示语
                );
            }
        }
    }
}

/// 判断归一化后的域名是否是**合法通配模式**（前提：调用方已确认含 `*`）。
/// 规则（RFC 6125/9525）：`*` 独占最左 label、只出现一次、后缀须为至少二级（含点）。
fn is_valid_wildcard(domain: &str) -> bool { // 校验通配模式
    let Some(rest) = domain.strip_prefix("*.") else { return false; }; // 1) 必须以 `*.` 开头
    if rest.contains('*') { return false; } // 2) 后缀不得再含 `*`（只允许一个通配符）
    rest.contains('.') // 3) 后缀须非空且至少含一个点（挡掉 `*.` / `*` / `*.com`）
}

/// 域名归一化：去首尾空白与末尾点，转小写（与 SNI 比对口径一致）
fn normalize_domain(d: &str) -> String { // 归一化域名
    d.trim().trim_end_matches('.').to_ascii_lowercase() // 归一化结果
}

/// 解析单条证书：证书链 + 私钥 -> `CertifiedKey`，并尝试取出 notAfter
fn parse_certified_key(entry: &CertEntry) -> Result<(Arc<CertifiedKey>, Option<i64>), TlsError> { // 解析一条证书
    let mut cert_reader = entry.cert_pem.as_slice(); // 以证书 PEM 字节构造读取器
    let certs: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut cert_reader) // 解析证书链
        .collect::<Result<_, _>>() // 收集为结果
        .map_err(|e| TlsError::Pem(e.to_string()))?; // 解析失败转错误
    if certs.is_empty() { // 没有证书
        return Err(TlsError::Missing(entry.domain.clone())); // 返回缺失错误
    }

    let mut key_reader = entry.key_pem.as_slice(); // 以私钥 PEM 字节构造读取器
    let key: PrivateKeyDer<'static> = rustls_pemfile::private_key(&mut key_reader) // 解析私钥
        .map_err(|e| TlsError::Pem(e.to_string()))? // 解析失败转错误
        .ok_or_else(|| TlsError::Missing(entry.domain.clone()))?; // 无私钥转缺失错误

    let provider = rustls::crypto::ring::default_provider(); // 取 ring crypto provider
    let signing_key = provider // 用 provider 加载签名私钥
        .key_provider // 私钥提供者
        .load_private_key(key) // 由私钥 DER 构造签名密钥
        .map_err(|e| TlsError::Crypto(e.to_string()))?; // 失败转加密错误

    let not_after = extract_not_after(certs[0].as_ref()); // 从叶子证书取到期时间
    Ok((Arc::new(CertifiedKey::new(certs, signing_key)), not_after)) // 组合并返回
}

/// 从叶子证书 DER 取 notAfter（Unix 秒）；解析失败返回 None（不影响握手）
fn extract_not_after(der: &[u8]) -> Option<i64> { // 解析证书有效期
    match x509_parser::parse_x509_certificate(der) { // 解析 X.509 证书
        Ok((_, cert)) => Some(cert.validity().not_after.timestamp()), // 取 notAfter 时间戳
        Err(e) => { // 解析失败
            tracing::debug!(error = %e, "parse x509 for expiry failed"); // 调试日志（不阻断）
            None // 返回 None
        }
    }
}

/// 动态 SNI resolver：每次握手读取当前仓库（证书热替换后立即生效，无需重启）
#[derive(Debug)] // 派生 Debug（CertStore 已手写 Debug）
pub struct DynamicResolver { // 动态解析器
    store: Arc<ArcSwap<CertStore>>, // 证书仓库句柄
}

impl DynamicResolver { // 为解析器实现构造
    pub fn new(store: Arc<ArcSwap<CertStore>>) -> Self { // 由仓库句柄构造
        Self { store } // 保存句柄
    }
}

impl ResolvesServerCert for DynamicResolver { // 实现 rustls 的 SNI 解析契约
    fn resolve(&self, hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> { // 按 SNI 选证书
        let name = hello.server_name()?; // 取 SNI；无 SNI 则返回 None（握手失败）
        let name = normalize_domain(name); // 归一化域名
        let found = self.store.load().lookup(&name); // 读当前仓库并查找
        if found.is_none() { // 未命中
            tracing::warn!(sni = %name, "no tls certificate for SNI, handshake will fail"); // 告警（不泄露内容）
        }
        found // 返回证书（可能为 None）
    }
}

#[cfg(test)] // 仅在测试时编译
mod tests { // store 单元测试
    use super::*; // 引入被测项

    /// 生成一条自签证书条目
    fn entry(domain: &str) -> CertEntry { // 构造测试用证书条目
        let ck = rcgen::generate_simple_self_signed(vec![domain.to_string()]).unwrap(); // 现场生成自签证书
        CertEntry { // 组装条目
            domain: domain.to_string(), // 域名
            cert_pem: ck.cert.pem().into_bytes(), // 证书 PEM
            key_pem: ck.signing_key.serialize_pem().into_bytes(), // 私钥 PEM
        }
    }

    #[test] // 精确匹配：命中 / 未命中
    fn build_and_lookup_exact() { // 测试精确查找
        let store = CertStore::build(&[entry("api.example.com"), entry("www.example.com")]).unwrap(); // 构建仓库
        assert!(store.lookup("api.example.com").is_some()); // 命中 api
        assert!(store.lookup("www.example.com").is_some()); // 命中 www
        assert!(store.lookup("other.example.com").is_none()); // 未命中
    }

    #[test] // 域名归一化：大小写不敏感
    fn build_normalizes_case() { // 测试归一化
        let store = CertStore::build(&[entry("API.Example.COM")]).unwrap(); // 构建仓库（大写域名）
        assert!(store.lookup("api.example.com").is_some()); // 小写查询应命中
    }

    #[test] // 到期时间被记录
    fn build_records_not_after() { // 测试到期记录
        let store = CertStore::build(&[entry("api.example.com")]).unwrap(); // 构建仓库
        assert!(store.not_after().get("api.example.com").copied().unwrap_or(0) > 0); // 应记录正的到期时间戳
    }

    #[test] // 坏 PEM 整体失败
    fn build_rejects_bad_pem() { // 测试非法 PEM
        let bad = CertEntry { // 构造非法条目
            domain: "x.example.com".to_string(), // 域名
            cert_pem: b"not a pem".to_vec(), // 非法证书
            key_pem: b"nope".to_vec(), // 非法私钥
        };
        assert!(CertStore::build(&[bad]).is_err()); // 应返回错误
    }

    #[test] // 通配：命中同层子域，不命中裸域与跨级
    fn wildcard_matches_single_label() { // 测试通配匹配
        let store = CertStore::build(&[entry("*.example.com")]).unwrap(); // 构建仓库
        assert!(store.lookup("a.example.com").is_some()); // 命中一层子域
        assert!(store.lookup("b.example.com").is_some()); // 命中另一个一层子域
        assert!(store.lookup("example.com").is_none()); // 不命中裸域
        assert!(store.lookup("a.b.example.com").is_none()); // 不命中跨级子域
    }

    #[test] // 精确优先：同时存在精确与通配时取精确那份
    fn exact_takes_priority_over_wildcard() { // 测试精确优先
        let store = CertStore::build(&[entry("*.example.com"), entry("a.example.com")]).unwrap(); // 构建仓库
        assert!(store.lookup("a.example.com").is_some()); // 命中精确条目
        assert!(store.lookup("b.example.com").is_some()); // 其余子域走通配
    }

    #[test] // 非法通配模式被跳过，同批合法条目仍生效
    fn invalid_wildcard_is_skipped() { // 测试跳过非法通配
        let store = CertStore::build(&[ // 同批含合法与非法条目
            entry("api.example.com"), // 合法精确
            entry("*.example.com"), // 合法通配
            entry("foo*.example.com"), // 非法（* 非独占 label）
            entry("a.*.example.com"), // 非法（* 不在最左）
            entry("*.*.example.com"), // 非法（多个 *）
            entry("*.com"), // 非法（顶级后缀）
        ]).unwrap(); // 构建应成功
        assert!(store.lookup("api.example.com").is_some()); // 合法精确仍生效
        assert!(store.lookup("x.example.com").is_some()); // 合法通配仍生效
    }

    #[test] // 二级后缀通配合法（含点，不误伤）
    fn wildcard_supports_second_level_suffix() { // 测试二级后缀通配
        let store = CertStore::build(&[entry("*.co.uk")]).unwrap(); // 构建仓库
        assert!(store.lookup("x.co.uk").is_some()); // 命中
        assert!(store.lookup("a.b.co.uk").is_none()); // 不跨级
    }

    #[test] // 空标签 SNI 不误命中（单 label / 纯顶级后缀）
    fn lookup_single_label_never_matches() { // 测试空后缀保护
        let store = CertStore::build(&[entry("*.example.com")]).unwrap(); // 构建仓库
        assert!(store.lookup("localhost").is_none()); // 单 label 不应命中
        assert!(store.lookup("com").is_none()); // 顶级后缀不应命中
    }

    #[test] // 通配模式的大小写也被归一化后匹配
    fn wildcard_normalized_case() { // 测试通配归一化
        let store = CertStore::build(&[entry("*.Example.COM")]).unwrap(); // 构建仓库（大写模式）
        assert!(store.lookup("a.example.com").is_some()); // 小写查询应命中
    }
}
