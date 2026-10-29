//! 证书热更新（`notify` + 防抖）。
//!
//! 流程（`docs/16` §5）：
//! `文件变更 → 防抖 500ms → 重新解析 → 成功则原子替换 CertStore（并丢弃 OCSP 装订）→ 失败保留旧证书`。
//!
//! 已建立的连接不受影响：新证书只作用于**新握手**。
//!
//! 替换必须走 [`CertSwap::apply`]：只换证书不丢 OCSP 缓存，会让 `SniResolver`
//! 继续返回缓存里那份带**旧证书链**的 `CertifiedKey`（续期后看起来成功、实际还在发旧证书）。

use std::time::Duration;

use notify::Watcher as _;

use crate::tls::service::CertSwap;

/// 防抖窗口：证书常以「写临时文件 + rename」方式更新，会产生多个事件。
const DEBOUNCE: Duration = Duration::from_millis(500);

/// 事件队列上限：只用来唤醒防抖循环，满了丢事件也不会漏掉「最后一次变更」。
const EVENT_QUEUE: usize = 64;

/// 启动监听任务（目录变更即热更新）。
pub(crate) fn spawn(swap: CertSwap) {
    let dir = swap.dir().to_path_buf();
    tokio::spawn(async move {
        let (tx, mut rx) = tokio::sync::mpsc::channel(EVENT_QUEUE);
        let mut watcher = match notify::recommended_watcher(move |event| {
            // 回调运行在 notify 的线程里，只做转发。有界队列：写入方可能比消费方快
            // （ACME 工具循环重试），无界队列会把事件堆成内存泄漏。
            let _ = tx.try_send(event);
        }) {
            Ok(watcher) => watcher,
            Err(error) => {
                tracing::warn!(%error, "证书监听器创建失败，热更新不可用");
                return;
            }
        };

        if let Err(error) = watcher.watch(&dir, notify::RecursiveMode::Recursive) {
            tracing::warn!(%error, dir = %dir.display(), "监听证书目录失败，热更新不可用");
            return;
        }
        tracing::info!(dir = %dir.display(), "证书热更新已启用");

        // watcher 必须在任务存活期间一直持有，否则监听会被立即取消
        let _watcher = watcher;

        while rx.recv().await.is_some() {
            // 防抖：持续有事件就不断顺延，静默 500ms 后才真正重载
            loop {
                tokio::select! {
                    () = tokio::time::sleep(DEBOUNCE) => break,
                    Some(_) = rx.recv() => {}
                }
            }

            // 解析是同步文件 IO：丢到阻塞线程池，别占住 tokio worker
            let reloader = swap.clone();
            let loaded = tokio::task::spawn_blocking(move || reloader.load()).await;
            match loaded {
                Ok(Ok(reloaded)) => {
                    let certs = swap.apply(reloaded);
                    tracing::info!(certs, "证书已热更新（仅对新连接生效）");
                }
                // fail-safe：坏文件绝不能让服务失去可用证书
                Ok(Err(error)) => {
                    tracing::warn!(%error, "证书热更新失败，继续使用旧证书");
                }
                Err(error) => {
                    tracing::warn!(%error, "证书热更新任务异常退出，继续使用旧证书");
                }
            }
        }
    });
}
