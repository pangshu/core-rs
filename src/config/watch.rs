//! 热更新：监听配置目录变更 → 重新合并校验 → `ArcSwap` 原子替换 → 通知订阅者。
//!
//! - 校验失败（toml 写坏、结构不匹配）**保留旧值并告警**（fail-safe，不中断服务）；
//! - 监听父目录并按文件名过滤（编辑器保存/原子替换都不会丢事件），去抖合并连发事件；
//! - 环境变量覆盖在每次重载时按原优先级重新应用；
//! - 重载只替换配置值：DB / Redis 连接池、JWT 密钥等已初始化资源**不会**自动重建，
//!   需要响应变更的组件通过 [`ConfigHandle::subscribe`] 注册回调自行处理
//!   （如日志级别即时生效）。

use std::time::Duration; // 引入时长类型，用于配置防抖窗口

use notify::Watcher as _; // 引入 notify 的 Watcher trait 以调用 watch 方法
use serde::de::DeserializeOwned; // 引入可反序列化 trait 作为泛型约束
use serde::Serialize; // 引入序列化 trait，供差异比较使用

use super::settings::ConfigHandle; // 引入配置只读句柄类型
use super::source::LoadOptions; // 引入加载选项类型

/// 热更新监听器（泛型配置根：框架用 `Settings`，应用可用 `AppSettings`）。
/// 变更通知统一走 [`ConfigHandle::subscribe`] 注册的回调。
pub struct Watcher<T: DeserializeOwned + Serialize + Clone + Send + Sync + 'static> { // 泛型配置热更新监听器
    options: LoadOptions, // 加载选项，重载时复用
    handle: ConfigHandle<T>, // 待热替换的配置句柄
    debounce: Duration, // 事件防抖窗口时长
}

impl<T: DeserializeOwned + Serialize + Clone + Send + Sync + 'static> Watcher<T> { // 为监听器实现构造与方法
    pub fn new(options: LoadOptions, handle: ConfigHandle<T>, debounce_ms: u64) -> Self { // 构造监听器
        Self { // 构造监听器实例
            options, // 保存加载选项
            handle, // 保存配置句柄
            debounce: Duration::from_millis(debounce_ms.max(50)), // 防抖窗口至少 50ms，避免过于频繁重载
        }
    }

    /// 被监听的文件（default.toml + {env}.toml，存在的才监听）
    fn watched_files(&self) -> Vec<std::path::PathBuf> { // 计算需要监听的配置文件路径
        ["default", self.options.environment.file_stem()] // 默认配置与环境配置的文件名干
            .iter() // 迭代文件名干
            .map(|stem| std::path::Path::new(&self.options.dir).join(format!("{stem}.toml"))) // 拼成 {dir}/{stem}.toml 路径
            .filter(|p| p.is_file()) // 只保留实际存在的文件
            .collect() // 收集为路径列表
    }

    /// 启动监听线程。文件一个都不存在时直接返回（无事可监听）。
    pub fn spawn(self) { // 启动后台监听线程
        let files = self.watched_files(); // 计算待监听文件
        if files.is_empty() { // 若没有任何配置文件存在
            return; // 直接返回，不启动监听
        }

        let (tx, rx) = std::sync::mpsc::channel::<notify::Event>(); // 创建事件通道，回调线程发、监听线程收
        let mut watcher = match notify::recommended_watcher(move |res| { // 创建推荐的文件系统监听器
            if let Ok(ev) = res { // 事件解析成功时
                let _ = tx.send(ev); // 把事件转发到通道
            }
        }) {
            Ok(w) => w, // 创建成功则使用该监听器
            Err(e) => { // 创建失败
                tracing::warn!(error = %e, "config watcher init failed, hot reload disabled"); // 告警并禁用热更新
                return; // 放弃启动监听
            }
        };

        // 监听父目录 + 按文件名过滤：直接 watch 文件在编辑器 rename/替换后失效
        let watched_names: Vec<String> = files // 收集待监听的纯文件名
            .iter() // 迭代文件路径
            .map(|p| p.file_name().unwrap_or_default().to_string_lossy().to_string()) // 提取文件名字符串
            .collect(); // 收集为文件名列表
        let mut watched_dirs: Vec<std::path::PathBuf> = Vec::new(); // 已成功监听的目录列表
        for f in &files { // 遍历每个待监听文件
            let dir = f.parent().map(|d| d.to_path_buf()).unwrap_or_default(); // 取其父目录
            if !watched_dirs.contains(&dir) { // 该目录尚未监听时
                if let Err(e) = watcher.watch(&dir, notify::RecursiveMode::NonRecursive) { // 非递归监听该目录
                    // 单目录失败只放弃该目录，不影响已成功监听的其他目录
                    tracing::warn!(dir = %dir.display(), error = %e, "watch dir failed, skipping"); // 告警并跳过该目录
                    continue; // 继续处理下一个文件
                }
                watched_dirs.push(dir); // 记录已成功监听的目录
            }
        }

        let options = self.options.clone(); // 克隆加载选项移入监听线程
        let handle = self.handle; // 移出配置句柄
        let debounce = self.debounce; // 移出防抖窗口时长
        std::thread::Builder::new() // 创建监听线程构建器
            .name("config-watcher".to_string()) // 命名线程便于排查
            .spawn(move || { // 启动线程执行监听循环
                let _watcher = watcher; // 保活：drop 即停发事件，监听线程随之退出
                loop { // 事件处理主循环
                    let Ok(first) = rx.recv() else { // 阻塞等待首个事件
                        return; // channel 关闭（进程退出）
                    };
                    if !matches_watched(&first, &watched_names) { // 事件与目标配置文件无关时
                        continue; // 忽略并等待下一个事件
                    }
                    // 防抖：等到静默窗口再重载（编辑器一次保存连发多个事件）。
                    // 窗口内的**无关文件**事件不得重置窗口，否则目录里其他文件
                    // （日志、.gitkeep）持续变动会无限推迟 reload。
                    let mut deadline = // 计算防抖截止时刻
                        std::time::Instant::now() + debounce;
                    loop { // 防抖等待循环
                        let remaining = deadline.saturating_duration_since(std::time::Instant::now()); // 距截止时刻剩余时长
                        if remaining.is_zero() { // 已到达截止时刻
                            break; // 结束防抖等待
                        }
                        match rx.recv_timeout(remaining) { // 在剩余窗口内等待事件
                            Ok(ev) => { // 收到窗口内的新事件
                                if matches_watched(&ev, &watched_names) { // 仅当是目标文件事件才重置窗口
                                    deadline = std::time::Instant::now() + debounce; // 顺延截止时刻
                                }
                            }
                            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => break, // 静默窗口结束，触发重载
                            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return, // 通道断开，退出线程
                        }
                    }
                    reload_once(&options, &handle); // 执行一次配置重载
                }
            })
            .expect("spawn config-watcher thread failed"); // 线程创建失败则 panic
    }
}

