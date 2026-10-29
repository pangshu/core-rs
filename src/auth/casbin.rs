//! 授权（Casbin RBAC）。
//!
//! - 默认**拒绝**（default-deny）：无匹配策略即拒绝（见 `docs/11` §3）
//! - 内嵌默认 RBAC 模型；也可由 `[authz].model_path` / `policy_path` 加载文件
//! - [`Principal::roles`] 会逐个作为 Casbin `r.sub` 参与 `g(r.sub, p.sub)` 匹配

use std::sync::Arc;

use casbin::{CoreApi, DefaultModel, Enforcer, FileAdapter, MemoryAdapter, MgmtApi};
use tokio::sync::RwLock;

use crate::auth::identity::Principal;
use crate::errors::{AppError, AppResult, ErrorCode};

/// 内嵌默认 RBAC 模型（`g(r.sub, p.sub)` 支持角色继承，`e = some(where allow)` 默认拒绝）。
pub const DEFAULT_RBAC_MODEL: &str = r#"
[request_definition]
r = sub, obj, act

[policy_definition]
p = sub, obj, act

[role_definition]
g = _, _

[policy_effect]
e = some(where (p.eft == allow))

[matchers]
m = g(r.sub, p.sub) && r.obj == p.obj && r.act == p.act
"#;

/// Casbin RBAC 授权器。
///
/// 以 `Arc` 共享（`AuthService.authorizer`）。
#[derive(Clone)]
pub struct Authorizer {
    enforcer: Arc<RwLock<Enforcer>>,
}

impl Authorizer {
    /// 以「模型文本 + 策略文本」构造。
    ///
    /// 策略为 CSV 行：`p, <主体>, <对象>, <动作>` 与 `g, <角色>, <成员>`；
    /// `#` 开头为注释。内部使用 [`MemoryAdapter`]，因此支持运行期
    /// [`add_policy`](Self::add_policy) / [`add_role`](Self::add_role)。
    ///
    /// # Errors
    /// 模型或策略非法时返回内部错误（`50001`）。
    pub async fn from_model_and_policy(model: &str, policy: &str) -> AppResult<Self> {
        let model = DefaultModel::from_str(model)
            .await
            .map_err(|e| AppError::internal(anyhow::anyhow!("casbin model parse failed: {e}")))?;
        let mut enforcer = Enforcer::new(model, MemoryAdapter::default())
            .await
            .map_err(|e| AppError::internal(anyhow::anyhow!("casbin init failed: {e}")))?;

        for line in policy.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let params: Vec<String> = line.split(',').map(str::trim).map(str::to_string).collect();
            let rule: Vec<String> = params.get(1..).unwrap_or_default().to_vec();
            match params.first().map(String::as_str) {
                Some("p") => {
                    enforcer.add_policy(rule).await.map_err(|e| {
                        AppError::internal(anyhow::anyhow!("casbin load policy `{line}`: {e}"))
                    })?;
                }
                Some("g") => {
                    enforcer.add_grouping_policy(rule).await.map_err(|e| {
                        AppError::internal(anyhow::anyhow!("casbin load role `{line}`: {e}"))
                    })?;
                }
                _ => continue,
            }
        }

