//! `[watch]` 配置段（feature `watch`）。

use serde::Deserialize;

/// 配置热更新。
///
/// 只控制「启动时是否自动监听配置目录」——不开启时仍可手动
/// `ConfigWatcher::start_with(...)`。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
#[non_exhaustive]
pub struct WatchSection {
    /// 是否随 `AppState::build_with_source(config, source)` 自动启动热更新。
    ///
    /// 监听哪些文件由 `ConfigSource` 决定；只调 `AppState::build` 时不会启动
    /// （框架不知道配置在哪）。
    ///
    /// 默认关闭：容器里配置常来自环境变量、没有配置目录，
    /// 默认开启会让「目录不存在」的告警每次都出现。
    pub enabled: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_default_to_disabled() {
        // 默认关闭：容器里配置常来自环境变量、没有配置目录，
        // 默认开启会让「目录不存在」的告警每次都出现
        assert!(!WatchSection::default().enabled);
    }
}
