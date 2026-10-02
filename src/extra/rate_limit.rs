//! 按 IP 令牌桶限流（feature = "rate-limit"，tower-governor）。
//! 由 `web::middleware::apply` 根据 `[server.rate_limit]` 配置自动挂载。

use axum::Router;
use tower_governor::key_extractor::SmartIpKeyExtractor;

use crate::config::RateLimitConfig;

pub(crate) fn apply(app: Router, cfg: &RateLimitConfig) -> Router {
    if !cfg.enabled {
        return app;
    }

    let proxy_headers = match cfg.key.as_str() {
        "" | "peer_ip" => false,
        "proxy_headers" => true,
        other => {
            tracing::warn!(key = other, "unknown rate_limit key, falling back to peer_ip");
            false
        }
    };

    tracing::info!(
        per_second = cfg.per_second,
        burst = cfg.burst,
        key = if proxy_headers { "proxy_headers" } else { "peer_ip" },
        "rate limit enabled"
    );

    // 两个分支的 keyer 类型不同，各自构建与挂载
    if proxy_headers {
        // 反代部署：从 X-Forwarded-For / X-Real-IP / Forwarded 取真实 IP，
        // 逐级回退到 ConnectInfo / peer 地址
        let mut builder = tower_governor::governor::GovernorConfigBuilder::default()
            .key_extractor(SmartIpKeyExtractor);
        builder.per_second(cfg.per_second).burst_size(cfg.burst);
        let config = builder.finish().expect("rate limit config invalid");
        app.layer(tower_governor::GovernorLayer::new(config))
    } else {
        // 直连部署：按对端 IP
        let mut builder = tower_governor::governor::GovernorConfigBuilder::default();
        builder.per_second(cfg.per_second).burst_size(cfg.burst);
        let config = builder.finish().expect("rate limit config invalid");
        app.layer(tower_governor::GovernorLayer::new(config))
    }
}
