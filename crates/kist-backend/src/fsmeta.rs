//! 來源走訪用的 POSIX metadata 擷取與檔名 bytes 轉換（kist-core::fsmeta 的
//! backend 側最小版——core 依賴 backend，不能反向）。
//!
//! - 檔名：Unix 用原始 OS bytes；Windows 用 UTF-8（無法轉換回錯）。
//! - mode/uid/gid/ctime/inode/dev/nlink：Unix 擷取；其他平台 = 0。

use std::ffi::{OsStr, OsString};

/// 本機檔案系統擷取的 POSIX metadata（快速路徑用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PosixMeta {
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub mtime_ns: i64,
    /// 0 = 平台沒有，不拿來比對。
    pub ctime_ns: i64,
    /// 0 = 平台沒有。
    pub inode: u64,
    pub dev: u64,
    pub nlink: u64,
}

pub fn capture(meta: &std::fs::Metadata) -> PosixMeta {
    PosixMeta {
        mode: mode_of(meta),
        uid: uid_of(meta),
        gid: gid_of(meta),
        mtime_ns: mtime_ns_of(meta),
        ctime_ns: ctime_ns_of(meta),
        inode: inode_of(meta),
        dev: dev_of(meta),
        nlink: nlink_of(meta),
    }
}

pub fn path_to_bytes(path: &std::path::Path) -> Result<Vec<u8>, OsString> {
    name_to_bytes(path.as_os_str()).ok_or_else(|| path.as_os_str().to_owned())
}

pub fn name_to_bytes(name: &OsStr) -> Option<Vec<u8>> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        Some(name.as_bytes().to_vec())
    }
    #[cfg(not(unix))]
    {
        name.to_str().map(|s| s.as_bytes().to_vec())
    }
}

pub fn bytes_to_name(bytes: &[u8]) -> Result<OsString, String> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        Ok(OsString::from_vec(bytes.to_vec()))
    }
    #[cfg(not(unix))]
    {
        String::from_utf8(bytes.to_vec())
            .map(OsString::from)
            .map_err(|_| String::from_utf8_lossy(bytes).into_owned())
    }
}

/// bytes → OS 名稱；無法轉換（Windows 非 UTF-8）時 panic-free 地 lossy。
/// 只用於來源路徑拼接——名稱來自來源列目，與 bytes_to_name 的差別在這裡
/// 不會失敗（失敗也無處回報）。
pub fn bytes_to_os(bytes: &[u8]) -> OsString {
    bytes_to_name(bytes)
        .unwrap_or_else(|_| OsString::from(String::from_utf8_lossy(bytes).into_owned()))
}

pub fn os_to_bytes(name: OsString) -> Vec<u8> {
    name_to_bytes(&name).unwrap_or_else(|| name.to_string_lossy().into_owned().into_bytes())
}

fn mtime_ns_of(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .and_then(|d| i64::try_from(d.as_nanos()).ok())
        .unwrap_or(0)
}

#[cfg(unix)]
fn ctime_ns_of(meta: &std::fs::Metadata) -> i64 {
    use std::os::unix::fs::MetadataExt;
    meta.ctime().saturating_mul(1_000_000_000) + meta.ctime_nsec()
}

#[cfg(not(unix))]
fn ctime_ns_of(_: &std::fs::Metadata) -> i64 {
    0
}

#[cfg(unix)]
fn dev_of(meta: &std::fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    meta.dev()
}

#[cfg(not(unix))]
fn dev_of(_: &std::fs::Metadata) -> u64 {
    0
}

#[cfg(unix)]
fn nlink_of(meta: &std::fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    meta.nlink()
}

#[cfg(not(unix))]
fn nlink_of(_: &std::fs::Metadata) -> u64 {
    0
}

#[cfg(unix)]
fn inode_of(meta: &std::fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    meta.ino()
}

#[cfg(not(unix))]
fn inode_of(_: &std::fs::Metadata) -> u64 {
    0
}

#[cfg(unix)]
fn mode_of(meta: &std::fs::Metadata) -> u32 {
    use std::os::unix::fs::MetadataExt;
    meta.mode()
}

#[cfg(not(unix))]
fn mode_of(_: &std::fs::Metadata) -> u32 {
    0
}

#[cfg(unix)]
fn uid_of(meta: &std::fs::Metadata) -> u32 {
    use std::os::unix::fs::MetadataExt;
    meta.uid()
}

#[cfg(not(unix))]
fn uid_of(_: &std::fs::Metadata) -> u32 {
    0
}

#[cfg(unix)]
fn gid_of(meta: &std::fs::Metadata) -> u32 {
    use std::os::unix::fs::MetadataExt;
    meta.gid()
}

#[cfg(not(unix))]
fn gid_of(_: &std::fs::Metadata) -> u32 {
    0
}
