//! multipart 文件上传（feature = "upload"）：流式落盘 + 大小限制 + 文件名重写。
//! 静态资源由 `Application` 根据 `[server].static_dir` 自动挂载到 /static/*。

use std::path::Path;

use axum::extract::multipart::Multipart;
use tokio::io::AsyncWriteExt;
use uuid::Uuid;

use crate::error::{AppError, AppResult};

#[derive(Debug, Clone)]
pub struct SavedFile {
    pub original_name: String,
    pub path: std::path::PathBuf,
    pub size: u64,
}

/// 保存 multipart 中的所有文件字段（跳过普通表单字段）。
/// 文件重命名为 `uuid + 净化后的扩展名`，防止路径穿越；超限立即报错并清理半成品。
pub async fn save_multipart(
    mp: &mut Multipart,
    dir: impl AsRef<Path>,
    max_file_size: u64,
) -> AppResult<Vec<SavedFile>> {
    let dir = dir.as_ref();
    tokio::fs::create_dir_all(dir)
        .await
        .map_err(AppError::Io)?;

    let mut saved = Vec::new();
    while let Some(mut field) = mp
        .next_field()
        .await
        .map_err(|e| AppError::bad_request(format!("multipart error: {e}")))?
    {
        let Some(filename) = field.file_name().map(|s| s.to_string()) else {
            continue; // 非文件字段
        };

        let ext = sanitize_ext(Path::new(&filename).extension().and_then(|e| e.to_str()));
        let name = format!("{uuid}{ext}", uuid = Uuid::new_v4());
        let path = dir.join(&name);
        let mut file = tokio::fs::File::create(&path)
            .await
            .map_err(AppError::Io)?;

        let mut size: u64 = 0;
        loop {
            let Some(bytes) = field
                .chunk()
                .await
                .map_err(|e| AppError::bad_request(format!("multipart read error: {e}")))?
            else {
                break;
            };
            size += bytes.len() as u64;
            if size > max_file_size {
                drop(file);
                let _ = tokio::fs::remove_file(&path).await;
                return Err(AppError::bad_request(format!(
                    "file {filename:?} exceeds limit {max_file_size} bytes"
                )));
            }
            file.write_all(&bytes).await.map_err(AppError::Io)?;
        }
        file.flush().await.map_err(AppError::Io)?;

        saved.push(SavedFile {
            original_name: filename,
            path,
            size,
        });
    }
    Ok(saved)
}

/// 扩展名净化：只保留 1~8 位字母数字
fn sanitize_ext(ext: Option<&str>) -> String {
    match ext {
        Some(e) if !e.is_empty() && e.len() <= 8 && e.chars().all(|c| c.is_ascii_alphanumeric()) => {
            format!(".{e}")
        }
        _ => String::new(),
    }
}
