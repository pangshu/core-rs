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

use std::sync::Arc;
use std::task::{Context, Poll};

use axum::extract::Request;
use axum::response::{IntoResponse, Response};
use tower::Layer;

use crate::error::AppError;
use crate::state::CoreState;

/// 路由要求的权限（由 [`required`] 层写入 extension，供日志/内省用）
#[derive(Debug, Clone)]
pub struct RequiredPermission {
    pub obj: String,
    pub act: String,
    /// 多租户场景的 domain（RBAC with domains 模型用）；空 = 不校验 dom
    pub domain: Option<String>,
}

impl RequiredPermission {
    pub fn new(obj: impl Into<String>, act: impl Into<String>) -> Self {
        Self {
            obj: obj.into(),
            act: act.into(),
            domain: None,
        }
    }

    pub fn with_domain(mut self, domain: impl Into<String>) -> Self {
        self.domain = Some(domain.into());
        self
    }
}

/// 声明路由所需权限并**就地强制校验**（推荐入口）：
/// `get(handler).layer(required("users", "read"))`。
pub fn required(obj: impl AsRef<str>, act: impl AsRef<str>) -> RequiredLayer {
    RequiredLayer {
        obj: Arc::from(obj.as_ref()),
        act: Arc::from(act.as_ref()),
        domain: None,
    }
}

/// 同 [`required`]，附多租户 domain。
pub fn required_in(
    domain: impl AsRef<str>,
    obj: impl AsRef<str>,
    act: impl AsRef<str>,
) -> RequiredLayer {
    RequiredLayer {
        obj: Arc::from(obj.as_ref()),
        act: Arc::from(act.as_ref()),
        domain: Some(Arc::from(domain.as_ref())),
    }
}

/// [`required`] 的 tower Layer
#[derive(Clone)]
pub struct RequiredLayer {
    obj: Arc<str>,
    act: Arc<str>,
    domain: Option<Arc<str>>,
}

impl<S> Layer<S> for RequiredLayer {
    type Service = Required<S>;

    fn layer(&self, inner: S) -> Self::Service {
        Required {
            inner,
            obj: self.obj.clone(),
            act: self.act.clone(),
            domain: self.domain.clone(),
        }
    }
}

/// 强制校验服务：未配置强制器 fail-closed 500，未认证 401，权限不足 403
pub struct Required<S> {
    inner: S,
    obj: Arc<str>,
    act: Arc<str>,
    domain: Option<Arc<str>>,
}

impl<S> Clone for Required<S>
where
    S: Clone,
{
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            obj: self.obj.clone(),
            act: self.act.clone(),
            domain: self.domain.clone(),
        }
    }
}

impl<S> tower::Service<Request> for Required<S>
where
    S: tower::Service<Request, Response = Response, Error = std::convert::Infallible>
        + Clone
        + Send
        + 'static,
    S::Future: Send + 'static,
{
    type Response = Response;
    type Error = std::convert::Infallible;
    type Future = futures::future::BoxFuture<'static, Result<Response, Self::Error>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request) -> Self::Future {
        let obj = self.obj.clone();
        let act = self.act.clone();
        let domain = self.domain.clone();
        let mut inner = self.inner.clone();
        Box::pin(async move {
            match enforce(req, &obj, &act, domain.as_deref()).await {
                Ok(req) => inner.call(req).await,
                Err(resp) => Ok(resp),
            }
        })
    }
}

/// 执行一次授权校验；通过则原样放行请求，否则返回现成的错误响应
#[allow(clippy::result_large_err)] // Err 携带完整 Response（统一错误体），此处属预期形态
async fn enforce(
    mut req: Request,
    obj: &str,
    act: &str,
    domain: Option<&str>,
) -> Result<Request, Response> {
    req.extensions_mut().insert(RequiredPermission {
        obj: obj.to_string(),
        act: act.to_string(),
        domain: domain.map(Into::into),
    });

    // fail-closed：拿不到强制器宁可 500 也绝不放行
    let Some(core) = req.extensions().get::<CoreState>().cloned() else {
        tracing::error!(
            obj, act,
            "authz: CoreState extension missing (router not assembled via App::serve?)"
        );
        return Err(AppError::internal("authorization not configured").into_response());
    };
    let Some(enforcer) = core.authz.clone() else {
        tracing::error!(obj, act, "authz: casbin enforcer not configured");
        return Err(AppError::internal("authorization not configured").into_response());
    };

    // 身份须先经 auth 中间件注入（推荐顺序 auth → authz）
    let Some(identity) = req.extensions().get::<crate::auth::Identity>().cloned() else {
        return Err(AppError::unauthorized("authentication required").into_response());
    };

    let result = match domain {
        Some(dom) => {
            enforcer
                .enforce_with_domain(&identity.id, dom, obj, act)
                .await
        }
        None => enforcer.enforce(&identity.id, obj, act).await,
    };
    match result {
        Ok(true) => Ok(req),
        Ok(false) => Err(AppError::forbidden(format!("requires {obj}:{act} permission")).into_response()),
        Err(e) => Err(e.into_response()),
    }
}
