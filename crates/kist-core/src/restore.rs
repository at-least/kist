//! restore 的檔案系統那一層（ADR 019 的強化）：暫存檔加 rename、擋路的東西、目錄 handle、硬連結、唯讀目錄、
//! 部分還原的選取、metadata 的順序。走訪在 [`crate::v4::restore`]；這一層原本與 v3 的走訪共用，v3 移除時只留下
//! 它（與 filter、fsmeta 一樣不屬於某個格式版本）。

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use crate::dirhandle::{DirHandle, Kind};
use crate::filter::{FilterState, PathFilter};
use crate::fsmeta;
use crate::{CoreError, Result};

/// 部分還原（ADR 020）：兩組樣式都空時還原整個 snapshot。樣式比對的路徑是節點整份還原時
/// 在目標底下的位置（本機來源＝原本的絕對路徑），規則見 [`crate::filter`]。
#[derive(Debug, Clone, Default)]
pub struct RestoreOptions {
    /// 只還原比對到的節點與它們底下的一切；沿途的上層目錄照樣建立、套記錄的 metadata、
    /// 計進 `dirs`。沒有東西被選到的分支一個目錄也不建；底下不可能被選到的子樹連 tree
    /// 都不讀。空的＝全部。
    pub include: PathFilter,
    /// 不還原比對到的節點（目錄整棵不走）。與 `include` 同時比對到時，排除優先。
    pub exclude: PathFilter,
    /// 只還原 snapshot 裡這個目錄的內容，直接放進目標、不重建它上面的路徑（ADR 022，
    /// 同 restic 的 `snapshot:path`）。段由 [`subtree_components`] 切好；空的＝整份還原的
    /// 版面。它可以是某個 root、root 裡的目錄、或幾個 root 共同的上層。樣式照樣比對
    /// snapshot 裡的完整路徑。
    pub subtree: Vec<Vec<u8>>,
}

/// `restore SNAPSHOT:PATH` 的 PATH → [`RestoreOptions::subtree`] 的段。PATH 是 snapshot 裡的
/// 路徑（與樣式同一套，`/` 開頭）；重複與結尾的 `/` 略過，`.`／`..` 報錯（snapshot 的
/// 路徑沒有這種段）。`/` 本身是頂端，等於整份還原的版面。
pub fn subtree_components(path: &str) -> Result<Vec<Vec<u8>>> {
    let Some(rest) = path.strip_prefix('/') else {
        return Err(CoreError::Usage(format!(
            "{path:?}: a path in a snapshot starts with `/` (as in `kist snapshots`)"
        )));
    };
    rest.split('/')
        .filter(|c| !c.is_empty())
        .map(|c| match c {
            "." | ".." => Err(CoreError::Usage(format!(
                "{path:?}: `.` and `..` are not components of a path in a snapshot"
            ))),
            c => Ok(c.as_bytes().to_vec()),
        })
        .collect()
}

/// [`RestoreOptions::subtree`] 的顯示字串（錯誤訊息用）。
pub(crate) fn subtree_display(subtree: &[Vec<u8>]) -> String {
    let parts: Vec<_> = subtree.iter().map(|c| String::from_utf8_lossy(c)).collect();
    format!("/{}", parts.join("/"))
}

/// SNAPSHOT:PATH 指到的不是目錄。
pub(crate) fn not_a_directory(subtree: &[Vec<u8>]) -> CoreError {
    CoreError::Usage(format!(
        "{} is not a directory in this snapshot",
        subtree_display(subtree)
    ))
}

/// [`crate::v4::Repo::plan_restore`]（dry run）回報的節點種類。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Planned {
    Dir,
    File,
    Symlink,
}

/// restore 的結果：單一檔案失敗不會中止整個 restore，而是記在 `errors` 裡。
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct RestoreSummary {
    pub files: u64,
    pub dirs: u64,
    pub symlinks: u64,
    pub errors: Vec<String>,
    /// 沒有選到任何節點的 include 樣式（原文，照給的順序）：snapshot 裡比對不到，或比對
    /// 到的全被 exclude 排除（被排除的子樹不會走進去，裡面的比對也就不算）。多半是打錯字
    /// 或選錯 snapshot：CLI 對它們印警告、以結束碼 3 結束。
    pub unmatched_includes: Vec<String>,
}

