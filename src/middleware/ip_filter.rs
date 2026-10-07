//! IP 过滤：IPv4/IPv6 CIDR 黑白名单（`allow` 非空时仅放行列表内来源；
//! `deny` 命中一律拒绝）。key 来源与限流一致（`[server].ip_key_mode`）。
//!
//! 解析纪律：**规则解析失败必须 fail-fast**（`from_rules` 返回 `Result`，
//! `CoreState::from_settings` 启动期校验）——静默丢弃 + 空 allow 短路为全放行
//! 组合会变成 fail-open（只配 IPv6 白名单时公网 IP 全部放行）。
//! IPv4-mapped IPv6（`::ffff:a.b.c.d`，双栈下常见 peer 形态）匹配前归一化为 IPv4。
//!
//! 依赖锚点：经请求 extension 读取 `CoreState`（`App::serve` 挂在最外层；
//! 裸模式自组装时同样由框架必需件保证存在，缺扩展时 500 fail-closed）。

use std::net::IpAddr; // 引入通用 IP 地址类型（V4/V6 统一表示）

use axum::extract::{Extension, Request}; // 引入扩展提取器与请求体类型
use axum::http::StatusCode; // 引入 HTTP 状态码
use axum::middleware::Next; // 引入 Next，用于把请求交给下游中间件
use axum::response::{IntoResponse, Response}; // 引入响应转换 trait 与响应类型

use crate::state::CoreState; // 引入框架核心状态（依赖锚点）
use crate::traits::HasConfig; // 引入状态能力 trait：取配置

/// 解析后的过滤规则
#[derive(Debug, Clone, Default)] // 派生调试/克隆/默认，Default 提供空名单
pub struct IpFilter { // IP 过滤器：持有解析后的白名单与黑名单
    allow: Vec<Cidr>, // 白名单（非空时仅放行其中来源）
    deny: Vec<Cidr>, // 黑名单（命中即拒绝，优先级高于白名单）
}

#[derive(Debug, Clone)] // 派生调试与克隆
enum Cidr { // 单条过滤规则：V4/V6 网段或精确 IP
    V4 { addr: std::net::Ipv4Addr, prefix: u8 }, // IPv4 网段（地址 + 前缀长度）
    V6 { addr: std::net::Ipv6Addr, prefix: u8 }, // IPv6 网段（地址 + 前缀长度）
    Exact(IpAddr), // 精确匹配单个 IP
}

impl Cidr { // 为单条规则实现解析与匹配
    fn parse(s: &str) -> Result<Self, String> { // 解析一条规则字符串，失败返回错误说明
        let s = s.trim(); // 去除首尾空白
        if let Some((addr, prefix)) = s.split_once('/') { // 含 '/' 视为 CIDR 网段
            let ip: IpAddr = addr // 解析斜杠前的地址部分
                .parse() // 解析为 IpAddr
                .map_err(|_| format!("invalid address in CIDR {s:?}"))?; // 解析失败即报错返回
            let prefix: u8 = prefix // 解析斜杠后的前缀长度
                .parse() // 解析为 u8
                .map_err(|_| format!("invalid prefix in CIDR {s:?}"))?; // 解析失败即报错返回
            match ip { // 依据地址族与前缀范围构造对应变体
                IpAddr::V4(a) if prefix <= 32 => Ok(Self::V4 { addr: a, prefix }), // IPv4 且前缀合法
                IpAddr::V6(a) if prefix <= 128 => Ok(Self::V6 { addr: a, prefix }), // IPv6 且前缀合法
                IpAddr::V4(_) => Err(format!("IPv4 prefix out of range in {s:?} (0-32)")), // IPv4 前缀越界
                IpAddr::V6(_) => Err(format!("IPv6 prefix out of range in {s:?} (0-128)")), // IPv6 前缀越界
            }
        } else { // 不含 '/' 视为精确 IP
            s.parse::<IpAddr>() // 解析为 IpAddr
                .map(Self::Exact) // 成功则包成 Exact 变体
                .map_err(|_| format!("invalid IP address {s:?}")) // 失败返回错误说明
        }
    }

