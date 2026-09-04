//! 檔案系統 metadata 的擷取與還原，平台差異都關在這裡。
//!
//! - 檔名：Unix 用原始 OS bytes；Windows 用 UTF-8（無法轉成 Unicode 的檔名回錯）。
//! - mode / uid / gid：Unix 擷取；Windows 存 0。還原時只還原 mode（uid/gid 需要 root，M1 不做）。
//! - mtime：兩邊都做，奈秒精度。

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use kist_format::tree::NodeMeta;

use crate::{CoreError, Result};

#[cfg(unix)]
pub fn name_to_bytes(name: &OsStr) -> Result<Vec<u8>> {
    use std::os::unix::ffi::OsStrExt;
    Ok(name.as_bytes().to_vec())
}

#[cfg(not(unix))]
pub fn name_to_bytes(name: &OsStr) -> Result<Vec<u8>> {
    name.to_str()
        .map(|s| s.as_bytes().to_vec())
        .ok_or_else(|| CoreError::BadFileName(PathBuf::from(name)))
}

#[cfg(unix)]
pub fn bytes_to_name(bytes: &[u8]) -> Result<OsString> {
    use std::os::unix::ffi::OsStringExt;
    Ok(OsString::from_vec(bytes.to_vec()))
}

#[cfg(not(unix))]
pub fn bytes_to_name(bytes: &[u8]) -> Result<OsString> {
    String::from_utf8(bytes.to_vec())
        .map(OsString::from)
        .map_err(|_| CoreError::BadFileName(PathBuf::from(String::from_utf8_lossy(bytes).as_ref())))
}

/// 整條路徑 → bytes（snapshot 的 `paths` 與根節點名稱用）。
pub fn path_to_bytes(path: &Path) -> Result<Vec<u8>> {
    name_to_bytes(path.as_os_str())
}

/// bytes → 路徑，並去掉根（`/` 或 `C:\`），讓它可以接在 restore 目標底下。
pub fn bytes_to_relative_path(bytes: &[u8]) -> Result<PathBuf> {
    let full = PathBuf::from(bytes_to_name(bytes)?);
    let mut rel = PathBuf::new();
    for comp in full.components() {
        match comp {
            std::path::Component::Prefix(_) | std::path::Component::RootDir => {}
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => rel.push("__parent__"),
            std::path::Component::Normal(n) => rel.push(n),
        }
    }
    Ok(rel)
}

pub fn capture(meta: &std::fs::Metadata) -> NodeMeta {
    let (secs, nanos) = mtime_of(meta);
    NodeMeta {
        mode: mode_of(meta),
        uid: uid_of(meta),
        gid: gid_of(meta),
        mtime_secs: secs,
        mtime_nanos: nanos,
    }
}

/// 兩份 metadata 的 mtime 是否相同（backup 快速路徑用）。
pub fn same_mtime(a: &NodeMeta, meta: &std::fs::Metadata) -> bool {
    let (secs, nanos) = mtime_of(meta);
    a.mtime_secs == secs && a.mtime_nanos == nanos
}

fn mtime_of(meta: &std::fs::Metadata) -> (i64, u32) {
    let ft = filetime::FileTime::from_last_modification_time(meta);
    (ft.unix_seconds(), ft.nanoseconds())
}

#[cfg(unix)]
fn mode_of(meta: &std::fs::Metadata) -> u32 {
    use std::os::unix::fs::MetadataExt;
    meta.mode()
}

#[cfg(unix)]
fn uid_of(meta: &std::fs::Metadata) -> u32 {
    use std::os::unix::fs::MetadataExt;
    meta.uid()
}

#[cfg(unix)]
fn gid_of(meta: &std::fs::Metadata) -> u32 {
    use std::os::unix::fs::MetadataExt;
    meta.gid()
}

#[cfg(not(unix))]
fn mode_of(_: &std::fs::Metadata) -> u32 {
    0
}

#[cfg(not(unix))]
fn uid_of(_: &std::fs::Metadata) -> u32 {
    0
}

#[cfg(not(unix))]
fn gid_of(_: &std::fs::Metadata) -> u32 {
    0
}

/// 還原 mode（Unix）與 mtime。symlink 只還原 mtime（且不跟隨連結）。
pub fn apply(path: &Path, meta: &NodeMeta, is_symlink: bool) -> Result<()> {
    let mtime = filetime::FileTime::from_unix_time(meta.mtime_secs, meta.mtime_nanos);
    if is_symlink {
        // 有些平台不支援設定 symlink 本身的時間；失敗不算錯。
        let _ = filetime::set_symlink_file_times(path, mtime, mtime);
        return Ok(());
    }
    apply_mode(path, meta.mode)?;
    filetime::set_file_mtime(path, mtime).map_err(|e| CoreError::io(path, e))?;
    Ok(())
}

#[cfg(unix)]
fn apply_mode(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    if mode == 0 {
        return Ok(()); // 來自沒有 mode 的平台
    }
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode & 0o7777))
        .map_err(|e| CoreError::io(path, e))
}

#[cfg(not(unix))]
fn apply_mode(_: &Path, _: u32) -> Result<()> {
    Ok(())
}
