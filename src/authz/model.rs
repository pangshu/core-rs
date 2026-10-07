//! Casbin 模型（文档 三·14）：RBAC 基础版 / RBAC with domains 多租户版。
//! 模型由 `[authz].model_path` 指向应用侧 .conf 文件；本模块提供两份内置模板，
//! 应用初始化时可直接落盘使用，避免手写 Casbin conf。

/// RBAC 基础版模板（sub, obj, act；g 角色继承）
pub const RBAC_MODEL: &str = r#"[request_definition]
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

/// RBAC with domains 模板（多租户：sub, dom, obj, act；g 租户内角色继承）
pub const RBAC_WITH_DOMAINS_MODEL: &str = r#"[request_definition]
r = sub, dom, obj, act

[policy_definition]
p = sub, dom, obj, act

[role_definition]
g = _, _, _

[policy_effect]
e = some(where (p.eft == allow))

[matchers]
m = g(r.sub, p.sub, r.dom) && r.dom == p.dom && r.obj == p.obj && r.act == p.act
"#;

/// 把内置模板写到目标路径（应用初始化 CLI 用）
pub fn write_default_model(path: &str, multi_tenant: bool) -> std::io::Result<()> { // 将内置模型模板落盘到指定路径
    let content = if multi_tenant { // 按是否多租户选择对应模板内容
        RBAC_WITH_DOMAINS_MODEL // 多租户：选用带 domains 的模型
    } else { // 非多租户分支
        RBAC_MODEL // 基础版 RBAC 模型
    };
    if let Some(parent) = std::path::Path::new(path).parent() { // 取目标文件的父目录（可能不存在）
        std::fs::create_dir_all(parent)?; // 递归创建父目录，避免写文件时目录缺失
    }
    std::fs::write(path, content) // 把选定的模型内容写入目标路径
}
