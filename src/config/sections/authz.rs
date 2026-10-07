//! `[authz]` 配置节：Casbin 模型路径、策略来源（file/db）、自动加载/热更新（文档 三·14）。

use serde::{Deserialize, Serialize}; // 引入 serde 反序列化/序列化派生宏

fn default_source() -> String { // 策略来源的默认值函数
    "file".to_string() // 默认从文件加载策略
}

#[derive(Debug, Clone, Serialize, Deserialize)] // 派生调试/克隆与 serde 能力
pub struct AuthzSettings { // 授权（Casbin）配置结构
    #[serde(default)] // 缺失时用默认值
    pub enabled: bool, // 是否启用授权
    /// Casbin 模型文件（.conf），RBAC / RBAC with domains 由内容决定
    #[serde(default)] // 缺失时用默认值
    pub model_path: String, // Casbin 模型文件路径
    /// 策略来源：file（开发）| db（生产，经 SeaORM 读 casbin_rule 表）
    #[serde(default = "default_source")] // 缺失时用 default_source 兜底
    pub source: String, // 策略来源类型
    /// file 来源的策略文件路径
    #[serde(default)] // 缺失时用默认值
    pub file_path: String, // 策略文件路径
    /// 是否自动加载（bootstrap 时装配；false 时应用自行延迟装配）
    #[serde(default = "default_true_authz")] // 缺失时默认开启自动加载
    pub auto_load: bool, // 是否启动时自动装配授权器
    /// 策略热更新（挂到 config watcher / 定时重载）
    #[serde(default)] // 缺失时用默认值
    pub auto_reload: bool, // 是否启用策略热更新
}

fn default_true_authz() -> bool { // 自动加载的默认值函数
    true // 默认开启
}

impl Default for AuthzSettings { // 为授权配置实现 Default
    fn default() -> Self { // 返回默认配置
        Self { // 构造默认配置
            enabled: false, // 默认不启用授权
            model_path: String::new(), // 默认无模型路径
            source: default_source(), // 默认策略来源为 file
            file_path: String::new(), // 默认无策略文件路径
            auto_load: default_true_authz(), // 默认开启自动加载
            auto_reload: false, // 默认不启用热更新
        }
    }
}
