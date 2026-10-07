//! 安全响应头：HSTS / X-Content-Type-Options / X-Frame-Options / CSP /
//! Referrer-Policy。值与开关由 `[server.security_headers]` 配置；
//! 值为空字符串的项不发送。

use std::sync::Arc; // 引入原子引用计数指针，用于跨请求共享头列表

use axum::Router; // 引入 axum 路由类型
use axum::http::header::{HeaderName, HeaderValue}; // 引入响应头名与值类型

use crate::config::sections::SecurityHeadersSettings; // 引入安全响应头配置节

/// 按配置挂载安全响应头层（自组装也可直接调用；`enabled=false` 时原样返回）
pub fn apply<S: Clone + Send + Sync + 'static>( // 定义安全头层装配函数
    router: Router<S>, // 待装配的路由
    cfg: &SecurityHeadersSettings, // 安全头配置
) -> Router<S> { // 返回装配后的路由
    if !cfg.enabled { // 未开启安全头时
        return router; // 直接返回不挂载
    }
    let mut headers: Vec<(HeaderName, HeaderValue)> = Vec::new(); // 预构建要注入的头名值列表
    let mut push = |name: &'static str, value: &str| { // 定义收集器：值为空则跳过
        if !value.is_empty() { // 仅处理非空配置值
            if let Ok(v) = HeaderValue::from_str(value) { // 仅接受合法头值
                headers.push((HeaderName::from_static(name), v)); // 收集该头名值对
            }
        }
    };
    push("x-content-type-options", "nosniff"); // 固定注入禁止 MIME 嗅探头
    push("x-frame-options", &cfg.frame_options); // 注入点击劫持防护头
    push("content-security-policy", &cfg.content_security_policy); // 注入内容安全策略头
    push("referrer-policy", &cfg.referrer_policy); // 注入 Referrer 策略头
    push("strict-transport-security", &cfg.hsts); // 注入 HSTS 头
    let headers = Arc::new(headers); // 用 Arc 包裹以便闭包多请求共享

    router.layer(axum::middleware::from_fn( // 挂载注入安全头的中间件
        move |req: axum::extract::Request, next: axum::middleware::Next| { // 中间件闭包
        let headers = headers.clone(); // 每个请求克隆 Arc（仅引用计数 +1）
        async move { // 异步处理块
            let mut res = next.run(req).await; // 先调用下游取得响应
            for (name, value) in headers.iter() { // 遍历预构建头列表
                res.headers_mut().insert(name, value.clone()); // 逐个写入响应头
            }
            res // 返回已加头的响应
        }
        },
    ))
}