/// 硬連結表：(dev, inode) → 第一個還原出來的名字相對於還原目標的路徑元件。
/// 記路徑、不記目錄 handle（ADR 019 A4）：大樹裡硬連結多，每筆開著一個 fd
/// 會把 fd 用光；後續名字到時再從目標的 handle 沿路徑逐層重新開
/// （[`link_to_first`]）。
pub(crate) type HardLinks = std::collections::HashMap<(u64, u64), Vec<OsString>>;

/// restore 套 metadata 與選取需要的 Entry 欄位（v4 的 Entry 的實作在 [`crate::v4::restore`]）。
pub(crate) trait RestoreEntry {
    fn name(&self) -> &[u8];
    fn is_dir(&self) -> bool;
    fn target(&self) -> &[u8];
    fn mode(&self) -> Option<u32>;
    fn mtime_ns(&self) -> Option<i64>;
    fn xattrs(
        &self,
    ) -> Option<&std::collections::BTreeMap<serde_bytes::ByteBuf, serde_bytes::ByteBuf>>;
    /// 記錄的擁有者（[`fsmeta::owner_of_entry`] 的規則：uid 與 gid 都有才算）。
    fn owner(&self) -> Option<(u32, u32)>;
    /// 套用用的 metadata（[`fsmeta::meta_of_entry`] 的規則：缺席的是 0）。
    fn fs_meta(&self) -> fsmeta::FsMeta;
}

/// 套用 metadata，一律經已開的 `file`（ADR 019 A3；`path` 只用在錯誤訊息）。
/// 順序：xattr → 擁有者 → 時間 → mode。xattr 在 mode 之前：記錄的 mode 可能是
/// 唯讀，之後 user.* 會設不進去（EACCES）。擁有者在 mode 之前（ADR 019 A43，
/// 與已移除的 Go restore.go 同順序）：chown 會清掉 setuid／setgid。擁有者只有
/// 以 root 還原時才設（[`fsmeta::apply_owner`]）；設不回去時 mode 照樣套，
/// 但清掉 0o6000（[`fsmeta::mode_to_apply`]）。
///
/// mtime **沒記錄**的 entry（§8 聯集裡 s3/generic 的缺席＝來源未知）不動目標的
/// 時間——設成 epoch 比不設更糟；這種 entry 也不會帶 mode 與 uid/gid（§8.1），
/// 所以時間與 mode 可以省。posix/sftp 的 mtime 必填，行為不變。
///
/// 回傳兩個結果：(xattr／時間／mode, 擁有者)。分開交回是因為 restore_dir 只在
/// 前者失敗（記錄的 mode 沒套上）時放回暫時加的寫入權；擁有者設不回去時 mode
/// 已經套上，不能再被蓋掉。
pub(crate) fn apply_meta(
    file: &std::fs::File,
    path: &Path,
    node: &impl RestoreEntry,
) -> (Result<()>, Result<()>) {
    if let Err(e) = fsmeta::apply_xattrs(file, path, node.xattrs()) {
        return (Err(e), Ok(()));
    }
    let as_root = fsmeta::running_as_root();
    let owner = fsmeta::apply_owner(file, path, node.owner(), as_root);
    let owner_restored = matches!(owner, Ok(true));
    let applied = if node.mtime_ns().is_none() {
        Ok(())
    } else {
        let mut meta = node.fs_meta();
        meta.mode = fsmeta::mode_to_apply(meta.mode, as_root, owner_restored);
        fsmeta::apply(file, path, &meta)
    };
    (applied, owner.map(|_| ()))
}

