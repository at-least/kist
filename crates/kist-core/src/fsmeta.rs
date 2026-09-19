//! 檔案系統 metadata 的擷取與還原，平台差異都關在這裡。
//!
//! - 檔名：Unix 用原始 OS bytes；Windows 用 UTF-8（無法轉成 Unicode 的檔名回錯）。
//! - mode / uid / gid：Unix 擷取；Windows 存 0。還原時只還原 mode（uid/gid 需要 root，M1 不做）。
//! - mtime：兩邊都做，奈秒精度。

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use kist_format::tree::Entry;

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
        .map_err(|_| {
            CoreError::BadFileName(PathBuf::from(String::from_utf8_lossy(bytes).into_owned()))
        })
}

/// 整條路徑 → bytes（snapshot 的 `paths` 與根節點名稱用）。
pub fn path_to_bytes(path: &Path) -> Result<Vec<u8>> {
    name_to_bytes(path.as_os_str())
}

/// bytes → 路徑，並去掉根（`/` 或 `C:\`），讓它可以接在 restore 目標底下。
pub fn bytes_to_relative_path(bytes: &[u8]) -> Result<PathBuf> {
    locator_to_relative(bytes)
}

/// v3 的 root 定位字串（`Root.path`）→ restore 目標底下的相對路徑。
/// 本機絕對路徑 `/srv/data` → `srv/data`；帶 scheme 的遠端定位去掉 scheme
/// 後切段：`s3://bucket/prefix` → `bucket/prefix`、`sftp://host/path` →
/// `host/path`（format-v3-draft §9 的 restore 映射，兩實作必須一致）。
pub fn locator_to_relative(bytes: &[u8]) -> Result<PathBuf> {
    // 去掉 `scheme://`（有 scheme 且後接 // 才剝；Windows 的 `C:` 不會中）。
    let rest = match bytes.iter().position(|&b| b == b':') {
        Some(i) if bytes.len() >= i + 3 && &bytes[i + 1..i + 3] == b"//" => &bytes[i + 3..],
        _ => bytes,
    };
    let mut rel = PathBuf::new();
    for comp in rest.split(|&b| b == b'/') {
        match comp {
            b"" | b"." => {}
            b".." => rel.push("__parent__"),
            // NUL 不是路徑元件（Go 端同判斷）：Unix 的 bytes_to_name 什麼
            // 都收，得在這裡擋。
            name if name.contains(&0) => {
                return Err(crate::CoreError::Corrupt {
                    key: "<locator>".to_owned(),
                    reason: format!(
                        "locator component {:?} is not a path component",
                        String::from_utf8_lossy(name)
                    ),
                })
            }
            name => rel.push(bytes_to_name(name)?),
        }
    }
    Ok(rel)
}

/// restore 用：tree 裡的子節點名稱來自 repo 內容，必須是「單一路徑元件」。
/// 空字串、`.`、`..`、含分隔符或 NUL 的名稱都可能讓 restore 寫到目標目錄之外。
pub fn validate_child_name(name: &[u8]) -> Result<()> {
    let bad = |why: &str| {
        Err(CoreError::Corrupt {
            key: format!("tree entry {:?}", String::from_utf8_lossy(name)),
            reason: format!("invalid file name: {why}"),
        })
    };
    if name.is_empty() {
        return bad("empty");
    }
    if name == b"." || name == b".." {
        return bad("directory reference");
    }
    if name.contains(&b'/') {
        return bad("contains '/'");
    }
    if name.contains(&0) {
        return bad("contains NUL");
    }
    if cfg!(windows) && name.contains(&b'\\') {
        return bad("contains '\\'");
    }
    Ok(())
}

/// 走訪當下擷取的 metadata（v2：時間是單一 i64 奈秒欄位，另含硬連結識別）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FsMeta {
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub mtime_ns: i64,
    /// inode 變更時間（奈秒）。0 = 平台沒有，不拿來比對。
    pub ctime_ns: i64,
    /// 0 = 平台沒有，不拿來比對。
    pub inode: u64,
    /// 硬連結識別（nlink > 1 才有意義）。
    pub dev: u64,
    pub nlink: u64,
}