        Ok(Self {
            enforcer: Arc::new(RwLock::new(enforcer)),
        })
    }

    /// 以「内嵌默认模型 + 空策略」构造（默认拒绝一切）。
    ///
    /// # Errors
    /// 模型非法时返回内部错误（`50001`）。
    pub async fn embedded() -> AppResult<Self> {
        Self::from_model_and_policy(DEFAULT_RBAC_MODEL, "").await
    }

    /// 从模型 / 策略文件加载。
    ///
    /// # Errors
    /// 读取或解析失败时返回内部错误（`50001`）。
    pub async fn from_files(model_path: &str, policy_path: &str) -> AppResult<Self> {
        let model = DefaultModel::from_file(model_path).await.map_err(|e| {
            AppError::internal(anyhow::anyhow!("read casbin model `{model_path}`: {e}"))
        })?;
        let enforcer = Enforcer::new(model, FileAdapter::new(policy_path.to_string()))
            .await
            .map_err(|e| AppError::internal(anyhow::anyhow!("casbin init failed: {e}")))?;
        Ok(Self {
            enforcer: Arc::new(RwLock::new(enforcer)),
        })
    }

    /// 新增一条 `p` 策略（主体 → 对象 → 动作）。
    ///
    /// # Errors
    /// 策略已存在或底层失败时返回错误。
    pub async fn add_policy(&self, subject: &str, object: &str, action: &str) -> AppResult<()> {
        let added = self
            .enforcer
            .write()
            .await
            .add_policy(vec![
                subject.to_string(),
                object.to_string(),
                action.to_string(),
            ])
            .await
            .map_err(|e| AppError::internal(anyhow::anyhow!("casbin add_policy failed: {e}")))?;
        if !added {
            return Err(AppError::conflict("policy already exists"));
        }
        Ok(())
    }

    /// 新增一条 `g` 分组策略（角色 ← 成员）。
    ///
    /// # Errors
    /// 分组已存在或底层失败时返回错误。
    pub async fn add_role(&self, role: &str, member: &str) -> AppResult<()> {
        let added = self
            .enforcer
            .write()
            .await
            .add_grouping_policy(vec![role.to_string(), member.to_string()])
            .await
            .map_err(|e| AppError::internal(anyhow::anyhow!("casbin add_role failed: {e}")))?;
        if !added {
            return Err(AppError::conflict("role membership already exists"));
        }
        Ok(())
    }

    /// 判定主体是否可对 `object` 执行 `action`。
    ///
    /// 依次尝试 `principal.sub` 与每个 `principal.roles` 作为 Casbin `r.sub`；
    /// 任一命中即放行（默认拒绝）。
    ///
    /// # Errors
    /// 策略引擎出错时返回内部错误（`50001`）。
    pub async fn enforce(
        &self,
        principal: &Principal,
        object: &str,
        action: &str,
    ) -> AppResult<bool> {
        let enforcer = self.enforcer.read().await;
        let mut subjects = Vec::with_capacity(1 + principal.roles.len());
        subjects.push(principal.sub.clone());
        subjects.extend(principal.roles.iter().cloned());

        for subject in subjects {
            let allowed = enforcer
                .enforce(vec![subject.as_str(), object, action])
                .map_err(|e| AppError::internal(anyhow::anyhow!("casbin enforce failed: {e}")))?;
            if allowed {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// 判定并转换为权限错误（供 [`require_permission!`](crate::require_permission) 使用）。
    ///
    /// # Errors
    /// 无权时返回 `20004`（403）。
    pub async fn require(
        &self,
        principal: &Principal,
        object: &str,
        action: &str,
    ) -> AppResult<()> {
        if self.enforce(principal, object, action).await? {
            Ok(())
        } else {
            Err(AppError::biz(
                ErrorCode::PERMISSION_DENIED,
                "permission denied",
            ))
        }
    }
}

impl std::fmt::Debug for Authorizer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Authorizer").finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn should_deny_by_default() {
        let authorizer = Authorizer::embedded().await.unwrap();
        let principal = Principal::from_id(1);
        assert!(
            !authorizer
                .enforce(&principal, "users", "read")
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn should_allow_after_adding_policy() {
        let authorizer = Authorizer::embedded().await.unwrap();
        authorizer
            .add_policy("alice", "users", "read")
            .await
            .unwrap();

        let alice = Principal::new("alice");
        assert!(authorizer.enforce(&alice, "users", "read").await.unwrap());
        assert!(!authorizer.enforce(&alice, "users", "delete").await.unwrap());
        assert!(
            !authorizer
                .enforce(&Principal::new("bob"), "users", "read")
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn should_resolve_role_inheritance() {
        let authorizer = Authorizer::embedded().await.unwrap();
        authorizer
            .add_policy("admin", "users", "delete")
            .await
            .unwrap();
        authorizer.add_role("admin", "alice").await.unwrap();

        // alice 通过角色 admin 获得权限
        let alice = Principal::new("alice").with_role("admin");
        assert!(authorizer.enforce(&alice, "users", "delete").await.unwrap());

        // 无该角色的主体仍被拒绝
        assert!(
            !authorizer
                .enforce(&Principal::new("bob"), "users", "delete")
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn should_require_map_to_403() {
        let authorizer = Authorizer::embedded().await.unwrap();
        let err = authorizer
            .require(&Principal::new("alice"), "users", "read")
            .await
            .unwrap_err();
        assert_eq!(err.code(), ErrorCode::PERMISSION_DENIED);
        assert_eq!(err.code().status(), axum::http::StatusCode::FORBIDDEN);
    }
}