/// 事件是否指向被监听的配置文件（只关心内容变更类事件）
fn matches_watched(ev: &notify::Event, names: &[String]) -> bool { // 判断事件是否与目标配置文件相关
    let relevant = matches!( // 判断事件类型是否属于关心的变更类
        ev.kind, // 事件类型
        notify::EventKind::Modify(_) | notify::EventKind::Create(_) | notify::EventKind::Remove(_) // 修改/创建/删除均视为相关
    );
    relevant // 类型相关
        && ev.paths.iter().any(|p| { // 且任一事件路径的文件名在监听列表内
            p.file_name() // 取路径文件名
                .map(|n| names.contains(&n.to_string_lossy().to_string())) // 判断是否命中监听文件名列表
                .unwrap_or(false) // 无文件名时视为不相关
        })
}

/// 执行一次重载：成功则记录变更、原子切换（并触发 handle 上注册的回调）；
/// 失败保留旧配置并告警（fail-safe）
pub fn reload_once<T: DeserializeOwned + Serialize + Clone + Send + Sync + 'static>( // 按原选项重载一次配置
    options: &LoadOptions, // 加载选项
    handle: &ConfigHandle<T>, // 目标配置句柄
) {
    match super::source::load::<T>(options) { // 重新加载并校验配置
        Ok(new) => { // 重载成功
            log_changes(handle.load().as_ref(), &new); // 记录新旧配置的段级差异
            handle.store(new); // 原子替换并通知订阅者
        }
        Err(e) => { // 重载失败
            tracing::error!(error = %e, "config reload failed, keeping previous config"); // 保留旧配置并记录错误
        }
    }
}

/// 顶层段级 diff 日志：info 列出变化的段名，debug 输出新配置全文
fn log_changes<T: serde::Serialize>(old: &T, new: &T) { // 比较并记录新旧配置的段级变化
    let (Ok(old_v), Ok(new_v)) = (serde_json::to_value(old), serde_json::to_value(new)) else { // 序列化为 JSON 值
        return; // 序列化失败则不记录差异
    };
    let (Some(old_obj), Some(new_obj)) = (old_v.as_object(), new_v.as_object()) else { // 取二者的对象视图
        return; // 非对象结构则不比较
    };
    let changed: Vec<&String> = old_obj // 收集发生变化的顶层段名
        .keys() // 遍历旧配置的所有键
        .filter(|k| old_obj.get(*k) != new_obj.get(*k)) // 保留值发生变化的键
        .chain(new_obj.keys().filter(|k| !old_obj.contains_key(*k))) // 追加新增的键
        .collect(); // 收集为变化键列表
    if changed.is_empty() { // 若无任何变化
        return; // 不输出日志
    }
    let names: Vec<&str> = changed.iter().map(|s| s.as_str()).collect(); // 转换为字符串切片列表
    // 只打变更段名：全文 JSON 会把 DB 密码 / JWT secret / 各连接串打进日志
    //（环境变量来源同样会被合并进 Settings），debug 级别也不例外
    tracing::info!(changed = ?names, "config hot-reloaded"); // 记录变更段名
}
