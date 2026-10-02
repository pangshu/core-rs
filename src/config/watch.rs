use serde::{Deserialize, Serialize};

fn default_enabled() -> bool {
    true
}

/// `[watch]` 配置段：配置热更新（feature = "watch"，默认开启）。
/// 开启后框架监听 `app.yml`（及 profile 覆盖文件）变更，自动重载并原子切换；
/// 变更细节见 [`crate::config::hot_reload`]。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WatchConfig {
    /// 是否启用配置文件监听
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

impl Default for WatchConfig {
    fn default() -> Self {
        Self {
            enabled: default_enabled(),
        }
    }
}
