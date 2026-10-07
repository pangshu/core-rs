//! Casbin 授权层（feature = "casbin"，文档 三·14）：按路由声明校验 `obj/act`，
//! 未过返回 403（未认证 401）。
//!
//! 路由声明权限要求（**声明即校验**，强制逻辑与声明在同一层，与挂载顺序无关）：
//!
//! ```rust,ignore
//! use core_rs::middleware::authz::required;
//! Router::new()
//!     .route("/admin/users", get(list).layer(required("users", "read")))
//! ```
//!
//! 设计说明：不要用「全局中间件 + 路由上挂 `Extension(RequiredPermission)`」实现
//! 授权——`Router::layer` 后挂的层在外层先执行，会先于路由内层读取 extension，
//! 声明永远读不到（fail-open）。[`required`] 把声明与强制放进同一个路由内层，
//! 从类型上杜绝这类层序绕过；未配置强制器/未挂 CoreState 时 **fail-closed 500**，
//! 未认证 401，权限不足 403。

use std::sync::Arc; // 引入原子引用计数指针，用于低成本共享权限字符串
use std::task::{Context, Poll}; // 引入轮询上下文与轮询结果类型

use axum::extract::Request; // 引入 axum 请求类型
use axum::response::{IntoResponse, Response}; // 引入响应转换 trait 与响应类型
use tower::Layer; // 引入 tower 的 Layer trait，用于包装服务

use crate::error::AppError; // 引入统一应用错误类型
use crate::state::CoreState; // 引入框架核心状态（内含 casbin 强制器）

/// 路由要求的权限（由 [`required`] 层写入 extension，供日志/内省用）
#[derive(Debug, Clone)] // 派生调试与克隆
pub struct RequiredPermission { // 路由声明的权限要求
    pub obj: String, // 资源对象（如 users）
    pub act: String, // 操作动作（如 read）
    /// 多租户场景的 domain（RBAC with domains 模型用）；空 = 不校验 dom
    pub domain: Option<String>, // 可选的租户域，None 表示不校验
}

impl RequiredPermission { // 为权限声明实现构造方法
    pub fn new(obj: impl Into<String>, act: impl Into<String>) -> Self { // 构造不含 domain 的权限声明
        Self { // 组装结构体
            obj: obj.into(), // 资源对象转 String
            act: act.into(), // 操作动作转 String
            domain: None, // 默认不校验 domain
        }
    }

    pub fn with_domain(mut self, domain: impl Into<String>) -> Self { // 链式附加租户域
        self.domain = Some(domain.into()); // 设置 domain
        self // 返回自身以便链式调用
    }
}

/// 声明路由所需权限并**就地强制校验**（推荐入口）：
/// `get(handler).layer(required("users", "read"))`。
pub fn required(obj: impl AsRef<str>, act: impl AsRef<str>) -> RequiredLayer { // 创建权限校验层
    RequiredLayer { // 组装层
        obj: Arc::from(obj.as_ref()), // 资源对象转为共享 Arc<str>
        act: Arc::from(act.as_ref()), // 操作动作转为共享 Arc<str>
        domain: None, // 默认不校验 domain
    }
}

/// 同 [`required`]，附多租户 domain。
pub fn required_in( // 创建带租户域的权限校验层
    domain: impl AsRef<str>, // 租户域
    obj: impl AsRef<str>, // 资源对象
    act: impl AsRef<str>, // 操作动作
) -> RequiredLayer { // 返回权限校验层
    RequiredLayer { // 组装层
        obj: Arc::from(obj.as_ref()), // 资源对象转为共享 Arc<str>
        act: Arc::from(act.as_ref()), // 操作动作转为共享 Arc<str>
        domain: Some(Arc::from(domain.as_ref())), // 设置租户域
    }
}

/// [`required`] 的 tower Layer
#[derive(Clone)] // 派生克隆，tower 层需要可克隆
pub struct RequiredLayer { // 权限校验的 Layer 实现
    obj: Arc<str>, // 资源对象
    act: Arc<str>, // 操作动作
    domain: Option<Arc<str>>, // 可选租户域
}

impl<S> Layer<S> for RequiredLayer { // 为 RequiredLayer 实现 tower Layer
    type Service = Required<S>; // 包装后产出 Required 服务

    fn layer(&self, inner: S) -> Self::Service { // 用本层包装内层服务
        Required { // 组装 Required 服务
            inner, // 内层服务
            obj: self.obj.clone(), // 克隆资源对象
            act: self.act.clone(), // 克隆操作动作
            domain: self.domain.clone(), // 克隆租户域
        }
    }
}

