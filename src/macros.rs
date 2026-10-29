//! 声明宏。
//!
//! Rust 规定 `#[proc_macro_derive]` 必须定义在独立的 `proc-macro` crate 中，
//! 单 crate 方案下无法内置派生宏，故统一以 `macro_rules!` 实现（见
//! `docs/03-module-design.md` §7）。

/// 校验当前主体是否具备对某对象执行某动作的权限。
///
/// 权限不足时提前返回 `20004`（403，`权限不足`）；与
/// [`AuthService::require`](crate::auth::AuthService::require) 行为一致。
///
/// ```ignore
/// // handler 内
/// require_permission!(state.auth, &principal, "users", "delete");
/// ```
///
/// 仅在启用 `authz` feature 时可用（它调用 [`AuthService::enforce`](crate::auth::AuthService::enforce)，
/// 而 `enforce` 需要 RBAC 策略引擎）。
#[cfg(feature = "authz")]
#[macro_export]
macro_rules! require_permission {
    ($auth:expr, $principal:expr, $obj:expr, $act:expr $(,)?) => {{
        if !$auth.enforce($principal, $obj, $act).await? {
            return ::std::result::Result::Err($crate::errors::AppError::biz(
                $crate::errors::ErrorCode::PERMISSION_DENIED,
                "permission denied",
            ));
        }
    }};
}
