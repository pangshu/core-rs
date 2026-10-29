//! 配置句柄：`ArcSwap` 快照 + 原子替换（热更新的落点）。

use std::sync::Arc;

use arc_swap::ArcSwap;
use ipnet::IpNet;

use crate::config::AppConfig;

/// 运行期**不得放宽**的安全边界。
///
/// ## 为什么单独拎出来
///
/// 这些字段不是"业务参数"，而是"信任判定"：它们决定谁是可信代理、谁能抓指标。
/// 而中间件是**每请求读活配置**的，于是任何一条配置变更（本地文件 watcher 或
/// 配置中心）都能在运行期改动它们——攻击者拿到配置写权限后，可以把
/// `trusted_proxies` 改成自己所在的网段，从而伪造 `X-Forwarded-For`，
/// 绕过按 IP 限流、IP 白名单，并污染审计日志与 geoip。
///
/// 因此热更新只允许**收紧**：
/// - `trust_proxy`：只允许 `true → false`；
/// - `trusted_proxies`：新集合必须是旧集合的子集（按 CIDR 字面量比较，不做网段包含推导——
///   行为可预测，避免"看起来更严实际更宽"的推导错误）；
/// - `metrics.token`：不允许从"有令牌"变成"无令牌"（否则 `/metrics` 直接裸奔）。
///
/// 放宽一律**拒绝整次热更新**（不是只忽略该字段）：半新半旧的快照比旧快照更难推理。
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct SecurityBoundary {
    /// 是否采信 `X-Forwarded-*`。
    pub trust_proxy: bool,
    /// 可信反向代理网段。
    pub trusted_proxies: Vec<IpNet>,
    /// `/metrics` 抓取令牌（空 = 不校验）。
    pub metrics_token: String,
}

impl SecurityBoundary {
    /// 从配置快照提取安全边界。
    #[must_use]
    pub fn from_config(config: &AppConfig) -> Self {
        Self {
            trust_proxy: config.server.trust_proxy,
            trusted_proxies: config.server.trusted_proxies.clone(),
            metrics_token: config.telemetry.metrics.token.clone(),
        }
    }

    /// 校验从 `self`（旧）到 `next`（新）的变更是否**只收紧不放宽**。
    ///
    /// # Errors
    /// 任一项放宽时返回人类可读的原因（调用方会拒绝整次热更新）。
    pub fn check_transition(&self, next: &Self) -> Result<(), String> {
        if !self.trust_proxy && next.trust_proxy {
            return Err(
                "[server].trust_proxy 从 false 变为 true：会开始采信 X-Forwarded-For，\
                 必须重启生效（运行期不得放宽信任边界）"
                    .to_string(),
            );
        }

        let widened: Vec<String> = next
            .trusted_proxies
            .iter()
            .filter(|net| !self.trusted_proxies.contains(net))
            .map(ToString::to_string)
            .collect();
        if !widened.is_empty() {
            return Err(format!(
                "[server].trusted_proxies 新增了 {widened:?}：会扩大可信代理范围，\
                 必须重启生效（运行期只允许删除，不允许新增）"
            ));
        }

        if !self.metrics_token.trim().is_empty() && next.metrics_token.trim().is_empty() {
            return Err(
                "[telemetry].metrics.token 被清空：/metrics 将失去鉴权，必须重启生效".to_string(),
            );
        }
        Ok(())
    }
}

/// 配置句柄：`ArcSwap` 快照 + 原子替换（热更新的落点）。
///
/// 之后按普通字段访问（`config.server.port`）。
///
/// 热更新语义（`docs/08` §12.1）：只有**请求处理时重新 `load()`** 的值会即时生效；
/// 启动期一次性装配的（中间件栈、监听地址、连接池）仍需重启。
/// 安全边界字段（[`SecurityBoundary`]）额外限制为"只可收紧"。
#[derive(Clone)]
pub struct ConfigHandle {
    current: Arc<ArcSwap<AppConfig>>,
    boundary: Arc<ArcSwap<SecurityBoundary>>,
}

impl ConfigHandle {
    /// 由配置构造句柄。
    #[must_use]
    pub fn new(config: AppConfig) -> Self {
        apply_runtime_knobs(&config);
        let boundary = SecurityBoundary::from_config(&config);
        Self {
            current: Arc::new(ArcSwap::from_pointee(config)),
            boundary: Arc::new(ArcSwap::from_pointee(boundary)),
        }
    }

    /// 当前配置快照（原子读，无锁）。
    #[must_use]
    pub fn load(&self) -> Arc<AppConfig> {
        self.current.load_full()
    }

    /// 当前安全边界（原子读）。
    ///
    /// **中间件应当读这里而不是 `config.server.trust_proxy`**：后者的值可能来自
    /// 一次尚未通过边界校验的热更新尝试。
    #[must_use]
    pub fn boundary(&self) -> Arc<SecurityBoundary> {
        self.boundary.load_full()
    }