/// symlink 條目的 metadata，設在連結本身（不跟隨），經它所在那層的 handle
/// （ADR 019 A4）：擁有者（只有 root，ADR 019 A43）→ mtime；symlink 沒有 mode
/// 可套。mtime 沒記錄就不動，理由同 [`apply_meta`]；設不了 mtime 不算錯（有些
/// 平台不支援），擁有者設不回去算這個節點的錯。
pub(crate) fn apply_symlink_meta(
    dir: &DirHandle,
    name: &OsStr,
    node: &impl RestoreEntry,
) -> Result<()> {
    let owner = match node.owner() {
        Some((uid, gid)) if fsmeta::running_as_root() => dir
            .chown_symlink(name, uid, gid)
            .map_err(|e| fsmeta::owner_error(&dir.child_path(name), uid, gid, e)),
        _ => Ok(()),
    };
    if let Some(mtime_ns) = node.mtime_ns() {
        dir.set_symlink_mtime(name, mtime_ns);
    }
    owner
}

/// 還原用的暫存名（ADR 019 A2）：內容或硬連結先放在正式名同目錄的隱藏
/// 暫存名下，最後 [`TempPath::commit`] 才 rename 成正式名。在那之前任何一步
/// 失敗，drop 就把暫存名刪掉——正式名底下使用者原有的檔從頭到尾沒被碰過。
/// SIGINT 不會跑 drop，會留下 `.kist-restore-*`（README 有寫）。rename 與
/// 刪除都經那一層目錄的 handle（ADR 019 A4）。
pub(crate) struct TempPath {
    pub(crate) dir: DirHandle,
    pub(crate) name: OsString,
    pub(crate) committed: bool,
}

impl TempPath {
    /// rename 成同一層的正式名 `dest`。rename 前再看一次 `dest`
    /// （[`refuse_in_the_way`]）：擋路的 symlink 照舊拒絕；一般檔被取代（「既有
    /// 檔案會被覆寫」）。這次檢查與 rename 之間仍有視窗，輸掉的後果只是 rename
    /// 取代了那個 symlink 目錄項本身——rename 不跟隨 symlink，寫不到目標之外。
    pub(crate) fn commit(mut self, dest: &OsStr) -> Result<()> {
        refuse_in_the_way(&self.dir, dest)?;
        self.dir
            .rename(&self.name, dest)
            .map_err(|e| CoreError::io(self.dir.child_path(dest), e))?;
        self.committed = true;
        Ok(())
    }

    /// 暫存名的完整路徑（錯誤訊息與測試接縫用）。
    #[cfg(all(test, unix))]
    pub(crate) fn path(&self) -> PathBuf {
        self.dir.child_path(&self.name)
    }
}

impl Drop for TempPath {
    fn drop(&mut self) {
        if !self.committed {
            let _ = self.dir.remove_file(&self.name);
        }
    }
}

/// 新的隱藏暫存名 `.kist-restore-<16 位 hex>`（亂數直接向作業系統要，與
/// repo id 同一個來源）。只產生名字，不建檔。
pub(crate) fn temp_name() -> Result<OsString> {
    let random = hex::encode(kist_crypto::random_bytes::<8>()?);
    Ok(OsString::from(format!(".kist-restore-{random}")))
}

/// 寫著內容的暫存檔：寫入用的 handle 加上負責刪檔的 [`TempPath`]。`file`
/// 放在前面：drop 時先關檔、再刪暫存名。
pub(crate) struct TempFile {
    pub(crate) file: std::fs::File,
    pub(crate) temp: TempPath,
}

impl TempFile {
    /// 在 `dir` 這一層建一個新的暫存檔（[`DirHandle::create_new_file`]：
    /// O_CREAT|O_EXCL，unix 另加 O_NOFOLLOW——名字已被佔用就失敗，不會打開
    /// 別人放的東西），建檔權限 `mode`（[`temp_file_mode`]）。
    pub(crate) fn create_in(dir: &DirHandle, mode: u32) -> Result<Self> {
        let name = temp_name()?;
        // 開檔成功才交給 TempPath：失敗時那個名字不是我們的，drop 不能去刪它。
        let file = dir
            .create_new_file(&name, mode)
            .map_err(|e| CoreError::io(dir.child_path(&name), e))?;
        Ok(Self {
            file,
            temp: TempPath {
                dir: dir.clone(),
                name,
                committed: false,
            },
        })
    }
}