/// parent tree 裡的檔案 entry → FsMeta（快速路徑的比較用）。
/// v3 的 Entry 欄位是 Option：posix 來源必填 mode/uid/gid/mtime，缺席的
/// 選填欄位以 0 呈現（與「平台沒有」同一語意，不拿來比對）。
pub fn meta_of_entry(entry: &Entry) -> FsMeta {
    FsMeta {
        mode: entry.mode.unwrap_or(0),
        uid: entry.uid.unwrap_or(0),
        gid: entry.gid.unwrap_or(0),
        mtime_ns: entry.mtime_ns.unwrap_or(0),
        ctime_ns: entry.ctime_ns.unwrap_or(0),
        inode: entry.inode.unwrap_or(0),
        dev: entry.dev.unwrap_or(0),
        nlink: entry.nlink.unwrap_or(0),
    }
}

pub fn capture(meta: &std::fs::Metadata) -> FsMeta {
    let mtime = mtime_ns_of(meta);
    let ctime = ctime_ns_of(meta);
    FsMeta {
        mode: mode_of(meta),
        uid: uid_of(meta),
        gid: gid_of(meta),
        mtime_ns: mtime,
        ctime_ns: ctime,
        inode: inode_of(meta),
        dev: dev_of(meta),
        nlink: nlink_of(meta),
    }
}

/// backup 快速路徑的完整判斷：size 相同，且 [`unchanged`] 成立。
pub fn file_unchanged(
    previous: &FsMeta,
    previous_size: u64,
    now: &FsMeta,
    now_size: u64,
    parent_start_ns: i64,
) -> bool {
    previous_size == now_size && unchanged(previous, now, parent_start_ns)
}

/// backup 快速路徑：上一次記錄的 metadata 與現在的是否「看起來沒變」。
///
/// - mtime 一定比；ctime 與 inode 在上一次有記錄（非 0）時也要相同。
/// - 另外要求 mtime 與 ctime 都**早於** parent snapshot 的開始時間 `parent_start`（Unix 秒、奈秒）：
///   檔案若在上一次 backup 讀它的同一個時間刻度內又被改（"racily clean"），metadata 看起來
///   一樣但內容不同；這種檔案永遠重讀，直到它的時間戳明確早於某次 backup 的開始為止。
pub fn unchanged(previous: &FsMeta, now: &FsMeta, parent_start_ns: i64) -> bool {
    if previous.mtime_ns != now.mtime_ns {
        return false;
    }
    let has_ctime = previous.ctime_ns != 0;
    if has_ctime && previous.ctime_ns != now.ctime_ns {
        return false;
    }
    if previous.inode != 0 && previous.inode != now.inode {
        return false;
    }
    if now.mtime_ns >= parent_start_ns {
        return false;
    }
    if has_ctime && now.ctime_ns >= parent_start_ns {
        return false;
    }
    true
}

fn mtime_ns_of(meta: &std::fs::Metadata) -> i64 {
    let ft = filetime::FileTime::from_last_modification_time(meta);
    ft.unix_seconds()
        .saturating_mul(1_000_000_000)
        .saturating_add(i64::from(ft.nanoseconds()))
}

#[cfg(unix)]
fn ctime_ns_of(meta: &std::fs::Metadata) -> i64 {
    use std::os::unix::fs::MetadataExt;
    meta.ctime().saturating_mul(1_000_000_000) + meta.ctime_nsec()
}

#[cfg(unix)]
fn dev_of(meta: &std::fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    meta.dev()
}

#[cfg(unix)]
fn nlink_of(meta: &std::fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    meta.nlink()
}

