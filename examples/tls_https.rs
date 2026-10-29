//! tls_https —— M6 示例：HTTPS + SNI 多域名 + 证书热更新。
//!
//! 运行：`cargo run --example tls_https --features tls`
//!
//! ```bash
//! # 默认证书（localhost / 127.0.0.1）
//! curl -k https://localhost:8443/
//!
//! # SNI：指定域名 + 自签证书（--resolve 把域名指到本机）
//! curl -k --resolve api.local.test:8443:127.0.0.1 https://api.local.test:8443/
//!
//! # 热更新：替换 certs/dev/fullchain.pem 后无需重启（500ms 防抖）
//! ```
//!
//! 首次运行会在 `certs/dev/` 生成**自签证书**（`rcgen`），**仅用于本地开发**；
//! 生产请按 `docs/16` §7 用 ACME / 商业 CA 签发。
//!
//! 证书布局见 `docs/16` §2：`fullchain.pem` + `privkey.pem` 为默认证书，
//! `domains/<域名>.pem` + `.key` 按 SNI 匹配。
//! ⚠️ 通配符证书文件名含 `*`，**Windows 无法创建该文件名**——示例只在
//! Linux/macOS 上生成通配符证书，Windows 下用精确域名即可。

use std::path::Path;

use axum::routing::get;

use aurora::prelude::*;

async fn hello() -> ApiResponse<&'static str> {
    ApiResponse::ok("hello over https")
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut config = preset::load_env_var("AURORA_ENV", "config")?;
    config.server.port = 8443;
    config.tls.enabled = true;
    config.tls.cert_dir = "certs/dev".to_string();
    config.tls.domains = vec!["localhost".to_string(), "api.local.test".to_string()];

    ensure_dev_certs(Path::new(&config.tls.cert_dir))?;

    init_tracing(&config.telemetry)?;
    let state = AppState::build(&config).await?;

    let tls = TlsService::from_config(&config.tls)?;
    tracing::info!(
        certs = tls.store().entries().len(),
        reload = config.tls.reload,
        "TLS 就绪"
    );

    let app = build_router(
        AppRoutes::all_public(Router::new().route("/", get(hello))),
        state,
    );
    serve_tls(app, config.server.socket_addr()?, tls).await?;
    Ok(())
}

/// 本地自签证书（仅开发用）；已存在则跳过。
fn ensure_dev_certs(dir: &Path) -> anyhow::Result<()> {
    if dir.join("fullchain.pem").is_file() && dir.join("privkey.pem").is_file() {
        return Ok(());
    }

    std::fs::create_dir_all(dir.join("domains"))?;
    write_pair(
        &dir.join("fullchain.pem"),
        &dir.join("privkey.pem"),
        &["localhost", "127.0.0.1"],
    )?;
    write_pair(
        &dir.join("domains").join("api.local.test.pem"),
        &dir.join("domains").join("api.local.test.key"),
        &["api.local.test"],
    )?;

    println!(
        "已生成自签证书到 {}（仅开发用；生产见 docs/16 §7）",
        dir.display()
    );
    Ok(())
}

fn write_pair(cert_path: &Path, key_path: &Path, sans: &[&str]) -> anyhow::Result<()> {
    let names = sans
        .iter()
        .map(|name| (*name).to_string())
        .collect::<Vec<_>>();
    let generated = rcgen::generate_simple_self_signed(names)?;
    std::fs::write(cert_path, generated.cert.pem())?;
    std::fs::write(key_path, generated.key_pair.serialize_pem())?;
    // `[tls].strict_key_perms` 默认开启：私钥必须 0600，否则加载期就拒绝启动
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(key_path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}