/// 記錄的 mode 會不會被套上：與 [`apply_meta`] 同條件（有 mtime、mode 不是 0）。
pub(crate) fn mode_will_be_applied(node: &impl RestoreEntry) -> bool {
    node.mtime_ns().is_some() && node.mode().is_some_and(|m| m != 0)
}

/// 暫存檔的建檔權限（unix 再經 umask；非 unix 沒有 mode，不看）。內容寫進
/// 暫存檔之後、記錄的 mode 套上之前，它不能比該有的寬鬆（ADR 019 A2 審查：
/// 以 umask 預設建的話，覆寫使用者 0o600 的檔時內容會先放在 0o644 的暫存檔，
/// metadata 套不上時就停在 0o644）：
/// - 記錄的 mode 之後會套上（條件與 [`apply_meta`] 相同：有 mtime、mode 不是 0，
///   否則 `fsmeta::apply_mode` 不動）→ 0o600。metadata 套不上時就停在 0o600。
///   不以記錄的 mode 建檔：唯讀的 mode 會讓之後的 user.* xattr 設不進去。
/// - 不會套 mode（s3／generic 來源）→ 正式名處原有一般檔的權限（以前原地寫時
///   就是保留它），沒有就是 0o666（與 std 新建的檔相同）。
pub(crate) fn temp_file_mode(
    dir: &DirHandle,
    name: &OsStr,
    node: &impl RestoreEntry,
) -> Result<u32> {
    if mode_will_be_applied(node) {
        return Ok(0o600);
    }
    Ok(dir.file_mode(name)?.unwrap_or(0o666))
}

/// 硬連結的第二個以後的名字：從還原目標的 handle 沿第一個名字的相對路徑
/// `first` 逐層重新開到它所在的那一層（O_NOFOLLOW；途中被換成 symlink 就開
/// 不起來，呼叫端退回複製），再 linkat 到 `dir` 的 `name`（ADR 019 A4）。
/// 正式名處已經有東西時 linkat 回 EEXIST（最常見：同一個 snapshot 再還原
/// 一次），改走 [`link_over`]。
pub(crate) fn link_to_first(
    target_dir: &DirHandle,
    first: &[OsString],
    dir: &DirHandle,
    name: &OsStr,
) -> Result<()> {
    let Some((first_name, first_parents)) = first.split_last() else {
        return Err(CoreError::Corrupt {
            key: dir.child_path(name).display().to_string(),
            reason: "empty hard-link source".to_owned(),
        });
    };
    let first_dir = target_dir.open_rel(first_parents)?;
    match first_dir.hard_link(first_name, dir, name) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            link_over(&first_dir, first_name, dir, name)
        }
        Err(e) => Err(CoreError::io(dir.child_path(name), e)),
    }
}

/// 硬連結的正式名處已經有東西：先以同一層的暫存名 link，再 rename 蓋過去：
/// 與一般檔同一套「暫存＋rename」（ADR 019 A2），擋路的 symlink 照樣拒絕，
/// 硬連結關係也保住——改走複製的話，第二次還原後兩個名字就各自獨立了。
fn link_over(
    first_dir: &DirHandle,
    first_name: &OsStr,
    dir: &DirHandle,
    name: &OsStr,
) -> Result<()> {
    // 兩個名字已經是同一個 inode（例如兩個 root 落在同一個位置，同一個名字
    // 還原兩次）：rename 對同一個 inode 的兩個名字什麼都不做、暫存名會留下
    // 來，所以先擋掉——已經是要的結果。
    if first_dir.same_file(first_name, dir, name) {
        return Ok(());
    }
    let temp = temp_name()?;
    // link 成功才交給 TempPath，理由同 TempFile::create_in。
    first_dir
        .hard_link(first_name, dir, &temp)
        .map_err(|e| CoreError::io(dir.child_path(&temp), e))?;
    TempPath {
        dir: dir.clone(),
        name: temp,
        committed: false,
    }
    .commit(name)
}