    fn matches(&self, ip: &IpAddr) -> bool { // 判断给定 IP 是否命中本规则
        match self { // 按规则类型分派
            Self::Exact(x) => x == ip, // 精确匹配：地址相等即命中
            Self::V4 { addr, prefix } => match ip { // IPv4 网段规则
                IpAddr::V4(other) => { // 待测也是 IPv4
                    let a = u32::from(*addr); // 规则地址转 u32
                    let b = u32::from(*other); // 待测地址转 u32
                    let mask = v4_mask(*prefix); // 由前缀长度生成掩码
                    a & mask == b & mask // 掩码后相等即同网段
                }
                // IPv4 规则不匹配纯 IPv6 地址（mapped 形态已在入口归一化）
                IpAddr::V6(_) => false, // 地址族不同，直接不命中
            },
            Self::V6 { addr, prefix } => match ip { // IPv6 网段规则
                IpAddr::V6(other) => { // 待测也是 IPv6
                    let a = u128::from(*addr); // 规则地址转 u128
                    let b = u128::from(*other); // 待测地址转 u128
                    let mask = v6_mask(*prefix); // 由前缀长度生成掩码
                    a & mask == b & mask // 掩码后相等即同网段
                }
                IpAddr::V4(_) => false, // 地址族不同，直接不命中
            },
        }
    }
}

fn v4_mask(prefix: u8) -> u32 { // 生成 IPv4 前缀掩码
    if prefix == 0 { // 前缀为 0
        0 // 掩码全 0（匹配所有地址）
    } else { // 前缀非 0
        u32::MAX << (32 - prefix) // 高位连续 1 的掩码
    }
}

fn v6_mask(prefix: u8) -> u128 { // 生成 IPv6 前缀掩码
    if prefix == 0 { // 前缀为 0
        0 // 掩码全 0（匹配所有地址）
    } else { // 前缀非 0
        u128::MAX << (128 - prefix) // 高位连续 1 的掩码
    }
}

/// `::ffff:a.b.c.d`（IPv4-mapped IPv6）归一化为 IPv4，使双栈 peer 与
/// IPv4 规则可比；其余地址原样返回。
fn normalize(ip: IpAddr) -> IpAddr { // 把 IPv4-mapped IPv6 归一化为 IPv4
    match ip { // 按地址类型分派
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(IpAddr::V6(v6)), // mapped 则转 IPv4，否则保持 IPv6
        v4 => v4, // IPv4 原样返回
    }
}

impl IpFilter { // 为过滤器实现构造与判定
    /// 解析失败返回 Err（含出错规则），调用方应 fail-fast，**绝不静默丢弃**
    pub fn from_rules(allow: &[String], deny: &[String]) -> Result<Self, String> { // 从字符串规则解析出过滤器
        let parse_all = |rules: &[String], what: &str| -> Result<Vec<Cidr>, String> { // 闭包：批量解析并附加来源标签
            rules // 遍历规则数组
                .iter() // 取得迭代器
                .map(|s| Cidr::parse(s).map_err(|e| format!("{what}: {e}"))) // 逐条解析，失败时前缀来源名
                .collect() // 收集为 Result<Vec<Cidr>, _>
        };
        Ok(Self { // 组装过滤器
            allow: parse_all(allow, "allow")?, // 解析白名单，失败即传播
            deny: parse_all(deny, "deny")?, // 解析黑名单，失败即传播
        })
    }

    pub fn allows(&self, ip: &IpAddr) -> bool { // 判断某 IP 是否放行
        let ip = normalize(*ip); // 先归一化地址形态
        if self.deny.iter().any(|c| c.matches(&ip)) { // 命中任一黑名单
            return false; // 黑名单优先，拒绝
        }
        self.allow.is_empty() || self.allow.iter().any(|c| c.matches(&ip)) // 白名单为空则全放行，否则须命中白名单
    }
}

