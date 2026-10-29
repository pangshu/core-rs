//! `[app]` 配置段。

use serde::Deserialize;

/// 应用元信息配置段。
///
/// # 这里没有 `env`
///
/// 运行环境**不是配置项**，只能由 `AURORA_ENV` 指定（见
/// [`Environment`](crate::config::Environment)）。把 `env` 放进配置文件会产生两个
/// 来源，而两者冲突时无论以谁为准都存在降级路径——最危险的是
/// `AURORA_ENV=development` 把生产环境拉成开发，一次性打开错误详情、OpenAPI 文档、
/// `/metrics` 与宽松 CORS。
///
/// 配置文件里若还留着 `env = "..."`，会因 `deny_unknown_fields` 在**启动期直接报错**
/// （而不是静默忽略）：静默忽略会让人以为配置生效了。
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[non_exhaustive]
pub struct AppSection {
    /// 应用名。
    pub name: String,
    /// 应用版本，默认取 crate 版本。
    pub version: String,
    /// 显式承认"本次运行使用不安全配置"。
    ///
    /// **这不是开关，是绊线**：生产环境置 `true` 会直接拒绝启动。
    /// 想在生产关闭护栏只能改 `AURORA_ENV`（并因此**同时**失去所有护栏），
    /// 不存在"只关一条"的路径——那种粒度应该走各 section 自己的
    /// `allow_*` 选项（如 `[geoip].allow_degraded`）。
    pub allow_insecure: bool,
}

impl Default for AppSection {
    fn default() -> Self {
        Self {
            name: "aurora".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            allow_insecure: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 配置里写 `env` 必须**报错**，不能静默忽略。
    ///
    /// 静默忽略最坏：运维改了 `env = "production"` 以为切过去了，实际环境由
    /// `AURORA_ENV` 决定，两边的认知从此不一致。
    #[test]
    fn should_reject_env_key_in_config_file() {
        let error = toml_like_parse("[app]\nenv = \"production\"\n")
            .expect_err("配置文件里出现 env 必须报错");
        assert!(
            error.contains("env"),
            "错误信息应指向 env 字段，实际：{error}"
        );
    }

    #[test]
    fn should_accept_app_section_without_env() {
        let section = toml_like_parse("[app]\nname = \"demo\"\nallow_insecure = true\n")
            .expect("不含 env 时应正常解析");
        assert_eq!(section.name, "demo");
        assert!(section.allow_insecure);
    }

    /// 用 `serde_json` 走一遍 `deny_unknown_fields`（避免引入 toml 依赖）。
    fn toml_like_parse(input: &str) -> Result<AppSection, String> {
        // `[app]\nkey = "v"` → {"app":{"key":"v"}} 的极简转换
        let mut map = serde_json::Map::new();
        for line in input.lines().skip(1) {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let (key, value) = line.split_once('=').expect("形如 key = value");
            let key = key.trim();
            let value = value.trim();
            let json = match value {
                "true" => serde_json::Value::Bool(true),
                "false" => serde_json::Value::Bool(false),
                other => serde_json::Value::String(other.trim_matches('"').to_string()),
            };
            map.insert(key.to_string(), json);
        }
        serde_json::from_value(serde_json::Value::Object(map)).map_err(|error| error.to_string())
    }
}
