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

#[cfg(unix)]
pub fn capture(meta: &std::fs::Metadata) -> PosixMeta {
    use std::os::unix::fs::MetadataExt;
    PosixMeta {
        mode: meta.mode(),
        uid: meta.uid(),
        gid: meta.gid(),
        mtime_ns: mtime_ns_of(meta),
        ctime_ns: meta.ctime().saturating_mul(1_000_000_000) + meta.ctime_nsec(),
        inode: meta.ino(),
        dev: meta.dev(),
        nlink: meta.nlink(),
    }
}

#[cfg(not(unix))]
pub fn capture(meta: &std::fs::Metadata) -> PosixMeta {
    PosixMeta {
        mode: 0,
        uid: 0,
        gid: 0,
        mtime_ns: mtime_ns_of(meta),
        ctime_ns: 0,
        inode: 0,
        dev: 0,
        nlink: 0,
    }
}

pub fn path_to_bytes(path: &std::path::Path) -> Option<Vec<u8>> {
    name_to_bytes(path.as_os_str())
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

/// bytes → OS 名稱；無法轉換（Windows 非 UTF-8）時 panic-free 地 lossy。
/// 只用於來源路徑拼接——名稱來自來源列目，這裡不會失敗（失敗也無處回報）。
pub fn bytes_to_os(bytes: &[u8]) -> OsString {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        OsString::from_vec(bytes.to_vec())
    }
    #[cfg(not(unix))]
    {
        OsString::from(String::from_utf8_lossy(bytes).into_owned())
    }
}

pub fn os_to_bytes(name: OsString) -> Vec<u8> {
    name_to_bytes(&name).unwrap_or_else(|| name.to_string_lossy().into_owned().into_bytes())
}

pub(crate) fn mtime_ns_of(meta: &std::fs::Metadata) -> i64 {
    // mtime 取不到才用 0（= 未記錄）；取得到就要保留全範圍——1970 年以前的
    // mtime 是合法的（解壓縮打包檔常見），夾成 0 會讓 restore 靜靜把時間
    // 設成 1970-01-01。
    let Ok(t) = meta.modified() else {
        return 0;
    };
    match t.duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => i64::try_from(d.as_nanos()).unwrap_or(i64::MAX),
        Err(e) => match i64::try_from(e.duration().as_nanos()) {
            Ok(ns) => -ns,
            Err(_) => i64::MIN,
        },
    }
}
