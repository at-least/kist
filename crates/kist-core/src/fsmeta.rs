//! 檔案系統 metadata 的擷取與還原，平台差異都關在這裡。
//!
//! - 檔名：Unix 用原始 OS bytes；Windows 用 UTF-8（無法轉成 Unicode 的檔名回錯）。
//! - mode / uid / gid：Unix 擷取；Windows 存 0。還原時套 mode；以 root（euid 0）還原時先把
//!   uid/gid 設回記錄的值再套 mode，設不回去就清掉 setuid/setgid（ADR 019 A43）。非 root
//!   不 chown：還原出的檔屬於還原者本人。
//! - mtime：兩邊都做，奈秒精度。

use std::ffi::{OsStr, OsString};
use std::fs::File;
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
/// `host/path`（docs/format.md §9 的 restore 映射）。
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
            // NUL 不是路徑元件：Unix 的 bytes_to_name 什麼
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

/// entry 記錄的擁有者 (uid, gid)，restore 用（ADR 019 A43）。不走
/// [`meta_of_entry`]：那裡把缺席當 0，而 0 是 root。uid 與 gid 都有才算——
/// 只有一個（sftp 的選填欄位）當成沒記錄：只改 uid 會留下 root 的 gid，
/// setgid 指向 gid 0 一樣是提權。
pub fn owner_of_entry(entry: &Entry) -> Option<(u32, u32)> {
    match (entry.uid, entry.gid) {
        (Some(uid), Some(gid)) => Some((uid, gid)),
        _ => None,
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

/// 擷取 `user.*` 擴充屬性。
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

/// 還原延伸屬性，經已開的 `file`（fsetxattr；`path` 只用在錯誤訊息）。只套
/// `user.` namespace（與記錄端同一道防線：惡意 repo 不能
/// 指揮我們寫 security./trusted. 之類需要特權的 namespace）。任何一顆失敗
/// （檔案系統不支援、權限）就整節點回錯——與 times/mode 的 policy 一致：
/// metadata 丢了就是錯，其他檔案繼續。
#[cfg(unix)]
pub fn apply_xattrs(
    file: &File,
    path: &Path,
    xattrs: Option<&std::collections::BTreeMap<serde_bytes::ByteBuf, serde_bytes::ByteBuf>>,
) -> Result<()> {
    use std::os::unix::ffi::OsStrExt;
    use xattr::FileExt;
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
        file.set_xattr(name, value.as_ref())
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
    _: &File,
    _: &Path,
    _: Option<&std::collections::BTreeMap<serde_bytes::ByteBuf, serde_bytes::ByteBuf>>,
) -> Result<()> {
    Ok(())
}

/// 記錄的奈秒 mtime → filetime 的時間（1970 年前的負值照實）。symlink 本身的
/// 時間在 restore 那一層的目錄 handle 上設（dirhandle.rs，ADR 019 A4）。
pub(crate) fn file_time(mtime_ns: i64) -> filetime::FileTime {
    filetime::FileTime::from_unix_time(
        mtime_ns.div_euclid(1_000_000_000),
        mtime_ns.rem_euclid(1_000_000_000) as u32,
    )
}

/// 還原 mtime（atime 設成同一個值）與 mode（Unix），一律經已開的 `file`
/// （`path` 只用在錯誤訊息）。以路徑套用的話，路徑在內容寫完之後被換成
/// symlink，時間與 mode（含 setuid／setgid 位）就套到連結指向的檔上
/// （ADR 019 A3）。呼叫端先套 xattr（[`apply_xattrs`]）再呼叫這裡：記錄的
/// mode 可能是唯讀，之後 user.* 就設不進去。擁有者（[`apply_owner`]）也在
/// 這裡之前：chown 會清掉 setuid／setgid；`meta.mode` 由呼叫端先經
/// [`mode_to_apply`] 決定（ADR 019 A43）。
///
/// 先時間後 mode。以前以路徑設時間時這個順序是必要的：filetime 0.2.29 的
/// `set_file_times` 會先開檔再 futimens，mode 已是 0o000 就開不起來。經 handle
/// 設指定的時間只要求是擁有者、不看 mode，順序照舊只為了不意外。
pub fn apply(file: &File, path: &Path, meta: &FsMeta) -> Result<()> {
    // 與 filetime 的 set_file_times 同一個換算（1970 年前的負值照實）。
    let mtime = std::time::SystemTime::from(file_time(meta.mtime_ns));
    let times = std::fs::FileTimes::new()
        .set_accessed(mtime)
        .set_modified(mtime);
    file.set_times(times).map_err(|e| CoreError::io(path, e))?;
    apply_mode(file, path, meta.mode)
}

#[cfg(unix)]
fn apply_mode(file: &File, path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    if mode == 0 {
        return Ok(()); // 來自沒有 mode 的平台
    }
    file.set_permissions(std::fs::Permissions::from_mode(mode & 0o7777))
        .map_err(|e| CoreError::io(path, e))
}

#[cfg(not(unix))]
fn apply_mode(_: &File, _: &Path, _: u32) -> Result<()> {
    Ok(())
}

/// 這個 process 是不是以 root（euid 0）在跑：只有 root 會還原擁有者
/// （ADR 019 A43）。
#[cfg(unix)]
pub fn running_as_root() -> bool {
    rustix::process::geteuid().is_root()
}

/// 非 unix 不還原擁有者（備份端也存 0）。
#[cfg(not(unix))]
pub fn running_as_root() -> bool {
    false
}

/// 還原時要套的 mode（ADR 019 A43）。以 root 還原、擁有者卻沒設回記錄的值
/// （entry 沒記錄 uid／gid，或 chown 失敗）時清掉 setuid／setgid（0o6000）：
/// 檔案還是 root 的，照套會把別人的 4755 變成 root 擁有的 setuid 檔。其餘照
/// 記錄的值：root 已把擁有者設回去；非 root 還原出的檔屬於還原者本人，setuid
/// 指向自己不構成提權。sticky（0o1000）與其餘位元不動。
pub fn mode_to_apply(recorded: u32, as_root: bool, owner_restored: bool) -> u32 {
    if as_root && !owner_restored {
        recorded & !0o6000
    } else {
        recorded
    }
}

/// ADR 019 A43：以 root 還原時，把擁有者設回記錄的 `owner`（uid, gid），經
/// 已開的 `file`（fchown；`path` 只用在錯誤訊息）。回傳擁有者是否已設回：
/// 非 root、或 entry 沒記錄擁有者，就不做 syscall、回 `false`。要在套 mode
/// **之前**呼叫（與已移除的 Go restore.go 同順序）：chown 會清掉 setuid／
/// setgid，先套 mode 的話記錄的 4755 會悄悄變成 755。
#[cfg(unix)]
pub fn apply_owner(
    file: &File,
    path: &Path,
    owner: Option<(u32, u32)>,
    as_root: bool,
) -> Result<bool> {
    if !as_root {
        return Ok(false);
    }
    let Some((uid, gid)) = owner else {
        return Ok(false);
    };
    check_owner_ids(uid, gid)
        .and_then(|()| std::os::unix::fs::fchown(file, Some(uid), Some(gid)))
        .map_err(|e| owner_error(path, uid, gid, e))?;
    Ok(true)
}

/// 非 unix 不還原擁有者。
#[cfg(not(unix))]
pub fn apply_owner(_: &File, _: &Path, _: Option<(u32, u32)>, _: bool) -> Result<bool> {
    Ok(false)
}

/// chown 的 uid／gid 是 -1（`u32::MAX`）代表「不改」：照傳的話 chown 會成功、
/// 擁有者其實還是 root，接著套完整的 mode 正是 A43 要擋的事。當成還原失敗。
#[cfg(unix)]
pub(crate) fn check_owner_ids(uid: u32, gid: u32) -> std::io::Result<()> {
    if uid == u32::MAX || gid == u32::MAX {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "id 4294967295 means \"unchanged\" to chown",
        ));
    }
    Ok(())
}

