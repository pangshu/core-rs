//! 索引目录的文件系统细节（权限 / 落盘原子性）。
//!
//! 索引里存的是**文档原文**（`STORED` 字段），默认 umask 下通常是 0755/0644，
//! 多用户主机或共享卷上等于把内容公开。这里统一把目录收到 0700（Unix）：
//! 目录不可穿越后，里面的文件即使仍是 0644 也访问不到。
//!
//! Windows 不处理（ACL 继承由部署侧保证），只保证落盘原子性。

use std::io::Write;
use std::path::Path;

use crate::errors::{AppError, AppResult, ErrorCode};

/// 目录权限收到 0700（Unix）。
pub(crate) fn harden_dir(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(error) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)) {
            tracing::warn!(
                path = %path.display(),
                %error,
                "收紧索引目录权限失败（索引内容为文档原文，建议手工 chmod 700）"
            );
        }
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
}

/// 目录项落盘（rename 之后调用，保证 `CURRENT` 的改动不会因掉电丢失）。
pub(crate) fn sync_dir(path: &Path) {
    #[cfg(unix)]
    {
        if let Ok(dir) = std::fs::File::open(path) {
            let _ = dir.sync_all();
        }
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
}

/// 原子写一个小文件：`create_new` + `write_all` + `sync_all`，权限 0600。
///
/// 用 `create_new` 是为了不跟随已存在的符号链接（固定文件名 + 跟随符号链接
/// = 任意文件覆盖）。
pub(crate) fn write_new(path: &Path, content: &str) -> AppResult<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }

    let mut file = options.open(path).map_err(|error| {
        AppError::biz(
            ErrorCode::SERVICE_UNAVAILABLE,
            format!("写入索引版本文件失败：{error}"),
        )
    })?;
    file.write_all(content.as_bytes()).map_err(|error| {
        AppError::biz(
            ErrorCode::SERVICE_UNAVAILABLE,
            format!("写入索引版本文件失败：{error}"),
        )
    })?;
    file.sync_all().map_err(|error| {
        AppError::biz(
            ErrorCode::SERVICE_UNAVAILABLE,
            format!("刷盘索引版本文件失败：{error}"),
        )
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_write_and_sync() {
        let dir = std::env::temp_dir().join(format!("aurora-fsx-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("CURRENT");
        write_new(&path, "v1").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "v1");
        // 已存在时必须失败（不覆盖、不跟随）
        assert!(write_new(&path, "v2").is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn should_not_follow_symlink() {
        let dir = std::env::temp_dir().join(format!("aurora-fsx-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("target");
        std::fs::write(&target, "secret").unwrap();
        let link = dir.join("link");
        if make_symlink(&target, &link) {
            assert!(write_new(&link, "v1").is_err(), "不能顺着符号链接写");
            assert_eq!(std::fs::read_to_string(&target).unwrap(), "secret");
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(unix)]
    fn make_symlink(target: &Path, link: &Path) -> bool {
        std::os::unix::fs::symlink(target, link).is_ok()
    }

    #[cfg(not(unix))]
    fn make_symlink(_target: &Path, _link: &Path) -> bool {
        // Windows 建符号链接需要特权，跳过
        false
    }
}