/// 正式檔名處已有的東西：沒有、或是一般檔（會被 rename 取代）→ 放行；
/// symlink、目錄、特殊檔（FIFO、socket、裝置）→ 拒絕，而且不刪它、不跟隨
/// （ADR 019 A2 裁定：擋路的 symlink 維持拒絕）。擋路的是本機的東西，不是
/// repo 損壞（ADR 019 A30）。
pub(crate) fn refuse_in_the_way(dir: &DirHandle, name: &OsStr) -> Result<()> {
    let what = match dir.kind_of(name)? {
        None | Some(Kind::File) => return Ok(()),
        Some(Kind::Symlink) => "a symlink",
        Some(Kind::Dir) => "a directory",
        Some(Kind::Special) => "a special file",
    };
    Err(CoreError::RestoreBlocked {
        path: dir.child_path(name),
        reason: format!("{what} is in the way of a restored file"),
    })
}

/// ADR 019 A2：還原到既有、擁有者不可寫的目錄（例如同一個 snapshot 先前還原
/// 出的 0o555）時，子項目的暫存檔建不進去。這個目錄記錄的 mode 會在子項目
/// 寫完後重新套上（[`apply_meta`]），所以先暫時加上擁有者寫入權，回傳原本的
/// 權限（套 metadata 失敗時放回去）。只對會被重新套 mode 的目錄呼叫（s3/generic
/// 來源沒有 mode 或 mtime 的不動，見 [`PendingDir::child`]）。fstat 與 chmod 都經
/// 往下走時開的那個 handle（ADR 019 A3、A4）；chmod 要能套 metadata 的 handle
/// （[`DirHandle::meta_handle`]），擁有者讀不了的目錄拿不到，照 chmod 失敗處理。
#[cfg(unix)]
fn make_owner_writable(dir: &DirHandle) -> Result<Option<std::fs::Permissions>> {
    use std::os::unix::fs::PermissionsExt;
    let original = dir.permissions()?;
    if original.mode() & 0o200 != 0 {
        return Ok(None);
    }
    let writable = std::fs::Permissions::from_mode((original.mode() & 0o7777) | 0o200);
    // chmod 失敗（例如目錄不是我們的、或讀不了）不擋整個目錄：群組或其他人的
    // 寫入權也許就夠；不夠的話，每個子項目各自回報寫不進去。
    let chmod = dir.meta_handle().and_then(|handle| {
        handle
            .set_permissions(writable)
            .map_err(|e| CoreError::io(dir.path(), e))
    });
    if let Err(e) = chmod {
        tracing::warn!("cannot make the existing directory writable for the restore: {e}");
        return Ok(None);
    }
    Ok(Some(original))
}

/// 非 unix 沒有 mode 可還原（fsmeta 的 apply_mode 是空的），不需要。
#[cfg(not(unix))]
fn make_owner_writable(_: &DirHandle) -> Result<Option<std::fs::Permissions>> {
    Ok(None)
}

/// 還原路徑上的一層目錄，第一次要往裡面寫東西時才建立。還原目標本身一開始就開著
/// （[`DirHandle`] 直接實作）；其他層是 [`PendingDir`]，沿 `parent` 一路接到目標。
/// 每層只開著自己的一個 handle，同時開著的數目仍是遞迴深度（ADR 019 A4）。
pub(crate) trait OpenDir {
    /// 這一層的 handle，準備好往裡面寫：還沒建就先建（缺的上層一起建）；第一次寫入
    /// 之前，既有的唯讀目錄暫時加上擁有者寫入權（[`make_owner_writable`]）。
    fn open(&mut self) -> Result<&DirHandle>;
    /// 這一層的完整路徑（錯誤訊息用；還沒建也算得出來）。
    fn location(&self) -> &Path;
}

impl OpenDir for DirHandle {
    fn open(&mut self) -> Result<&DirHandle> {
        Ok(self)
    }

    fn location(&self) -> &Path {
        self.path()
    }
}