/// 擁有者設不回去的錯誤（記進該節點）：訊息帶記錄的 uid／gid。
pub(crate) fn owner_error(path: &Path, uid: u32, gid: u32, e: std::io::Error) -> CoreError {
    let kind = e.kind();
    CoreError::io(
        path,
        std::io::Error::new(kind, format!("restoring owner uid {uid} gid {gid}: {e}")),
    )
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

/// ADR 019 A43：以 root 還原時先還原擁有者、再套 mode。這裡沒有 root，只驗
/// 純函式與非 root 能走到的路徑；root 真的 chown 成別人的行為 UNVERIFIED。
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod owner_tests {
    use super::mode_to_apply;

    /// (記錄的 mode, 以 root 還原, 擁有者已還原) → 要套的 mode。
    #[test]
    fn setid_bits_are_cleared_only_for_root_without_the_recorded_owner() {
        let cases = [
            // root、擁有者已設回記錄的值：完整照套（含 setuid／setgid／sticky）。
            (0o104755, true, true, 0o104755),
            (0o7777, true, true, 0o7777),
            (0o42755, true, true, 0o42755),
            // root、擁有者沒還原（沒記錄 uid／gid，或 chown 失敗）：清掉 0o6000，
            // sticky 與其餘權限、檔案類型位不動。
            (0o104755, true, false, 0o100755),
            (0o7777, true, false, 0o1777),
            (0o42755, true, false, 0o40755),
            (0o1777, true, false, 0o1777),
            // 非 root：維持現狀（檔案屬於還原者本人，setuid 指向自己）。
            (0o104755, false, false, 0o104755),
            (0o7777, false, false, 0o7777),
            (0o104755, false, true, 0o104755),
            // 0 = 來自沒有 mode 的平台，照舊不套（apply_mode 看到 0 就略過）。
            (0, true, false, 0),
            (0, false, false, 0),
        ];
        for (recorded, as_root, owned, want) in cases {
            assert_eq!(
                mode_to_apply(recorded, as_root, owned),
                want,
                "recorded {recorded:o}, as_root {as_root}, owned {owned}"
            );
        }
    }

    /// apply_owner 在非 root 也能驗的部分：沒要求（非 root、沒記錄擁有者）就不做
    /// syscall；-1 的 id 直接拒絕（chown 把它當「不改」，照傳會以為成功）；
    /// chown 成自己一定成功；chown 成別人（非 root）失敗並帶上記錄的 id。
    #[cfg(unix)]
    #[test]
    fn apply_owner_only_acts_when_asked_and_reports_failures() {
        use std::os::unix::fs::MetadataExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f");
        let file = std::fs::File::create(&path).unwrap();
        let meta = file.metadata().unwrap();
        let (own_uid, own_gid) = (meta.uid(), meta.gid());
        let other = if own_uid == 4242 { 4243 } else { 4242 };
        let owner_of = |f: &std::fs::File| {
            let m = f.metadata().unwrap();
            (m.uid(), m.gid())
        };

        // 非 root：記錄的是別人也不動、不回錯。
        let got = super::apply_owner(&file, &path, Some((other, other)), false).unwrap();
        assert!(!got);
        // 沒記錄擁有者：root 也不動。
        let got = super::apply_owner(&file, &path, None, true).unwrap();
        assert!(!got);
        assert_eq!(owner_of(&file), (own_uid, own_gid));

        // -1：不下 syscall，回錯。
        for (uid, gid) in [(u32::MAX, own_gid), (own_uid, u32::MAX)] {
            let err = super::apply_owner(&file, &path, Some((uid, gid)), true).unwrap_err();
            assert!(
                err.to_string()
                    .contains(&format!("restoring owner uid {uid} gid {gid}")),
                "{err}"
            );
        }

        // chown 成自己：真的呼叫 fchown，成功。
        let got = super::apply_owner(&file, &path, Some((own_uid, own_gid)), true).unwrap();
        assert!(got);
        assert_eq!(owner_of(&file), (own_uid, own_gid));

        // chown 成別人：非 root 會 EPERM（以 root 跑這條就會成功，略過）。
        if own_uid != 0 {
            let err = super::apply_owner(&file, &path, Some((other, other)), true).unwrap_err();
            assert!(
                err.to_string()
                    .contains(&format!("restoring owner uid {other} gid {other}")),
                "{err}"
            );
            assert_eq!(owner_of(&file), (own_uid, own_gid));
        }
    }
}
