//! **可选的**「按环境选文件」约定。
//!
//! 框架不强制这套约定——[`ConfigSource`] 才是唯一的加载入口，
//! 读哪些文件完全由应用层决定。本模块只是把团队常用的 4 环境布局封装成几个函数，
//! 想用就用，不想用就自己拼 `ConfigSource`。
//!
//! # 约定
//!
//! - 环境变量名**由调用方给**（如 `AURORA_ENV`），取值 ∈
//!   [`Environment::ALL`]：`development` / `test` / `staging` / `production`，不接受别名；
//! - 目录下 `{env}.{toml|yaml|yml|json}`，按 `.toml` > `.yaml`/`.yml` > `.json` 取第一个；
//! - 目录**不存在** → 零文件（容器里配置全来自环境变量，合法）；
//! - 目录**存在但缺文件** → 报错（静默用代码默认值启动，在生产比启动失败更危险）；
//! - 环境变量未设置 → 按 `production` 处理（fail-closed）并告警。
//!
//! # 用法
//!
//! ```no_run
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! use aurora::config::preset;
//!
//! // 两行拿到配置：环境从 AURORA_ENV 读，文件取 config/{env}.toml
//! let config = preset::from_env_var("AURORA_ENV", "config")?.load()?;
//! # let _ = config;
//! # Ok(())
//! # }
//! ```
//!
//! 想换布局（比如一份文件、或者 10 份）就别用本模块，直接
//! `ConfigSource::new(env).file(...).file(...)`。

use std::path::Path;

use crate::boot::startup::StartupError;
use crate::config::{ConfigSource, Environment, loader};

/// 从环境变量读取运行环境。
///
/// 未设置（或为空）时返回 [`Environment::Production`]：宁可让生产护栏误伤本地开发
/// （错误信息会直接说明缺什么），也不能让一次"忘了设变量"的生产部署静默跑在无护栏状态下。
///
/// # Errors
/// 变量有值但不属于 4 个合法取值时返回错误（拒绝启动，不做兜底猜测）。
pub fn env_from_var(var: &str) -> Result<Environment, String> {
    let raw = std::env::var(var).ok();
    match raw.as_deref().map(str::trim).filter(|v| !v.is_empty()) {
        Some(value) => {
            Environment::parse(value).map_err(|error| format!("环境变量 {var}：{error}"))
        }
        None => {
            tracing::warn!(
                var,
                env = %Environment::Production,
                "未设置 {var}，按 production 处理（fail-closed）：生产护栏全部生效。\
                 本地开发请显式设为 development"
            );
            Ok(Environment::Production)
        }
    }
}

/// 按约定在 `dir` 下找与 `env` 同名的配置文件，构造 [`ConfigSource`]。
///
/// # Errors
/// 目录存在但缺少 `{env}.*` 时返回错误；目录存在且对「其他用户」可写时，
/// 生产环境返回错误、其它环境告警。
pub fn env_files(dir: impl AsRef<Path>, env: Environment) -> Result<ConfigSource, StartupError> {
    let dir = dir.as_ref();
    if !dir.exists() {
        tracing::debug!(
            dir = %dir.display(),
            "配置目录不存在：仅使用环境变量与代码默认值"
        );
        return Ok(ConfigSource::new(env));
    }
    check_dir_permissions(dir, env)?;
    if !dir.is_dir() {
        // `check_dir_permissions` 已告警
        return Ok(ConfigSource::new(env));
    }

    let Some((path, _format)) = loader::find_file(dir, env.as_str()) else {
        let expected = dir.join(format!("{}.toml", env.as_str()));
        return Err(StartupError::Config(format!(
            "缺少环境配置文件：{}（环境 = {env}）。配置目录存在时必须提供与当前环境同名的\
             配置文件，可选环境只有 development / test / staging / production ——\
             例如 {}。若本服务只靠环境变量配置，请不要创建配置目录",
            expected.display(),
            expected.display(),
        )));
    };
    Ok(ConfigSource::new(env).file(path))
}

/// 环境变量 + 目录约定的组合：`from_env_var("AURORA_ENV", "config")`。
///
/// # Errors
/// 环境变量非法或目录里缺对应文件时返回错误。
pub fn from_env_var(var: &str, dir: impl AsRef<Path>) -> Result<ConfigSource, StartupError> {
    let env = env_from_var(var).map_err(StartupError::Config)?;
    env_files(dir, env)
}

