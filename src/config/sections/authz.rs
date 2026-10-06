//! `[authz]` 配置节：Casbin 模型路径、策略来源（file/db）、自动加载/热更新（文档 三·14）。

use serde::{Deserialize, Serialize};

fn default_source() -> String {
    "file".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthzSettings {
    #[serde(default)]
    pub enabled: bool,
    /// Casbin 模型文件（.conf），RBAC / RBAC with domains 由内容决定
    #[serde(default)]
    pub model_path: String,
    /// 策略来源：file（开发）| db（生产，经 SeaORM 读 casbin_rule 表）
    #[serde(default = "default_source")]
    pub source: String,
    /// file 来源的策略文件路径
    #[serde(default)]
    pub file_path: String,
    /// 是否自动加载（bootstrap 时装配；false 时应用自行延迟装配）
    #[serde(default = "default_true_authz")]
    pub auto_load: bool,
    /// 策略热更新（挂到 config watcher / 定时重载）
    #[serde(default)]
    pub auto_reload: bool,
}

fn default_true_authz() -> bool {
    true
}

impl Default for AuthzSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            model_path: String::new(),
            source: default_source(),
            file_path: String::new(),
            auto_load: default_true_authz(),
            auto_reload: false,
        }
    }
}
