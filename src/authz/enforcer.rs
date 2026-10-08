//! Casbin 强制器 [`Enforcer`]：装配入口 + `enforce(sub, obj, act)` 助手。
//!
//! 内部用 `tokio::sync::RwLock` 包裹 Casbin 强制器——读校验并发、写用于策略热更新。

use std::sync::Arc; // 引入原子引用计数指针，用于跨任务共享强制器

use casbin::{CoreApi, DefaultModel, MgmtApi}; // 引入 Casbin 核心 API：模型加载、强制校验与策略管理

use crate::config::sections::AuthzSettings; // 引入 [authz] 配置段，装配时读取模型/策略路径等
use crate::error::{AppError, AppResult}; // 引入框架统一错误类型与结果别名

use super::adapter; // 引入策略存储适配器（file / db）

/// Casbin 强制器（内部 tokio RwLock 支持策略热更新重载）
pub struct Enforcer { // 封装 Casbin 强制器的授权句柄
    inner: Arc<tokio::sync::RwLock<casbin::Enforcer>> // 用异步读写锁保护强制器：读校验并发、写用于热更新策略
}

impl Enforcer { // 强制器的装配与校验实现
    /// 按 `[authz]` 配置装配（App bootstrap 时自动调用）
    pub async fn build(settings: &AuthzSettings) -> AppResult<Self> { // 依据配置构建适配器并完成装配
        let a = adapter::DbOrFileAdapter::from_settings(settings) // 按 authz.source 创建 file 或 db 策略适配器
            .await // 等待适配器异步初始化完成
            .map_err(|e| AppError::internal(format!("casbin adapter init failed: {e}")))?; // 适配器失败转成内部错误返回
        Self::build_with(settings, a).await // 复用 build_with 完成模型加载与强制器构建
    }

    /// 用调用方准备好的 adapter 装配：db 策略源必须先把连接池注入 adapter
    /// 再传进来（[`Enforcer::new`] 内部会立即 `load_policy`，重建 adapter 会拿到
    /// db=None 的空壳导致启动失败）。
    pub async fn build_with( // 使用外部传入的适配器装配强制器
        settings: &AuthzSettings, // 授权配置，用于读取 model_path
        a: adapter::DbOrFileAdapter, // 已注入好连接池的策略适配器
    ) -> AppResult<Self> { // 返回装配完成的强制器
        if settings.model_path.is_empty() { // 模型路径为空说明配置不完整，直接报错
            return Err(AppError::internal( // 返回内部配置错误
                "authz.enabled = true but authz.model_path is empty", // 错误信息：启用授权但未配置模型路径
            ));
        }
        let m = DefaultModel::from_file(&settings.model_path) // 从 .conf 文件加载 Casbin 模型定义
            .await // 等待异步读取模型文件
            .map_err(|e| AppError::internal(format!("casbin model load failed: {e}")))?; // 模型加载失败转内部错误
        let enforcer = casbin::Enforcer::new(m, a) // 用模型与适配器构造 Casbin 强制器（内部会立即 load_policy）
            .await // 等待强制器初始化（含策略加载）
            .map_err(|e| AppError::internal(format!("casbin enforcer init failed: {e}")))?; // 初始化失败转内部错误
        Ok(Self { // 包装成框架的授权句柄返回
            inner: Arc::new(tokio::sync::RwLock::new(enforcer)), // 用异步读写锁包裹，便于后续策略热更新
        })
    }

    /// 核心 RBAC 校验：`enforce(user_id, 资源, 动作)`
    pub async fn enforce(&self, sub: &str, obj: &str, act: &str) -> AppResult<bool> { // 单租户 RBAC 校验入口
        let e = self.inner.read().await; // 取读锁：允许并发校验，仅阻塞策略写操作
        e.enforce((sub, obj, act)) // 调用 Casbin 判定 (主体, 资源, 动作) 是否允许
            .map_err(|e| AppError::internal(format!("casbin enforce failed: {e}"))) // 强制校验出错时转内部错误
    }

    /// 多租户校验（RBAC with domains 模型：sub, dom, obj, act）
    pub async fn enforce_with_domain( // 多租户 RBAC 校验入口
        &self, // 强制器自身
        sub: &str, // 主体（用户 id）
        dom: &str, // 域（租户标识）
        obj: &str, // 资源
        act: &str, // 动作
    ) -> AppResult<bool> { // 返回是否允许
        let e = self.inner.read().await; // 取读锁以并发执行校验
        e.enforce((sub, dom, obj, act)) // 调用 Casbin 判定 (主体, 域, 资源, 动作) 是否允许
            .map_err(|e| AppError::internal(format!("casbin enforce failed: {e}"))) // 校验出错时转内部错误
    }

    /// 校验不通过即 403（middleware/authz 与 handler 内检查共用）
    pub async fn require(&self, sub: &str, obj: &str, act: &str) -> AppResult<()> { // 校验不通过则返回 403 错误的便捷封装
        if self.enforce(sub, obj, act).await? { // 先执行 RBAC 校验，通过则放行
            Ok(()) // 校验通过：返回成功
        } else { // 校验不通过的分支
            Err(AppError::forbidden(format!("requires {obj}:{act} permission"))) // 返回 403 并附带所需权限说明
        }
    }

    /// 运行时加策略（热更新）
    pub async fn add_policy(&self, sub: &str, obj: &str, act: &str) -> AppResult<()> { // 运行时新增一条 RBAC 策略
        let mut e = self.inner.write().await; // 取写锁：修改策略需独占
        e.add_policy(vec![sub.to_string(), obj.to_string(), act.to_string()]) // 以 (主体, 资源, 动作) 构造策略并添加
            .await // 等待添加策略完成
            .map_err(|e| AppError::internal(format!("casbin add_policy failed: {e}")))?; // 添加失败转内部错误
        Ok(()) // 添加成功
    }

    /// 运行时移除策略（热更新）
    pub async fn remove_policy(&self, sub: &str, obj: &str, act: &str) -> AppResult<()> { // 运行时移除一条 RBAC 策略
        let mut e = self.inner.write().await; // 取写锁：修改策略需独占
        e.remove_policy(vec![sub.to_string(), obj.to_string(), act.to_string()]) // 以 (主体, 资源, 动作) 定位并移除策略
            .await // 等待移除策略完成
            .map_err(|e| AppError::internal(format!("casbin remove_policy failed: {e}")))?; // 移除失败转内部错误
        Ok(()) // 移除成功
    }

    /// 从存储全量重载策略（策略热更新，可挂到 config watcher / 定时任务）
    pub async fn reload(&self) -> AppResult<()> { // 从策略存储全量重载，实现热更新
        let mut e = self.inner.write().await; // 取写锁：重载会替换整个策略集，需独占
        e.load_policy() // 从适配器（file/db）重新加载全部策略
            .await // 等待策略加载完成
            .map_err(|e| AppError::internal(format!("casbin reload failed: {e}")))?; // 重载失败转内部错误
        Ok(()) // 重载成功
    }
}