#[cfg(not(unix))]
fn dev_of(_: &std::fs::Metadata) -> u64 {
    0
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
fn ctime_ns_of(_: &std::fs::Metadata) -> i64 {
    0
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

/// 擷取 `user.*` 擴充屬性（與 Go 端同一個 namespace 約定）。
/// 鍵與值都是 bytes：xattr 名稱不保證是 UTF-8。讀不到（不支援、無權限）
/// 一律視為沒有——xattr 記錄是盡力而為，不讓備份因此失敗。
#[cfg(unix)]
pub fn read_xattrs(
    path: &Path,
) -> Option<std::collections::BTreeMap<serde_bytes::ByteBuf, serde_bytes::ByteBuf>> {
    use std::collections::BTreeMap;
    use std::os::unix::ffi::OsStrExt;
    let names = xattr::list(path).ok()?;
    let mut out = BTreeMap::new();
    for name in names {
        if !name.as_bytes().starts_with(b"user.") {
            continue;
        }
        if let Ok(Some(value)) = xattr::get(path, &name) {
            out.insert(
                serde_bytes::ByteBuf::from(name.as_bytes().to_vec()),
                serde_bytes::ByteBuf::from(value),
            );
        }
    }
    (!out.is_empty()).then_some(out)
}

#[cfg(not(unix))]
pub fn read_xattrs(
    _: &Path,
) -> Option<std::collections::BTreeMap<serde_bytes::ByteBuf, serde_bytes::ByteBuf>> {
    None
}

/// 還原延伸屬性。只套 `user.` namespace（與記錄端同一道防線：惡意 repo 不能
/// 指揮我們寫 security./trusted. 之類需要特權的 namespace）。任何一顆失敗
/// （檔案系統不支援、權限）就整節點回錯——與 times/mode 的 policy 一致：
/// metadata 丢了就是錯，其他檔案繼續。
#[cfg(unix)]
pub fn apply_xattrs(
    path: &Path,
    xattrs: Option<&std::collections::BTreeMap<serde_bytes::ByteBuf, serde_bytes::ByteBuf>>,
) -> Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let Some(map) = xattrs else {
        return Ok(());
    };
    for (name, value) in map {
        if !name.starts_with(b"user.") {
            tracing::debug!(
                "skipping non-user xattr {} on {}",
                String::from_utf8_lossy(name),
                path.display()
            );
            continue;
        }
        let name = std::ffi::OsStr::from_bytes(name);
        xattr::set(path, name, value.as_ref())
            .map_err(|e| CoreError::io(path, e))
            .map_err(|e| CoreError::Corrupt {
                key: path.display().to_string(),
                reason: format!("setting xattr {}: {e}", name.to_string_lossy()),
            })?;
    }
    Ok(())
}

/// Windows：xattr 是 Unix 的 user.* namespace，沒有對應物（備份端也不記錄）。
#[cfg(not(unix))]
pub fn apply_xattrs(
    _: &Path,
    _: Option<&std::collections::BTreeMap<serde_bytes::ByteBuf, serde_bytes::ByteBuf>>,
) -> Result<()> {
    Ok(())
}

/// 還原 mode（Unix）與 mtime。symlink 只還原 mtime（且不跟隨連結）。
pub fn apply(path: &Path, meta: &FsMeta, is_symlink: bool) -> Result<()> {
    let mtime = filetime::FileTime::from_unix_time(
        meta.mtime_ns.div_euclid(1_000_000_000),
        meta.mtime_ns.rem_euclid(1_000_000_000) as u32,
    );
    if is_symlink {
        // 有些平台不支援設定 symlink 本身的時間；失敗不算錯。
        let _ = filetime::set_symlink_file_times(path, mtime, mtime);
        return Ok(());
    }
    // 先設時間再設 mode：`set_file_times` 走 utimensat（路徑），不需要打開檔案，
    // 所以 mode 是 0o000 的目錄也設得了；`set_file_mtime` 會先 open 檔案，對這種目錄會失敗。
    filetime::set_file_times(path, mtime, mtime).map_err(|e| CoreError::io(path, e))?;
    apply_mode(path, meta.mode)?;
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

#[cfg(all(test, unix))]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod xattr_tests {
    #[test]
    fn user_xattrs_are_captured_and_sorted() {
        let dir = std::env::temp_dir().join("kist-fsmeta-xattr-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("f.txt");
        std::fs::write(&path, b"x").unwrap();
        // 環境不支援 user.* xattr（某些掛載）就退回：記錄 None 是合法行為。
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        if xattr::set(&path, OsStr::from_bytes(b"user.b"), b"2").is_err() {
            let _ = std::fs::remove_file(&path);
            eprintln!("skipping: user.* xattrs not supported here");
            return;
        }
        xattr::set(&path, OsStr::from_bytes(b"user.a"), b"1").unwrap();
        let got = super::read_xattrs(&path).expect("xattrs present");
        let names: Vec<Vec<u8>> = got.keys().map(|k| k.to_vec()).collect();
        assert_eq!(names, vec![b"user.a".to_vec(), b"user.b".to_vec()]);
        assert_eq!(
            &got[&serde_bytes::ByteBuf::from(b"user.a".to_vec())][..],
            b"1"
        );
        let _ = std::fs::remove_file(&path);
    }
}