/// 目標底下還沒建（或還沒開）的一層：root 的落點，或 snapshot 裡的一個目錄。
pub(crate) struct PendingDir<'p> {
    parent: &'p mut dyn OpenDir,
    make: Make,
    path: PathBuf,
    /// 這一層最後會套回記錄的 mode（[`mode_will_be_applied`]），所以既有的唯讀目錄
    /// 可以先暫時加寫入權。root 的落點沒有 metadata，不加。
    loosen: bool,
    pub(crate) handle: Option<DirHandle>,
    /// [`make_owner_writable`] 做過了（第一次往這層寫東西之前做一次）。
    prepared: bool,
    /// 暫時加上寫入權之前的權限：套 metadata 失敗時放回去。
    pub(crate) loosened: Option<std::fs::Permissions>,
    /// 上一次建立這一層失敗了（之後建起來就清掉）：root 的落點用它判斷整個 restore 要不要
    /// 回錯（[`crate::v4::Repo::restore`]）。
    pub(crate) create_failed: bool,
}

enum Make {
    /// root 的落點：上一層（還原目標）底下的相對路徑，逐段建立
    /// （[`DirHandle::create_dirs`]）。
    Base(PathBuf),
    /// snapshot 裡的目錄：上一層裡的這個名字。
    Child(OsString),
}

impl<'p> PendingDir<'p> {
    /// root 的落點 `rel`（目標底下）。
    pub(crate) fn base(parent: &'p mut dyn OpenDir, rel: &Path) -> Self {
        let path = parent.location().join(rel);
        Self::new(parent, Make::Base(rel.to_path_buf()), path, false)
    }

    /// snapshot 裡的目錄節點 `node`，名字 `name`。
    pub(crate) fn child(
        parent: &'p mut dyn OpenDir,
        name: &OsStr,
        node: &impl RestoreEntry,
    ) -> Self {
        let path = parent.location().join(name);
        let loosen = mode_will_be_applied(node);
        Self::new(parent, Make::Child(name.to_os_string()), path, loosen)
    }

    fn new(parent: &'p mut dyn OpenDir, make: Make, path: PathBuf, loosen: bool) -> Self {
        Self {
            parent,
            make,
            path,
            loosen,
            handle: None,
            prepared: false,
            loosened: None,
            create_failed: false,
        }
    }

    /// 建立這一層（建過就沿用），不做寫入前的準備。
    pub(crate) fn create(&mut self) -> Result<()> {
        self.ensure(false).map(|_| ())
    }

    fn ensure(&mut self, for_writing: bool) -> Result<&DirHandle> {
        let Self {
            parent,
            make,
            loosen,
            handle,
            prepared,
            loosened,
            create_failed,
            ..
        } = self;
        let handle = match handle {
            Some(handle) => &*handle,
            slot @ None => {
                let above = parent.open()?;
                let created = match make {
                    Make::Base(rel) => above.create_dirs(rel),
                    Make::Child(name) => above.create_child_dir(name),
                };
                *create_failed = created.is_err();
                &*slot.insert(created?)
            }
        };
        if for_writing && !*prepared {
            if *loosen {
                *loosened = make_owner_writable(handle)?;
            }
            // 成功之後才記：失敗時下一次寫入前再試（審查修正）。
            *prepared = true;
        }
        Ok(handle)
    }
}

impl OpenDir for PendingDir<'_> {
    fn open(&mut self) -> Result<&DirHandle> {
        self.ensure(true)
    }

    fn location(&self) -> &Path {
        &self.path
    }
}

/// 部分還原的選取（[`RestoreOptions`]）：每個節點的 [`Pos`] 從上一層往下走一步。
pub(crate) struct Select<'o> {
    include: &'o PathFilter,
    exclude: &'o PathFilter,
    /// 每條 include 樣式有沒有選到東西：涵蓋（比對到它或它的上層）某個被還原的節點
    /// （[`Select::credit`]、[`RestoreSummary::unmatched_includes`]）。
    hit: Vec<bool>,
}

