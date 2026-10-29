//! `[openapi]` 配置段（feature `openapi`）。
//!
//! 字段与默认值见 `docs/08-config-spec.md` §3 `[openapi]`。

use serde::Deserialize;

/// OpenAPI 文档配置段。
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[non_exhaustive]
pub struct OpenApiSection {
    /// 是否挂载文档（规范 JSON 端点与文档 UI）。
    ///
    /// **默认关闭**：`/docs` + `openapi.json` 会公开全部路由、参数与数据模型，
    /// 等于把攻击面测绘结果送到对手手上。
    pub enabled: bool,
    /// 文档 UI 路径。
    pub path: String,
    /// OpenAPI 规范 JSON 路径。
    pub json_path: String,
    /// 生产环境是否允许挂载（**显式确认**）。
    ///
    /// 只有当文档已置于鉴权之后或仅内网可达时才置 `true`；否则生产环境
    /// 开启 `enabled` 会直接拒绝启动。
    pub allow_in_production: bool,
    /// 文档 UI 的静态资源基址（`swagger-ui.css` / `swagger-ui-bundle.js` 所在目录）。
    ///
    /// 默认是固定版本的 unpkg CDN。**生产建议改为自建静态资源**
    /// （例如 `/docs/assets`，由本服务的 `ServeDir` 提供）：CDN 上的 JS 与本服务
    /// **同源执行**，第三方被投毒等于在你域名下执行任意脚本。
    pub ui_assets_base: String,
    /// 可选的 SRI（`integrity` 属性），配合自建 / 固定版本资源使用。
    ///
    /// 形如 `sha384-...`。必须与 `ui_assets_base` 下的**确切文件**对应，
    /// 否则浏览器会拒绝加载（页面白屏）。
    pub ui_integrity: Option<String>,
}

impl Default for OpenApiSection {
    fn default() -> Self {
        Self {
            enabled: false,
            path: "/docs".to_string(),
            json_path: "/api-docs/openapi.json".to_string(),
            allow_in_production: false,
            // 固定精确版本：`@5` 这种浮动标签会让同一份代码在不同时间加载到不同 JS
            ui_assets_base: "https://unpkg.com/swagger-ui-dist@5.17.14".to_string(),
            ui_integrity: None,
        }
    }
}

impl OpenApiSection {
    /// 启动期校验（按**非生产**环境处理，等价于 `validate_for_env(false)`）。
    ///
    /// # Errors
    /// 路径非法时返回错误。
    pub fn validate(&self) -> Result<(), String> {
        self.validate_for_env(false)
    }

    /// 按运行环境校验（fail-fast）。
    ///
    /// # Errors
    /// 路径不以 `/` 开头，或生产环境开启文档但未显式确认时返回错误。
    pub fn validate_for_env(&self, production: bool) -> Result<(), String> {
        if !self.enabled {
            return Ok(());
        }
        for (key, value) in [("path", &self.path), ("json_path", &self.json_path)] {
            if !value.trim().starts_with('/') {
                return Err(format!("[openapi].{key} = \"{value}\" 必须以 '/' 开头"));
            }
        }
        if let Some(integrity) = &self.ui_integrity {
            let integrity = integrity.trim();
            if !integrity.starts_with("sha256-")
                && !integrity.starts_with("sha384-")
                && !integrity.starts_with("sha512-")
            {
                return Err(format!(
                    "[openapi].ui_integrity = \"{integrity}\" 格式非法（期望 sha256-/sha384-/sha512- 开头）"
                ));
            }
        }
        if !self.ui_assets_base.trim().starts_with('/')
            && !self.ui_assets_base.trim().starts_with("http://")
            && !self.ui_assets_base.trim().starts_with("https://")
        {
            return Err(format!(
                "[openapi].ui_assets_base = \"{}\" 必须是 `/` 开头的站内路径或 http(s) URL",
                self.ui_assets_base
            ));
        }
        if production && self.ui_assets_base.contains("unpkg.com") {
            tracing::warn!(
                base = %self.ui_assets_base,
                "生产环境仍从公网 CDN 加载 Swagger UI：第三方 JS 与本服务**同源执行**，                 建议把 swagger-ui-dist 静态资源自建（[openapi].ui_assets_base = \"/docs/assets\"）                 并配置 [openapi].ui_integrity"
            );
        }
        if production && !self.allow_in_production {
            return Err(
                "[openapi].enabled = true 在生产环境被拒绝：/docs 与 openapi.json 会公开\
                 全部路由与数据模型。请确认文档已置于鉴权之后或仅内网可达，\
                 再设置 [openapi].allow_in_production = true；生产通常应保持 enabled = false"
                    .to_string(),
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_default_to_disabled() {
        let s = OpenApiSection::default();
        assert!(!s.enabled, "默认不挂载文档（避免生产暴露路由清单）");
        assert!(!s.allow_in_production);
        assert_eq!(s.path, "/docs");
        assert_eq!(s.json_path, "/api-docs/openapi.json");
    }

    #[test]
    fn should_parse_overrides() {
        let json = r#"{ "enabled": false, "path": "/api/docs" }"#;
        let s: OpenApiSection = serde_json::from_str(json).unwrap();
        assert!(!s.enabled);
        assert_eq!(s.path, "/api/docs");
        assert_eq!(s.json_path, "/api-docs/openapi.json");
    }

    #[test]
    fn should_require_explicit_opt_in_in_production() {
        let s = OpenApiSection {
            enabled: true,
            ..OpenApiSection::default()
        };
        assert!(s.validate_for_env(false).is_ok(), "非生产直接允许");
        let error = s.validate_for_env(true).unwrap_err();
        assert!(error.contains("allow_in_production"), "实际：{error}");

        let confirmed = OpenApiSection {
            allow_in_production: true,
            ..s
        };
        assert!(confirmed.validate_for_env(true).is_ok());
    }

    #[test]
    fn should_reject_relative_paths() {
        let s = OpenApiSection {
            enabled: true,
            path: "docs".to_string(),
            ..OpenApiSection::default()
        };
        assert!(s.validate().is_err());
    }
}