/// 强制校验服务：未配置强制器 fail-closed 500，未认证 401，权限不足 403
pub struct Required<S> { // 承载权限校验逻辑的服务
    inner: S, // 被包装的内层服务
    obj: Arc<str>, // 资源对象
    act: Arc<str>, // 操作动作
    domain: Option<Arc<str>>, // 可选租户域
}

impl<S> Clone for Required<S> // 手写 Clone（内层可能未派生 Clone）
where // 泛型约束子句
    S: Clone, // 内层可克隆即可克隆本服务
{
    fn clone(&self) -> Self { // 克隆本服务
        Self { // 组装副本
            inner: self.inner.clone(), // 克隆内层服务
            obj: self.obj.clone(), // 克隆资源对象
            act: self.act.clone(), // 克隆操作动作
            domain: self.domain.clone(), // 克隆租户域
        }
    }
}

impl<S> tower::Service<Request> for Required<S> // 为 Required 实现 tower Service
where // 泛型约束子句
    S: tower::Service<Request, Response = Response, Error = std::convert::Infallible> // 内层服务须处理 Request 且错误不可发生
        + Clone // 内层须可克隆（call 时移动克隆体）
        + Send // 内层须可跨线程
        + 'static, // 内层须为静态生命周期
    S::Future: Send + 'static, // 内层的 Future 须可跨线程
{
    type Response = Response; // 响应类型
    type Error = std::convert::Infallible; // 错误类型不可发生
    type Future = futures::future::BoxFuture<'static, Result<Response, Self::Error>>; // 装箱异步结果

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> { // 就绪轮询
        self.inner.poll_ready(cx) // 直接转发给内层
    }

    fn call(&mut self, req: Request) -> Self::Future { // 处理一次请求
        let obj = self.obj.clone(); // 克隆资源对象供异步任务使用
        let act = self.act.clone(); // 克隆操作动作供异步任务使用
        let domain = self.domain.clone(); // 克隆租户域供异步任务使用
        let mut inner = self.inner.clone(); // 克隆内层服务（tower 惯例：call 后需可复用）
        Box::pin(async move { // 装箱异步块
            match enforce(req, &obj, &act, domain.as_deref()).await { // 先执行授权校验
                Ok(req) => inner.call(req).await, // 通过则继续调用内层
                Err(resp) => Ok(resp), // 未过则直接返回错误响应
            }
        })
    }
}

/// 执行一次授权校验；通过则原样放行请求，否则返回现成的错误响应
#[allow(clippy::result_large_err)] // Err 携带完整 Response（统一错误体），此处属预期形态
async fn enforce( // 授权校验核心逻辑
    mut req: Request, // 待校验请求（需可变以写 extension）
    obj: &str, // 资源对象
    act: &str, // 操作动作
    domain: Option<&str>, // 可选租户域
) -> Result<Request, Response> { // 通过返回请求，未过返回错误响应
    req.extensions_mut().insert(RequiredPermission { // 把权限声明写入 extension 供日志/内省
        obj: obj.to_string(), // 资源对象转 String
        act: act.to_string(), // 操作动作转 String
        domain: domain.map(Into::into), // 租户域转 String
    });

    // fail-closed：拿不到强制器宁可 500 也绝不放行
    let Some(core) = req.extensions().get::<CoreState>().cloned() else { // 取挂载的 CoreState
        tracing::error!( // 记录错误：路由未由 App::serve 装配
            obj, act, // 附上权限信息
            "authz: CoreState extension missing (router not assembled via App::serve?)" // 错误信息
        );
        return Err(AppError::internal("authorization not configured").into_response()); // 返回 500
    };
    let Some(enforcer) = core.authz.clone() else { // 取 casbin 强制器
        tracing::error!(obj, act, "authz: casbin enforcer not configured"); // 记录错误：未配置强制器
        return Err(AppError::internal("authorization not configured").into_response()); // 返回 500
    };

    // 身份须先经 auth 中间件注入（推荐顺序 auth → authz）
    let Some(identity) = req.extensions().get::<crate::auth::Identity>().cloned() else { // 取已认证身份
        return Err(AppError::unauthorized("authentication required").into_response()); // 无身份返回 401
    };

    let result = match domain { // 按是否带租户域选择校验入口
        Some(dom) => { // 带租户域
            enforcer // 调用强制器
                .enforce_with_domain(&identity.id, dom, obj, act) // 按 domain+sub+obj+act 校验
                .await // 等待校验结果
        }
        None => enforcer.enforce(&identity.id, obj, act).await, // 不带域时按 sub+obj+act 校验
    };
    match result { // 处理校验结果
        Ok(true) => Ok(req), // 允许：原样放行
        Ok(false) => Err(AppError::forbidden(format!("requires {obj}:{act} permission")).into_response()), // 拒绝：返回 403
        Err(e) => Err(e.into_response()), // 强制器出错：按错误响应返回
    }
}