/// 一個節點在兩組樣式裡的位置。
#[derive(Clone)]
pub(crate) struct Pos {
    include: FilterState,
    exclude: FilterState,
    /// 自己或某個上層被 include 比對到（沒有 include 時一律是）：要還原。
    pub(crate) selected: bool,
    /// 比對到自己或某個上層、還沒算成選到東西的正向 include 樣式。這個節點被還原時它們都
    /// 選到了東西（[`Select::credit`]）。
    pending: Vec<usize>,
}

impl<'o> Select<'o> {
    /// 只有反向樣式的 include 什麼都選不到：反向樣式只取消前面的 include 選到的（ADR
    /// 023）。直接報錯，不以「還原 0 個檔案」成功結束（審查修正）。
    pub(crate) fn new(opts: &'o RestoreOptions) -> Result<Self> {
        let include = &opts.include;
        if !include.is_empty() && (0..include.len()).all(|p| include.is_negated(p)) {
            return Err(CoreError::Usage(
                "every include pattern is negated: a `!` pattern only takes back what an \
                 earlier include selected, so nothing would be restored"
                    .to_owned(),
            ));
        }
        Ok(Self {
            include,
            exclude: &opts.exclude,
            hit: vec![false; include.len()],
        })
    }

    /// 路徑頂端（還沒走過任何元件）。
    pub(crate) fn start(&self) -> Pos {
        Pos {
            include: self.include.start(),
            exclude: self.exclude.start(),
            selected: self.include.is_empty(),
            pending: Vec::new(),
        }
    }

    /// `parent` 底下的 `name`；被 exclude 比對到 → `None`。
    pub(crate) fn child(&self, parent: &Pos, name: &[u8]) -> Option<Pos> {
        let exclude = self.exclude.descend(&parent.exclude, name);
        if exclude.is_match() {
            return None;
        }
        let include = self.include.descend(&parent.include, name);
        // 已經算過的不再帶著：被還原的節點底下，這串自己就空了。
        let mut pending: Vec<usize> = parent
            .pending
            .iter()
            .copied()
            .filter(|&p| !self.hit[p])
            .collect();
        for &p in include.matched() {
            if !self.include.is_negated(p) && !self.hit[p] && !pending.contains(&p) {
                pending.push(p);
            }
        }
        // 最後一個比對到這個節點的 include 樣式決定（反向的取消，ADR 023）；沒有樣式比對到
        // 它就沿用上一層：選到的目錄底下全部選到。
        Some(Pos {
            selected: include.decision().unwrap_or(parent.selected),
            include,
            exclude,
            pending,
        })
    }

    /// `pos` 被還原：比對到它或它上層的 include 樣式都選到了東西。只在這裡記，不在比對到
    /// 的時候記——比對到、但被後面的反向樣式取消，或比對到的上層底下全被排除，都不算
    /// （審查修正）。
    pub(crate) fn credit(&mut self, pos: &Pos) {
        if pos.selected {
            for &p in &pos.pending {
                self.hit[p] = true;
            }
        }
    }

    /// 從頂端沿 `names` 走到底；途中被 exclude 比對到 → `None`（那底下全部不還原）。
    pub(crate) fn walk<'n>(&self, names: impl IntoIterator<Item = &'n [u8]>) -> Option<Pos> {
        let mut pos = self.start();
        for name in names {
            pos = self.child(&pos, name)?;
        }
        Some(pos)
    }

    /// `parent` 底下的 `entry` 要不要走：被排除、或沒被選到而且底下也不可能有東西被
    /// 選到 → `None`。
    pub(crate) fn visit(&mut self, parent: &Pos, entry: &impl RestoreEntry) -> Option<Pos> {
        let pos = self.child(parent, entry.name())?;
        self.credit(&pos);
        (pos.selected || (entry.is_dir() && pos.include.may_match_below())).then_some(pos)
    }

    /// 這個節點底下還值得走嗎：自己被選到，或底下還可能有東西被選到。
    pub(crate) fn worth_walking(pos: &Pos) -> bool {
        pos.selected || pos.include.may_match_below()
    }

    /// 沒選到東西的 include 樣式。反向樣式不算：它們本來就只會取消。
    pub(crate) fn unmatched(&self) -> Vec<String> {
        (0..self.include.len())
            .filter(|&p| !self.hit[p] && !self.include.is_negated(p))
            .map(|p| self.include.pattern(p).to_owned())
            .collect()
    }
}