/// 一次性完成「读环境变量 → 选文件 → 加载 + 校验」。
///
/// 不需要热更新时用这个最省事；需要热更新就保留 [`from_env_var`] 返回的
/// `ConfigSource`，把它同时交给 `AppState::build_with_source` / `ConfigWatcher`。
///
/// # Errors
/// 环境变量非法、目录里缺对应文件、或校验不通过时返回错误。
pub fn load_env_var(
    var: &str,
    dir: impl AsRef<Path>,
) -> Result<crate::config::AppConfig, StartupError> {
    from_env_var(var, dir)?.load()
}

/// 加载 `.env`（**生产环境跳过**）。
///
/// `.env` 会从当前目录及其父目录逐级查找，内容直接进进程环境变量——而环境变量是
/// 优先级最高的配置源。生产若工作目录可被非特权用户写入，等于把「改配置」（含关 TLS、
/// 换 JWT 密钥）的权限交给了对方，因此只在非生产加载。
///
/// 必须在 [`ConfigSource::load`](crate::config::ConfigSource::load) **之前**调用，
/// 否则读到的环境变量不会被叠加层看到。
pub fn dotenv(env: Environment) {
    if env.is_production() {
        tracing::debug!("production 环境跳过 .env 加载");
        return;
    }
    let _ = dotenvy::dotenv();
}

/// 检查配置目录权限：对「其他用户」可写时，生产拒绝启动、其它环境告警。
///
/// 目录若存在且可被任何本机用户写入，等于任何人都能改配置（含关 TLS、换密钥）。
#[cfg_attr(not(unix), allow(unused_variables))]
fn check_dir_permissions(dir: &Path, env: Environment) -> Result<(), StartupError> {
    let Ok(meta) = std::fs::metadata(dir) else {
        return Ok(());
    };
    if !meta.is_dir() {
        tracing::warn!(
            dir = %dir.display(),
            "配置目录路径不是目录，将忽略并回退到代码默认值"
        );
        return Ok(());
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = meta.permissions().mode();
        if mode & 0o002 != 0 {
            let message = format!(
                "配置目录 {} 对「其他用户」可写（mode {mode:o}）：任何本机用户都能改配置",
                dir.display()
            );
            if env.is_production() {
                return Err(StartupError::Config(message));
            }
            tracing::warn!("{message}");
        } else if mode & 0o020 != 0 {
            tracing::warn!(
                dir = %dir.display(),
                mode = format!("{mode:o}"),
                "配置目录对「同组用户」可写，请确认组成员可信"
            );
        }
    }
    #[cfg(not(unix))]
    {
        // Windows 的 ACL 没有可移植的读取接口，这里只留痕
        tracing::debug!(dir = %dir.display(), "非 Unix 平台跳过配置目录权限检查");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_parse_all_four_env_names() {
        for env in Environment::ALL {
            assert_eq!(Environment::parse(env.as_str()), Ok(env));
        }
    }

    #[test]
    fn should_reject_alias_and_traversal() {
        for raw in ["prod", "dev", "uat", "ci", "default", "../production"] {
            assert!(
                Environment::parse(raw).is_err(),
                "{raw:?} 必须被拒绝（封闭集合）"
            );
        }
    }

    /// 目录不存在 → 零文件（容器场景）。
    #[test]
    fn should_allow_absent_dir() {
        let dir = std::env::temp_dir().join(format!("aurora-preset-absent-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let source = env_files(&dir, Environment::Development).expect("目录不存在应合法");
        assert!(source.file_paths().is_empty());
        assert_eq!(source.env(), Environment::Development);
    }

    /// 目录存在但缺文件 → 报错（不静默回退）。
    #[test]
    fn should_reject_missing_env_file() {
        let dir = std::env::temp_dir().join(format!("aurora-preset-miss-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("development.toml"), "[server]\nport = 7000\n").unwrap();

        let error = env_files(&dir, Environment::Staging).expect_err("缺 staging.toml 必须报错");
        assert!(
            error.to_string().contains("缺少环境配置文件"),
            "实际：{error}"
        );

        // 有的环境能正常取到
        let source = env_files(&dir, Environment::Development).unwrap();
        assert_eq!(source.file_paths().len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
