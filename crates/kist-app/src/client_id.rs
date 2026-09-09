//! 這台機器的 client id：16 bytes 亂數，第一次用時產生並存成 hex 檔。
//!
//! 預設位置是使用者資料目錄下的 `kist/client-id`（Linux：`~/.local/share/kist/client-id`）。
//! 刻意不用 hostname：改機器名稱不該變成一個新的 client（會影響 GC 的活躍判定）。

use std::path::{Path, PathBuf};

use crate::{AppError, Result};

pub fn load_or_create(explicit: Option<&Path>) -> Result<[u8; 16]> {
    let path = resolve_path(explicit)?;
    if path.is_file() {
        let text = std::fs::read_to_string(&path).map_err(|e| io(&path, e))?;
        let bytes = hex::decode(text.trim()).map_err(|e| {
            AppError::Other(format!(
                "client id file {} is not valid hex: {e}",
                path.display()
            ))
        })?;
        let id: [u8; 16] = bytes.try_into().map_err(|_| {
            AppError::Other(format!(
                "client id file {} must hold 16 bytes",
                path.display()
            ))
        })?;
        return Ok(id);
    }
    let id = kist_crypto::random_bytes::<16>().map_err(|e| AppError::Other(e.to_string()))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| io(parent, e))?;
    }
    std::fs::write(&path, format!("{}\n", hex::encode(id))).map_err(|e| io(&path, e))?;
    Ok(id)
}

/// 同一個 client id 一次只能跑一個 backup：GC 的「活躍 client 在標記後有新 snapshot」
/// 這條保護假設每台 client 的 backup 是一個接一個的；排程重疊會破壞它。
/// 鎖是 client id 檔旁邊的 `client-id.lock`，程序結束（或 drop）自動釋放。
pub fn lock(explicit: Option<&Path>) -> Result<std::fs::File> {
    let path = resolve_path(explicit)?.with_extension("lock");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| io(parent, e))?;
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .map_err(|e| io(&path, e))?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(std::fs::TryLockError::WouldBlock) => Err(AppError::Other(format!(
            "another kist backup is already running for this client id (lock file {})",
            path.display()
        ))),
        Err(std::fs::TryLockError::Error(e)) => Err(io(&path, e)),
    }
}

fn resolve_path(explicit: Option<&Path>) -> Result<PathBuf> {
    match explicit {
        Some(p) => Ok(p.to_path_buf()),
        None => default_path(),
    }
}

fn default_path() -> Result<PathBuf> {
    let Some(base) = dirs::data_local_dir() else {
        return Err(AppError::Other(
            "cannot determine the local data directory; pass --client-id-file".to_owned(),
        ));
    };
    Ok(base.join("kist").join("client-id"))
}

fn io(path: &Path, source: std::io::Error) -> AppError {
    AppError::Io {
        path: path.to_path_buf(),
        source,
    }
}

pub fn hostname() -> String {
    let name = gethostname::gethostname()
        .to_string_lossy()
        .trim()
        .to_owned();
    if name.is_empty() {
        "unknown".to_owned()
    } else {
        name
    }
}

pub fn username() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "unknown".to_owned())
}
