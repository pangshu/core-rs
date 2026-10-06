//! 授权（能做什么，文档 三·14）：Casbin RBAC —— 模型与策略由 config 集中配置。
//!
//! - [`model`]：Casbin 模型（RBAC 基础版 / RBAC with domains 多租户版）；
//! - [`adapter`]：策略存储 —— 文件（开发）/ DB（生产，经 SeaORM 读 `casbin_rule` 表），
//!   支持策略热更新；
//! - `Enforcer`：装配入口 + `enforce(sub, obj, act)` 助手；
//!   `middleware/authz` 在认证之后按路由要求 obj/act 校验，未过返回 403。

pub mod adapter;
pub mod model;

use std::sync::Arc;

use casbin::{CoreApi, DefaultModel, MgmtApi};

use crate::config::sections::AuthzSettings;
use crate::error::{AppError, AppResult};

/// Casbin 强制器（内部 tokio RwLock 支持策略热更新重载）
pub struct Enforcer {
    inner: Arc<tokio::sync::RwLock<casbin::Enforcer>>
}

impl Enforcer {
    /// 按 `[authz]` 配置装配（App bootstrap 时自动调用）
    pub async fn build(settings: &AuthzSettings) -> AppResult<Self> {
        let a = adapter::DbOrFileAdapter::from_settings(settings)
            .await
            .map_err(|e| AppError::internal(format!("casbin adapter init failed: {e}")))?;
        Self::build_with(settings, a).await
    }

    /// 用调用方准备好的 adapter 装配：db 策略源必须先把连接池注入 adapter
    /// 再传进来（[`Enforcer::new` 内部会立即 `load_policy`]，重建 adapter 会拿到
    /// db=None 的空壳导致启动失败）。
    pub async fn build_with(
        settings: &AuthzSettings,
        a: adapter::DbOrFileAdapter,
    ) -> AppResult<Self> {
        if settings.model_path.is_empty() {
            return Err(AppError::internal(
                "authz.enabled = true but authz.model_path is empty",
            ));
        }
        let m = DefaultModel::from_file(&settings.model_path)
            .await
            .map_err(|e| AppError::internal(format!("casbin model load failed: {e}")))?;
        let enforcer = casbin::Enforcer::new(m, a)
            .await
            .map_err(|e| AppError::internal(format!("casbin enforcer init failed: {e}")))?;
        Ok(Self {
            inner: Arc::new(tokio::sync::RwLock::new(enforcer)),
        })
    }

    /// 核心 RBAC 校验：`enforce(user_id, 资源, 动作)`
    pub async fn enforce(&self, sub: &str, obj: &str, act: &str) -> AppResult<bool> {
        let e = self.inner.read().await;
        e.enforce((sub, obj, act))
            .map_err(|e| AppError::internal(format!("casbin enforce failed: {e}")))
    }

    /// 多租户校验（RBAC with domains 模型：sub, dom, obj, act）
    pub async fn enforce_with_domain(
        &self,
        sub: &str,
        dom: &str,
        obj: &str,
        act: &str,
    ) -> AppResult<bool> {
        let e = self.inner.read().await;
        e.enforce((sub, dom, obj, act))
            .map_err(|e| AppError::internal(format!("casbin enforce failed: {e}")))
    }

    /// 校验不通过即 403（middleware/authz 与 handler 内检查共用）
    pub async fn require(&self, sub: &str, obj: &str, act: &str) -> AppResult<()> {
        if self.enforce(sub, obj, act).await? {
            Ok(())
        } else {
            Err(AppError::forbidden(format!("requires {obj}:{act} permission")))
        }
    }

    /// 运行时加策略（热更新）
    pub async fn add_policy(&self, sub: &str, obj: &str, act: &str) -> AppResult<()> {
        let mut e = self.inner.write().await;
        e.add_policy(vec![sub.to_string(), obj.to_string(), act.to_string()])
            .await
            .map_err(|e| AppError::internal(format!("casbin add_policy failed: {e}")))?;
        Ok(())
    }

    /// 运行时移除策略（热更新）
    pub async fn remove_policy(&self, sub: &str, obj: &str, act: &str) -> AppResult<()> {
        let mut e = self.inner.write().await;
        e.remove_policy(vec![sub.to_string(), obj.to_string(), act.to_string()])
            .await
            .map_err(|e| AppError::internal(format!("casbin remove_policy failed: {e}")))?;
        Ok(())
    }

    /// 从存储全量重载策略（策略热更新，可挂到 config watcher / 定时任务）
    pub async fn reload(&self) -> AppResult<()> {
        let mut e = self.inner.write().await;
        e.load_policy()
            .await
            .map_err(|e| AppError::internal(format!("casbin reload failed: {e}")))?;
        Ok(())
    }
}