/// dry run（[`crate::v4::Repo::plan_restore`]）的走訪狀態。
pub(crate) struct Plan<'p, 'o> {
    pub(crate) select: Select<'o>,
    pub(crate) summary: RestoreSummary,
    pub(crate) on_node: &'p mut dyn FnMut(&Path, Planned),
    /// 目前路徑上的每一層，對應真的還原的 [`PendingDir`]。
    pub(crate) levels: Vec<PlanLevel>,
}

pub(crate) struct PlanLevel {
    pub(crate) path: PathBuf,
    /// snapshot 裡的目錄（root 的落點不是：真的還原不計數、這裡也不回報）。
    pub(crate) snapshot_dir: bool,
    pub(crate) created: bool,
}

impl Plan<'_, '_> {
    /// 要往最深一層寫東西：沿路還沒建的層算是建了，snapshot 裡的目錄依序回報（上層先）。
    /// 計數等那一層走完（v4 restore 的 `plan_dir`），與真的還原同一個時機。
    pub(crate) fn open(&mut self) {
        let Plan {
            levels, on_node, ..
        } = self;
        for level in levels.iter_mut().filter(|l| !l.created) {
            level.created = true;
            if level.snapshot_dir {
                on_node(&level.path, Planned::Dir);
            }
        }
    }
}

/// 還原 symlink 條目：建連結（[`replace_with_symlink`]），再套它本身的 metadata。
pub(crate) fn restore_symlink(
    dir: &DirHandle,
    name: &OsStr,
    node: &impl RestoreEntry,
    summary: &mut RestoreSummary,
) -> Result<()> {
    let link_target = fsmeta::bytes_to_name(node.target())?;
    replace_with_symlink(dir, name, Path::new(&link_target))?;
    summary.symlinks += 1;
    if node.xattrs().is_some() {
        tracing::warn!(
            "{}: snapshot has extended attributes for this symlink; \
             Linux cannot set user.* on a symlink and following it \
             would write to the target, so they are not restored",
            dir.child_path(name).display()
        );
    }
    apply_symlink_meta(dir, name, node)
}

/// 建 symlink 前先移除既有的檔案或 symlink（第二次 restore 到同一目錄）；既有的是
/// 目錄則回本機擋路的錯（ADR 019 A30）。都經 `dir` 這一層的 handle（ADR 019 A4）。
fn replace_with_symlink(dir: &DirHandle, name: &OsStr, link_target: &Path) -> Result<()> {
    match dir.kind_of(name) {
        Ok(Some(Kind::Dir)) => {
            return Err(CoreError::RestoreBlocked {
                path: dir.child_path(name),
                reason: "a directory is in the way of a symlink".to_owned(),
            });
        }
        Ok(Some(_)) => dir
            .remove_file(name)
            .map_err(|e| CoreError::io(dir.child_path(name), e))?,
        // 沒有、或看不到：直接建，建不起來由建 symlink 那一步回報（與以前相同）。
        Ok(None) | Err(_) => {}
    }
    create_symlink(dir, name, link_target)
}

#[cfg(unix)]
fn create_symlink(dir: &DirHandle, name: &OsStr, link_target: &Path) -> Result<()> {
    dir.symlink(link_target, name)
        .map_err(|e| CoreError::io(dir.child_path(name), e))
}

#[cfg(windows)]
fn create_symlink(dir: &DirHandle, name: &OsStr, link_target: &Path) -> Result<()> {
    // Windows 建 symlink 需要特權；失敗只警告，不讓整個 restore 中止。
    if let Err(e) = dir.symlink(link_target, name) {
        tracing::warn!(
            "{}: cannot create symlink: {e}",
            dir.child_path(name).display()
        );
    }
    Ok(())
}