    /// 原子替换为新快照（由 `config::watch` / `config::nacos` 的 watcher 调用）。
    ///
    /// 替换后若旧快照**已无人引用**，就地擦除其中的密钥（`Arc::into_inner` 只在
    /// 强引用计数为 1 时返回 `Some`，否则放弃——不能为了清零阻塞在读请求上）。
    ///
    /// # Errors
    /// 新配置会**放宽**安全边界（见 [`SecurityBoundary::check_transition`]）时返回原因，
    /// 且**不替换**任何快照——调用方应把它当成"本次热更新失败，需重启"。
    pub fn store(&self, config: AppConfig) -> Result<(), String> {
        let next = SecurityBoundary::from_config(&config);
        let current = self.boundary.load();
        if let Err(reason) = current.check_transition(&next) {
            tracing::error!(%reason, "配置热更新被拒绝：会放宽安全边界，需重启生效");
            return Err(reason);
        }

        apply_runtime_knobs(&config);
        let old = self.current.swap(Arc::new(config));
        self.boundary.store(Arc::new(next));
        if let Some(mut old) = Arc::into_inner(old) {
            old.zeroize_secrets();
        }
        Ok(())
    }
}

/// 把「请求路径上要读」的配置投影到进程级单元格。
///
/// 这类参数由无状态入口（自由函数 / 泛型提取器）消费，拿不到 `AppState`，
/// 只能走进程级单元格；放在这里是为了让**配置热更新**同样生效。
#[cfg(feature = "security")]
fn apply_runtime_knobs(config: &AppConfig) {
    crate::validation::options::set_global(config.validation.to_options());
}

/// 未启用 `security` 时没有需要投影的运行时参数。
#[cfg(not(feature = "security"))]
fn apply_runtime_knobs(_config: &AppConfig) {}

impl std::fmt::Debug for ConfigHandle {
    /// 展开当前配置快照（标准 `Debug` 输出，不做字段级掩码）。
    ///
    /// **注意**：输出含连接串与密钥。项目已明确"移除配置脱敏机制"，
    /// 因此应用侧**不得**直接 `{:?}` 打印 `AppConfig` / `AppState` / `ConfigHandle`；
    /// 启动日志只记录 `app` / `env`（见 `AppState::build`）。
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConfigHandle")
            .field("config", &*self.load())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AppConfig;

    fn config_with(trust_proxy: bool, proxies: &[&str], token: &str) -> AppConfig {
        let mut config = AppConfig::default();
        config.server.trust_proxy = trust_proxy;
        config.server.trusted_proxies = proxies
            .iter()
            .map(|cidr| cidr.parse().expect("合法 CIDR"))
            .collect();
        config.telemetry.metrics.token = token.to_string();
        config
    }

    #[test]
    fn should_swap_snapshot_when_boundary_unchanged() {
        let handle = ConfigHandle::new(config_with(false, &[], ""));
        let mut next = config_with(false, &[], "");
        next.server.port = 9090;

        handle.store(next).expect("未改边界，应允许");
        assert_eq!(handle.load().server.port, 9090);
    }

    #[test]
    fn should_reject_widening_trust_proxy() {
        let handle = ConfigHandle::new(config_with(false, &[], ""));
        let error = handle
            .store(config_with(true, &["10.0.0.0/8"], ""))
            .expect_err("false → true 必须拒绝");
        assert!(error.contains("trust_proxy"), "实际：{error}");
        // 整次热更新被拒绝：旧快照保持
        assert!(!handle.load().server.trust_proxy);
    }

    #[test]
    fn should_allow_narrowing_trust_proxy() {
        let handle = ConfigHandle::new(config_with(true, &["10.0.0.0/8"], ""));
        handle
            .store(config_with(false, &["10.0.0.0/8"], ""))
            .expect("收紧应允许");
        assert!(!handle.load().server.trust_proxy);
    }

    #[test]
    fn should_reject_new_trusted_proxies_but_allow_removal() {
        let handle = ConfigHandle::new(config_with(true, &["10.0.0.0/8", "192.168.0.0/16"], ""));
        // 新增 → 拒绝
        let error = handle
            .store(config_with(
                true,
                &["10.0.0.0/8", "192.168.0.0/16", "172.16.0.0/12"],
                "",
            ))
            .expect_err("新增可信网段必须拒绝");
        assert!(error.contains("trusted_proxies"), "实际：{error}");

        // 删除 → 允许
        handle
            .store(config_with(true, &["10.0.0.0/8"], ""))
            .expect("删除可信网段应允许");
        assert_eq!(handle.load().server.trusted_proxies.len(), 1);
    }

    #[test]
    fn should_reject_clearing_metrics_token() {
        let handle = ConfigHandle::new(config_with(false, &[], "scrape-token"));
        let error = handle
            .store(config_with(false, &[], ""))
            .expect_err("清空令牌必须拒绝");
        assert!(error.contains("metrics.token"), "实际：{error}");

        // 轮换（非空 → 另一个非空）应允许
        handle
            .store(config_with(false, &[], "new-token"))
            .expect("轮换令牌应允许");
    }
}