pub(crate) async fn handle(Extension(core): Extension<CoreState>, req: Request, next: Next) -> Response { // IP 过滤中间件入口
    let server = &core.config().load().server; // 读取服务器配置片段
    if !server.ip_filter.enabled { // IP 过滤未启用
        return next.run(req).await; // 直接放行到下游
    }
    // 启动期已校验过（state.rs），此处 Err 属防御分支：fail-closed 500
    let filter = match IpFilter::from_rules(&server.ip_filter.allow, &server.ip_filter.deny) { // 运行时重新解析规则
        Ok(f) => f, // 解析成功
        Err(e) => { // 解析失败（防御分支）
            tracing::error!(error = %e, "ip_filter rule re-parse failed at request time"); // 记录错误日志
            return ( // 返回 500，宁可失败也不放行
                StatusCode::INTERNAL_SERVER_ERROR, // 状态码 500
                axum::Json(crate::web::response::ApiResponse::error( // 使用统一 JSON 错误体
                    500, // 业务错误码 500
                    "internal server error", // 错误信息
                )),
            )
                .into_response(); // 转为 Response
        }
    };
    let peer = req // 从连接信息取对端地址
        .extensions() // 访问请求扩展
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>() // 取 axum 注入的连接信息
        .map(|c| c.0.ip()); // 映射为对端 IP
    let mode = crate::utils::client_ip::IpKeyMode::parse(&server.ip_key_mode) // 解析客户端 IP 取值模式
        .unwrap_or(crate::utils::client_ip::IpKeyMode::PeerIp); // 解析失败退回直连对端模式
    let ip = match mode { // 按模式确定用于过滤的 IP
        crate::utils::client_ip::IpKeyMode::PeerIp => peer, // 直连模式：用对端 IP
        crate::utils::client_ip::IpKeyMode::ProxyHeaders => { // 代理头模式
            crate::utils::client_ip::resolve(req.headers(), peer) // 从代理头解析真实客户端 IP
        }
    };
    match ip { // 依据解析出的 IP 决定放行或拒绝
        Some(ip) if filter.allows(&ip) => next.run(req).await, // 有 IP 且被允许：放行
        Some(_) => ( // 有 IP 但被拒绝
            StatusCode::FORBIDDEN, // 状态码 403
            axum::Json(crate::web::response::ApiResponse::error(403, "forbidden")), // 统一 JSON 错误体
        )
            .into_response(), // 转为 Response
        // 解析不出来源 IP：配置了白名单时 fail-closed（不能把"看不见的 IP"当放行）
        None => { // 无来源 IP
            if !server.ip_filter.allow.is_empty() { // 配置了白名单
                ( // 无法确认来源即拒绝
                    StatusCode::FORBIDDEN, // 状态码 403
                    axum::Json(crate::web::response::ApiResponse::error(403, "forbidden")), // 统一 JSON 错误体
                )
                    .into_response() // 转为 Response
            } else { // 未配白名单
                next.run(req).await // 无白名单时放行
            }
        }
    }
}

/// 自组装用层（裸模式）：IP 黑白名单过滤。生效与否由 `[server.ip_filter] enabled`
/// 配置决定（支持热更新）。
pub fn layer() -> super::BoxedLayer { // 返回装箱的 IP 过滤层
    super::BoxedLayer::new(axum::middleware::from_fn(handle)) // 装箱屏蔽具体层类型
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cidr_matches() {
        let f = IpFilter::from_rules(&["10.0.0.0/8".into(), "192.168.1.7".into()], &[]).unwrap();
        assert!(f.allows(&"10.1.2.3".parse().unwrap()));
        assert!(f.allows(&"192.168.1.7".parse().unwrap()));
        assert!(!f.allows(&"192.168.1.8".parse().unwrap()));
        assert!(!f.allows(&"11.0.0.1".parse().unwrap()));
        assert!(IpFilter::from_rules(&[], &[]).unwrap().allows(&"1.2.3.4".parse().unwrap()));

        let d = IpFilter::from_rules(&[], &["10.0.0.0/8".into()]).unwrap();
        assert!(!d.allows(&"10.1.2.3".parse().unwrap()));
    }

    #[test]
    fn ipv6_cidr_parses_and_matches() {
        let f = IpFilter::from_rules(&["2001:db8::/32".into(), "10.0.0.0/8".into()], &[]).unwrap();
        assert_eq!(f.allow.len(), 2, "IPv6 CIDR 不得被静默丢弃");
        assert!(f.allows(&"2001:db8:1234::1".parse().unwrap()));
        assert!(!f.allows(&"2001:db9::1".parse().unwrap()));
        // ::ffff:10.1.2.3 是 mapped IPv4，应命中 10.0.0.0/8
        assert!(f.allows(&"::ffff:10.1.2.3".parse().unwrap()));
        assert!(!f.allows(&"::ffff:11.0.0.1".parse().unwrap()));
    }

    #[test]
    fn invalid_rule_is_an_error_not_silently_dropped() {
        assert!(IpFilter::from_rules(&["not-an-ip".into()], &[]).is_err());
        assert!(IpFilter::from_rules(&["2001:db8::/129".into()], &[]).is_err());
        assert!(IpFilter::from_rules(&[], &["10.0.0.0/33".into()]).is_err());
    }

    #[test]
    fn only_ipv6_allowlist_blocks_public_ipv4() {
        // P0-2 回归：allow 只配 IPv6 时，IPv4 公网地址不得放行
        let f = IpFilter::from_rules(&["2001:db8::/32".into()], &[]).unwrap();
        assert!(!f.allows(&"8.8.8.8".parse().unwrap()));
    }
}
