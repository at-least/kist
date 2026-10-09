//! restore：把一個 snapshot 還原到目標目錄。
//!
//! 目標目錄底下會重建完整的絕對路徑（`<target>/home/user/data/...`），
//! 這樣一個 snapshot 含多個來源路徑時不會互相覆蓋。

use std::ffi::{OsStr, OsString};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::{Mutex, RwLock};

use kist_format::snapshot::Root;
use kist_format::tree::{content_type, node_type, parse_chunk_list, Entry};
use kist_format::{keys, ChunkId, TreeId};

use crate::dirhandle::{DirHandle, Kind};
use crate::filter::{FilterState, PathFilter};
use crate::fsmeta;
use crate::index::{ChunkIndex, ChunkLocator};
use crate::pack::decode_chunk;
use crate::repo::Repository;
use crate::{blocking, CoreError, Result, MAX_TREE_DEPTH};

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
fn subtree_display(subtree: &[Vec<u8>]) -> String {
    let parts: Vec<_> = subtree.iter().map(|c| String::from_utf8_lossy(c)).collect();
    format!("/{}", parts.join("/"))
}

/// SNAPSHOT:PATH 指到的不是目錄。
fn not_a_directory(subtree: &[Vec<u8>]) -> CoreError {
    CoreError::Usage(format!(
        "{} is not a directory in this snapshot",
        subtree_display(subtree)
    ))
}

/// v3 的檔案/symlink 來源（docs/format.md §9）：root tree 恰好一個非目錄 entry、名稱＝
/// 定位的末段 `last`。
fn is_file_root(last: &[u8], entries: &[Entry]) -> bool {
    entries.len() == 1 && entries[0].kind != node_type::DIR && entries[0].name == last
}

/// [`Repository::plan_restore`]（dry run）回報的節點種類。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Planned {
    Dir,
    File,
    Symlink,
}

/// 讀取途中 chunk 的 pack 不見了（prune 的 repack 把它搬走了）就重新載入的 index。
/// 重載最多每分鐘一次：真的壞掉的 repo 不會每個 chunk 都重載一遍。
pub struct ReloadableIndex {
    index: RwLock<ChunkIndex>,
    last_reload: Mutex<Option<std::time::Instant>>,
}

impl ReloadableIndex {
    pub fn new(index: ChunkIndex) -> Self {
        Self {
            index: RwLock::new(index),
            last_reload: Mutex::new(None),
        }
    }

    pub async fn get(&self) -> tokio::sync::RwLockReadGuard<'_, ChunkIndex> {
        self.index.read().await
    }

    /// 直接換上新的 index（mount 看到新 snapshot 時的主動重載；限流在呼叫端）。
    pub async fn store(&self, index: ChunkIndex) {
        *self.index.write().await = index;
    }

    /// chunk 的明文長度；不在 index 時（prune 的 repack 搬走了、或 mount 之後才
    /// 出現的新 pack）限流重載一次再查。真的沒有 = `None`。
    pub async fn raw_len_reloading(&self, repo: &Repository, id: &ChunkId) -> Result<Option<u64>> {
        {
            let guard = self.index.read().await;
            if let Some(loc) = guard.get(id) {
                return Ok(Some(loc.raw_len));
            }
        }
        self.reload(repo).await?;
        let guard = self.index.read().await;
        Ok(guard.get(id).map(|loc| loc.raw_len))
    }

    async fn reload(&self, repo: &Repository) -> Result<()> {
        let mut last = self.last_reload.lock().await;
        let due = last.is_none_or(|t| t.elapsed() >= RELOAD_MIN_INTERVAL);
        if due {
            tracing::warn!(
                "a chunk or its pack is missing from the index; reloading the index \
                 (a repack may be in progress)"
            );
            let fresh = repo.load_index().await?;
            *self.index.write().await = fresh;
            *last = Some(std::time::Instant::now());
        }
        Ok(())
    }
}

const RELOAD_MIN_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);

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
type HardLinks = std::collections::HashMap<(u64, u64), Vec<OsString>>;

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
fn apply_meta(file: &std::fs::File, path: &Path, node: &Entry) -> (Result<()>, Result<()>) {
    if let Err(e) = fsmeta::apply_xattrs(file, path, node.xattrs.as_ref()) {
        return (Err(e), Ok(()));
    }
    let as_root = fsmeta::running_as_root();
    let owner = fsmeta::apply_owner(file, path, fsmeta::owner_of_entry(node), as_root);
    let owner_restored = matches!(owner, Ok(true));
    let applied = if node.mtime_ns.is_none() {
        Ok(())
    } else {
        let mut meta = fsmeta::meta_of_entry(node);
        meta.mode = fsmeta::mode_to_apply(meta.mode, as_root, owner_restored);
        fsmeta::apply(file, path, &meta)
    };
    (applied, owner.map(|_| ()))
}

/// symlink 條目的 metadata，設在連結本身（不跟隨），經它所在那層的 handle
/// （ADR 019 A4）：擁有者（只有 root，ADR 019 A43）→ mtime；symlink 沒有 mode
/// 可套。mtime 沒記錄就不動，理由同 [`apply_meta`]；設不了 mtime 不算錯（有些
/// 平台不支援），擁有者設不回去算這個節點的錯。
fn apply_symlink_meta(dir: &DirHandle, name: &OsStr, node: &Entry) -> Result<()> {
    let owner = match fsmeta::owner_of_entry(node) {
        Some((uid, gid)) if fsmeta::running_as_root() => dir
            .chown_symlink(name, uid, gid)
            .map_err(|e| fsmeta::owner_error(&dir.child_path(name), uid, gid, e)),
        _ => Ok(()),
    };
    if let Some(mtime_ns) = node.mtime_ns {
        dir.set_symlink_mtime(name, mtime_ns);
    }
    owner
}

/// 還原用的暫存名（ADR 019 A2）：內容或硬連結先放在正式名同目錄的隱藏
/// 暫存名下，最後 [`TempPath::commit`] 才 rename 成正式名。在那之前任何一步
/// 失敗，drop 就把暫存名刪掉——正式名底下使用者原有的檔從頭到尾沒被碰過。
/// SIGINT 不會跑 drop，會留下 `.kist-restore-*`（README 有寫）。rename 與
/// 刪除都經那一層目錄的 handle（ADR 019 A4）。
struct TempPath {
    dir: DirHandle,
    name: OsString,
    committed: bool,
}

impl TempPath {
    /// rename 成同一層的正式名 `dest`。rename 前再看一次 `dest`
    /// （[`refuse_in_the_way`]）：擋路的 symlink 照舊拒絕；一般檔被取代（「既有
    /// 檔案會被覆寫」）。這次檢查與 rename 之間仍有視窗，輸掉的後果只是 rename
    /// 取代了那個 symlink 目錄項本身——rename 不跟隨 symlink，寫不到目標之外。
    fn commit(mut self, dest: &OsStr) -> Result<()> {
        refuse_in_the_way(&self.dir, dest)?;
        self.dir
            .rename(&self.name, dest)
            .map_err(|e| CoreError::io(self.dir.child_path(dest), e))?;
        self.committed = true;
        Ok(())
    }

    /// 暫存名的完整路徑（錯誤訊息與測試接縫用）。
    #[cfg(all(test, unix))]
    fn path(&self) -> PathBuf {
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
fn temp_name() -> Result<OsString> {
    let random = hex::encode(kist_crypto::random_bytes::<8>()?);
    Ok(OsString::from(format!(".kist-restore-{random}")))
}

/// 寫著內容的暫存檔：寫入用的 handle 加上負責刪檔的 [`TempPath`]。`file`
/// 放在前面：drop 時先關檔、再刪暫存名。
struct TempFile {
    file: std::fs::File,
    temp: TempPath,
}

impl TempFile {
    /// 在 `dir` 這一層建一個新的暫存檔（[`DirHandle::create_new_file`]：
    /// O_CREAT|O_EXCL，unix 另加 O_NOFOLLOW——名字已被佔用就失敗，不會打開
    /// 別人放的東西），建檔權限 `mode`（[`temp_file_mode`]）。
    fn create_in(dir: &DirHandle, mode: u32) -> Result<Self> {
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
fn mode_will_be_applied(node: &Entry) -> bool {
    node.mtime_ns.is_some() && node.mode.is_some_and(|m| m != 0)
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
fn temp_file_mode(dir: &DirHandle, name: &OsStr, node: &Entry) -> Result<u32> {
    if mode_will_be_applied(node) {
        return Ok(0o600);
    }
    Ok(dir.file_mode(name)?.unwrap_or(0o666))
}

/// 間接內容：串起來的清單 chunk → `ChunkList`（含版本檢查）→ chunk id。
fn chunk_ids_of(list: &[u8]) -> Result<Vec<ChunkId>> {
    parse_chunk_list(list)
        .map(|l| l.chunks)
        .map_err(|e| CoreError::corrupt("<chunk list>", e.to_string()))
}

/// 硬連結的第二個以後的名字：從還原目標的 handle 沿第一個名字的相對路徑
/// `first` 逐層重新開到它所在的那一層（O_NOFOLLOW；途中被換成 symlink 就開
/// 不起來，呼叫端退回複製），再 linkat 到 `dir` 的 `name`（ADR 019 A4）。
/// 正式名處已經有東西時 linkat 回 EEXIST（最常見：同一個 snapshot 再還原
/// 一次），改走 [`link_over`]。
fn link_to_first(
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
fn refuse_in_the_way(dir: &DirHandle, name: &OsStr) -> Result<()> {
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
trait OpenDir {
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
struct PendingDir<'p> {
    parent: &'p mut dyn OpenDir,
    make: Make,
    path: PathBuf,
    /// 這一層最後會套回記錄的 mode（[`mode_will_be_applied`]），所以既有的唯讀目錄
    /// 可以先暫時加寫入權。root 的落點沒有 metadata，不加。
    loosen: bool,
    handle: Option<DirHandle>,
    /// [`make_owner_writable`] 做過了（第一次往這層寫東西之前做一次）。
    prepared: bool,
    /// 暫時加上寫入權之前的權限：套 metadata 失敗時放回去。
    loosened: Option<std::fs::Permissions>,
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
    fn base(parent: &'p mut dyn OpenDir, rel: &Path) -> Self {
        let path = parent.location().join(rel);
        Self::new(parent, Make::Base(rel.to_path_buf()), path, false)
    }

    /// snapshot 裡的目錄節點 `node`，名字 `name`。
    fn child(parent: &'p mut dyn OpenDir, name: &OsStr, node: &Entry) -> Self {
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
        }
    }

    /// 建立這一層（建過就沿用），不做寫入前的準備。
    fn create(&mut self) -> Result<()> {
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
            ..
        } = self;
        let handle = match handle {
            Some(handle) => &*handle,
            slot @ None => {
                let above = parent.open()?;
                let created = match make {
                    Make::Base(rel) => above.create_dirs(rel)?,
                    Make::Child(name) => above.create_child_dir(name)?,
                };
                &*slot.insert(created)
            }
        };
        if for_writing && !*prepared {
            *prepared = true;
            if *loosen {
                *loosened = make_owner_writable(handle)?;
            }
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

/// restore 走訪時一路帶著的東西。
struct Walk<'w> {
    /// 還原目標本身：硬連結的後續名字從這裡沿路徑重新找第一個名字（[`link_to_first`]）。
    target_dir: DirHandle,
    index: &'w ReloadableIndex,
    summary: RestoreSummary,
    /// 範圍是**整個 snapshot、跨 roots**（docs/format.md §8.3）。
    hardlinks: HardLinks,
    select: Select<'w>,
}

/// 部分還原的選取（[`RestoreOptions`]）：每個節點的 [`Pos`] 從上一層往下走一步。
struct Select<'o> {
    include: &'o PathFilter,
    exclude: &'o PathFilter,
    /// 每條 include 樣式有沒有選到東西：涵蓋（比對到它或它的上層）某個被還原的節點
    /// （[`Select::credit`]、[`RestoreSummary::unmatched_includes`]）。
    hit: Vec<bool>,
}

/// 一個節點在兩組樣式裡的位置。
#[derive(Clone)]
struct Pos {
    include: FilterState,
    exclude: FilterState,
    /// 自己或某個上層被 include 比對到（沒有 include 時一律是）：要還原。
    selected: bool,
    /// 比對到自己或某個上層、還沒算成選到東西的正向 include 樣式。這個節點被還原時它們都
    /// 選到了東西（[`Select::credit`]）。
    pending: Vec<usize>,
}

impl<'o> Select<'o> {
    /// 只有反向樣式的 include 什麼都選不到：反向樣式只取消前面的 include 選到的（ADR
    /// 023）。直接報錯，不以「還原 0 個檔案」成功結束（審查修正）。
    fn new(opts: &'o RestoreOptions) -> Result<Self> {
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
    fn start(&self) -> Pos {
        Pos {
            include: self.include.start(),
            exclude: self.exclude.start(),
            selected: self.include.is_empty(),
            pending: Vec::new(),
        }
    }

    /// `parent` 底下的 `name`；被 exclude 比對到 → `None`。
    fn child(&self, parent: &Pos, name: &[u8]) -> Option<Pos> {
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
    fn credit(&mut self, pos: &Pos) {
        if pos.selected {
            for &p in &pos.pending {
                self.hit[p] = true;
            }
        }
    }

    /// 從頂端沿 `names` 走到底；途中被 exclude 比對到 → `None`（那底下全部不還原）。
    fn walk<'n>(&self, names: impl IntoIterator<Item = &'n [u8]>) -> Option<Pos> {
        let mut pos = self.start();
        for name in names {
            pos = self.child(&pos, name)?;
        }
        Some(pos)
    }

    /// `parent` 底下的 `entry` 要不要走：被排除、或沒被選到而且底下也不可能有東西被
    /// 選到 → `None`。
    fn visit(&mut self, parent: &Pos, entry: &Entry) -> Option<Pos> {
        let pos = self.child(parent, &entry.name)?;
        self.credit(&pos);
        (pos.selected || (entry.kind == node_type::DIR && pos.include.may_match_below()))
            .then_some(pos)
    }

    /// 這個節點底下還值得走嗎：自己被選到，或底下還可能有東西被選到。
    fn worth_walking(pos: &Pos) -> bool {
        pos.selected || pos.include.may_match_below()
    }

    /// 沒選到東西的 include 樣式。反向樣式不算：它們本來就只會取消。
    fn unmatched(&self) -> Vec<String> {
        (0..self.include.len())
            .filter(|&p| !self.hit[p] && !self.include.is_negated(p))
            .map(|p| self.include.pattern(p).to_owned())
            .collect()
    }
}

/// 一個 root 在這次還原裡的落點（[`Repository::place_root`]）。
struct Placement {
    /// 落點目錄：目標底下的相對路徑。
    rel: PathBuf,
    /// 落點在選取上的位置。
    pos: Pos,
    /// 落點的末段與上一層的選取位置。root 的條目恰好是一個與末段同名的非目錄時，這個 root
    /// 是檔案來源（docs/format.md §9），條目改放在上一層；讀到 tree 才知道，所以留到
    /// 第二輪（[`Repository::open_placement`]）。SNAPSHOT:PATH 是 root 本身或在它裡面時
    /// 沒有：第一輪已經確定是目錄。
    file_root: Option<(Vec<u8>, Pos)>,
    content: Content,
}

/// 要放進落點的條目。
enum Content {
    /// 第二輪才讀：同時只有一個 root 的條目在記憶體裡。
    Tree(TreeId),
    /// 第一輪已經讀過：SNAPSHOT:PATH 就是 root 本身（或 root 是 `/`，定位沒有任何段）。
    Read(Vec<Entry>),
}

enum Placed {
    /// 這個 root 不在 [`RestoreOptions::subtree`] 之下，也不包含它。
    Elsewhere,
    /// 與 subtree 有關，但選取上整個不必走（被排除，或底下不可能有東西被選到）。
    Pruned,
    Here(Box<Placement>),
}

/// dry run（[`Repository::plan_restore`]）的走訪狀態。
struct Plan<'p, 'o> {
    select: Select<'o>,
    summary: RestoreSummary,
    on_node: &'p mut dyn FnMut(&Path, Planned),
    /// 目前路徑上的每一層，對應真的還原的 [`PendingDir`]。
    levels: Vec<PlanLevel>,
}

struct PlanLevel {
    path: PathBuf,
    /// snapshot 裡的目錄（root 的落點不是：真的還原不計數、這裡也不回報）。
    snapshot_dir: bool,
    created: bool,
}

impl Plan<'_, '_> {
    /// 要往最深一層寫東西：沿路還沒建的層算是建了，snapshot 裡的目錄依序回報（上層先）。
    /// 計數等那一層走完（[`Repository::plan_dir`]），與真的還原同一個時機。
    fn open(&mut self) {
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

impl Repository {
    /// 還原到 `target`。目標目錄最好是空的：既有檔案會被覆寫（ADR 019 A2：先寫
    /// 同目錄的暫存檔、成功才 rename 取代，還原失敗時原有的檔不動）。目標**之下**
    /// 的路徑不穿過 symlink——不論是既有的還是 snapshot 自己種的（另一個
    /// root 的定位穿過它）；目錄路徑遇到 symlink 回錯，與檔案路徑的
    /// symlink-in-the-way 防護一致。`target` 本身與其之上是使用者自己的
    /// 路徑，照常跟隨。
    ///
    /// 目標之下以目錄 handle 為錨逐層走（ADR 019 A4，[`DirHandle`]）：每一層
    /// 以 O_NOFOLLOW 開好之後，子項目都相對於它建立，不再以完整路徑重新解析。
    /// 還原途中某一層被搬走、原位換成外指 symlink，寫入仍落在已開的那個
    /// 真目錄，不會跟著 symlink 到目標之外。
    pub async fn restore(
        &self,
        snapshot_key: &str,
        target: &Path,
        opts: RestoreOptions,
    ) -> Result<RestoreSummary> {
        let select = Select::new(&opts)?;
        let snapshot = self.read_snapshot(snapshot_key).await?;
        // 第一輪：每個 root 的落點。SNAPSHOT:PATH 不對時在建立任何目錄之前就回錯（審查修正）。
        let placements = self
            .place_roots(&snapshot.roots, &opts.subtree, &select)
            .await?;
        let index = ReloadableIndex::new(self.load_index().await?);
        std::fs::create_dir_all(target).map_err(|e| CoreError::io(target, e))?;
        // 目標本身的 handle，整個 restore 都開著：每個 root 從它往下走；硬連結
        // 跨 roots，後續名字也從它重新找第一個名字。兩份是同一個 fd（clone 不多開）。
        let mut top = DirHandle::open_target(target)?;
        let mut w = Walk {
            target_dir: top.clone(),
            index: &index,
            summary: RestoreSummary::default(),
            hardlinks: HardLinks::new(),
            select,
        };
        for placement in placements {
            let (rel, pos, entries) = self.open_placement(placement, &mut w.select).await?;
            // 落點建不起來（例如擋著 symlink）：整個 restore 回錯，不是某一項的錯。沒被
            // 選到的落點等到底下真的有東西要寫才建（[`PendingDir`]）。
            let mut base = PendingDir::base(&mut top, &rel);
            if pos.selected {
                base.create()?;
            }
            for entry in entries {
                // 名稱不合法時記錯、其他照常還原，與目錄裡的子項目同一套規則。格式讀取時
                // 已經拒絕分隔符、NUL、`.`、`..`，會到這裡的只有 Windows 上帶反斜線的名字。
                if let Err(e) = fsmeta::validate_child_name(&entry.name) {
                    w.summary
                        .errors
                        .push(format!("{}: {e}", base.location().display()));
                    continue;
                }
                let Some(child) = w.select.visit(&pos, &entry) else {
                    continue;
                };
                let name = fsmeta::bytes_to_name(&entry.name)?;
                if child.selected {
                    base.open()?;
                }
                self.restore_node(&mut w, &mut base, &entry, &name, 1, child)
                    .await;
            }
        }
        w.summary.unmatched_includes = w.select.unmatched();
        Ok(w.summary)
    }

    /// 第一輪：每個 root 的落點（[`Repository::place_root`]）。`subtree` 不在任何 root 裡
    /// → 錯誤；這時還沒建任何東西。
    async fn place_roots(
        &self,
        roots: &[Root],
        subtree: &[Vec<u8>],
        select: &Select<'_>,
    ) -> Result<Vec<Placement>> {
        let mut placements = Vec::new();
        let mut found = subtree.is_empty();
        for root in roots {
            match self.place_root(root, subtree, select).await? {
                Placed::Elsewhere => {}
                Placed::Pruned => found = true,
                Placed::Here(placement) => {
                    found = true;
                    placements.push(*placement);
                }
            }
        }
        if !found {
            return Err(CoreError::Usage(format!(
                "{} is not in this snapshot",
                subtree_display(subtree)
            )));
        }
        Ok(placements)
    }

    /// 一個 root 的落點（ADR 020、022）。v3 的 restore 映射（docs/format.md §9）：目錄
    /// 來源的 entries 放在 `<定位>` 底下；**檔案/symlink 來源**（root tree 恰好一個非目錄
    /// entry、名稱＝定位的末段）放在 `<定位去掉末段>` 底下。`subtree` 不是空的時，落點是
    /// 去掉 subtree 之後的部分；subtree 在 root 裡面時見 [`Repository::place_inside`]。
    async fn place_root(
        &self,
        root: &Root,
        subtree: &[Vec<u8>],
        select: &Select<'_>,
    ) -> Result<Placed> {
        let names = fsmeta::locator_components(root.path.as_slice())?;
        // root 的定位與 subtree 必須有一個是另一個的開頭，否則這個 root 跟這次還原無關。
        let shared = names
            .iter()
            .zip(subtree)
            .take_while(|(name, wanted)| **name == wanted.as_slice())
            .count();
        if shared < names.len().min(subtree.len()) {
            return Ok(Placed::Elsewhere);
        }
        let Some((last, above)) = names.split_last().filter(|_| subtree.len() < names.len()) else {
            return self.place_inside(root, &names, subtree, select).await;
        };
        // subtree 是定位的上層，一定存在。沿定位走：定位或它的上層被排除、或底下不可能有
        // 東西被選到，這個 root 連 tree 都不讀、落點也不建。
        let Some(parent) = select.walk(above.iter().copied()) else {
            return Ok(Placed::Pruned);
        };
        let pos = match select.child(&parent, last) {
            Some(pos) if Select::worth_walking(&pos) => pos,
            _ => return Ok(Placed::Pruned),
        };
        let rel = names[subtree.len()..]
            .iter()
            .map(|name| fsmeta::bytes_to_name(name))
            .collect::<Result<PathBuf>>()?;
        Ok(Placed::Here(Box::new(Placement {
            rel,
            pos,
            file_root: Some((last.to_vec(), parent)),
            content: Content::Tree(root.tree),
        })))
    }

    /// SNAPSHOT:PATH 是這個 root 本身或在它的定位底下：先沿 tree 確認那個路徑存在、是
    /// 目錄，再看選取——打錯的路徑不能因為 root 被選取剪掉，就以「什麼都沒還原」成功
    /// 結束（審查修正）。它的內容直接放進目標本身。
    async fn place_inside(
        &self,
        root: &Root,
        names: &[&[u8]],
        subtree: &[Vec<u8>],
        select: &Select<'_>,
    ) -> Result<Placed> {
        let mut entries = self.read_tree_chain(&root.tree).await?;
        if names
            .last()
            .is_some_and(|name| is_file_root(name, &entries))
        {
            return Err(not_a_directory(subtree));
        }
        // 最後一層的條目第二輪才讀。
        let mut unread = None;
        for wanted in &subtree[names.len()..] {
            if let Some(id) = unread {
                entries = self.read_tree_chain(&id).await?;
            }
            let Some(entry) = entries.iter().find(|e| e.name == *wanted) else {
                return Ok(Placed::Elsewhere);
            };
            if entry.kind != node_type::DIR || entry.subtree.is_zero() {
                return Err(not_a_directory(subtree));
            }
            unread = Some(entry.subtree);
        }
        let content = match unread {
            Some(id) => Content::Tree(id),
            None => Content::Read(entries),
        };
        match select.walk(subtree.iter().map(Vec::as_slice)) {
            Some(pos) if Select::worth_walking(&pos) => Ok(Placed::Here(Box::new(Placement {
                rel: PathBuf::new(),
                pos,
                file_root: None,
                content,
            }))),
            _ => Ok(Placed::Pruned),
        }
    }

    /// 第二輪：讀落點的條目；檔案來源的條目改放在落點的上一層。回傳落點、它的選取位置、
    /// 條目。落點被選到就算還原了（空的目錄 root 也是），在這裡記 include 選到東西，真的
    /// 還原與 dry run 一致。
    async fn open_placement(
        &self,
        placement: Placement,
        select: &mut Select<'_>,
    ) -> Result<(PathBuf, Pos, Vec<Entry>)> {
        let entries = match placement.content {
            Content::Tree(id) => self.read_tree_chain(&id).await?,
            Content::Read(entries) => entries,
        };
        let Placement {
            mut rel, mut pos, ..
        } = placement;
        if let Some((name, parent)) = placement.file_root {
            if is_file_root(&name, &entries) {
                rel.pop();
                pos = parent;
            }
        }
        select.credit(&pos);
        Ok((rel, pos, entries))
    }

    /// dry run（ADR 022）：與 [`Repository::restore`] 同樣的落點與選取，但什麼都不寫，連
    /// `target` 都不建。每個會被還原的節點依走訪順序交給 `on_node`（路徑是 `target`
    /// 底下的完整路徑；目錄在它的內容之前）；回傳的計數與 `unmatched_includes` 與真的
    /// 還原相同。只讀 tree、不讀檔案內容，所以真的還原時才會發生的錯誤（壞掉的 chunk、
    /// 目標處擋著的東西）這裡看不到。
    pub async fn plan_restore(
        &self,
        snapshot_key: &str,
        target: &Path,
        opts: RestoreOptions,
        on_node: &mut dyn FnMut(&Path, Planned),
    ) -> Result<RestoreSummary> {
        let select = Select::new(&opts)?;
        let snapshot = self.read_snapshot(snapshot_key).await?;
        let placements = self
            .place_roots(&snapshot.roots, &opts.subtree, &select)
            .await?;
        let mut p = Plan {
            select,
            summary: RestoreSummary::default(),
            on_node,
            levels: Vec::new(),
        };
        for placement in placements {
            let (rel, pos, entries) = self.open_placement(placement, &mut p.select).await?;
            let base = target.join(&rel);
            p.levels.push(PlanLevel {
                path: base.clone(),
                snapshot_dir: false,
                created: false,
            });
            if pos.selected {
                p.open();
            }
            for entry in entries {
                if let Err(e) = fsmeta::validate_child_name(&entry.name) {
                    p.summary.errors.push(format!("{}: {e}", base.display()));
                    continue;
                }
                let Some(child) = p.select.visit(&pos, &entry) else {
                    continue;
                };
                let path = base.join(fsmeta::bytes_to_name(&entry.name)?);
                self.plan_node(&mut p, &entry, path, 1, child).await;
            }
            p.levels.pop();
        }
        p.summary.unmatched_includes = p.select.unmatched();
        Ok(p.summary)
    }

    /// dry run 的一個節點，對應 [`Repository::restore_node`]（同樣的分派與錯誤記錄）。
    fn plan_node<'a, 'p: 'a, 'o: 'a>(
        &'a self,
        p: &'a mut Plan<'p, 'o>,
        node: &'a Entry,
        path: PathBuf,
        depth: usize,
        pos: Pos,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + 'a>> {
        Box::pin(async move {
            let result = match node.kind {
                node_type::DIR if !node.subtree.is_zero() => {
                    if depth >= MAX_TREE_DEPTH {
                        Err(CoreError::Corrupt {
                            key: path.display().to_string(),
                            reason: format!(
                                "tree nesting deeper than {MAX_TREE_DEPTH} levels \
                                 (corrupt or hostile repository)"
                            ),
                        })
                    } else {
                        self.plan_dir(p, node, &path, depth, pos).await
                    }
                }
                node_type::FILE | node_type::SYMLINK => {
                    p.open();
                    let planned = if node.kind == node_type::FILE {
                        p.summary.files += 1;
                        Planned::File
                    } else {
                        p.summary.symlinks += 1;
                        Planned::Symlink
                    };
                    (p.on_node)(&path, planned);
                    Ok(())
                }
                other => Err(CoreError::Corrupt {
                    key: path.display().to_string(),
                    reason: format!("unknown node type {other}"),
                }),
            };
            if let Err(e) = result {
                p.summary.errors.push(format!("{}: {e}", path.display()));
            }
        })
    }

    /// dry run 的目錄，對應 [`Repository::restore_dir`]：被選到的一進來就算建了，沒被選到
    /// 的等底下有東西才算；建了的在走完之後計數。
    async fn plan_dir(
        &self,
        p: &mut Plan<'_, '_>,
        node: &Entry,
        path: &Path,
        depth: usize,
        pos: Pos,
    ) -> Result<()> {
        p.levels.push(PlanLevel {
            path: path.to_path_buf(),
            snapshot_dir: true,
            created: false,
        });
        if pos.selected {
            p.open();
        }
        let result = self.plan_children(p, node, path, depth, &pos).await;
        let created = p.levels.pop().is_some_and(|level| level.created);
        result?;
        if created {
            p.summary.dirs += 1;
        }
        Ok(())
    }

    async fn plan_children(
        &self,
        p: &mut Plan<'_, '_>,
        node: &Entry,
        path: &Path,
        depth: usize,
        pos: &Pos,
    ) -> Result<()> {
        let children = self.read_tree_chain(&node.subtree).await?;
        for child in children {
            if let Err(e) = fsmeta::validate_child_name(&child.name) {
                p.summary.errors.push(format!("{}: {e}", path.display()));
                continue;
            }
            let Some(child_pos) = p.select.visit(pos, &child) else {
                continue;
            };
            let child_path = path.join(fsmeta::bytes_to_name(&child.name)?);
            self.plan_node(p, &child, child_path, depth + 1, child_pos)
                .await;
        }
        Ok(())
    }

    /// 還原一個節點：`dir` 這一層裡的 `name`。錯誤記進 summary，不往上拋：一個
    /// 壞掉的 chunk 不該讓其他 99% 的檔案也拿不回來。
    /// `depth`：DIR 巢狀深度（root 的子女 = 1）；超過 [`MAX_TREE_DEPTH`] 的
    /// chain 只能出自腐壞或敵意 repo——記錄錯誤、不深入，否則遞迴會把
    /// process 墊進 stack overflow。這個上限同時是同時開著的目錄 handle 數的
    /// 上限（每層一個，ADR 019 A4）。
    /// `pos`：這個節點在選取上的位置（[`Select::visit`] 放行的才會走到這裡：檔案與
    /// symlink 一定是被選到的，目錄則可能只是「底下也許有東西被選到」）。
    fn restore_node<'a, 'w: 'a>(
        &'a self,
        w: &'a mut Walk<'w>,
        dir: &'a mut dyn OpenDir,
        node: &'a Entry,
        name: &'a OsStr,
        depth: usize,
        pos: Pos,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + 'a>> {
        Box::pin(async move {
            // 完整路徑只用在錯誤訊息；實際操作都經 `dir`（ADR 019 A4）。
            let path = dir.location().join(name);
            let result = match node.kind {
                node_type::DIR if !node.subtree.is_zero() => {
                    if depth >= MAX_TREE_DEPTH {
                        Err(CoreError::Corrupt {
                            key: path.display().to_string(),
                            reason: format!(
                                "tree nesting deeper than {MAX_TREE_DEPTH} levels \
                                 (corrupt or hostile repository)"
                            ),
                        })
                    } else {
                        self.restore_dir(w, dir, node, name, depth, pos).await
                    }
                }
                node_type::FILE => match dir.open() {
                    Ok(dir) => self.restore_file_entry(w, node, dir, name).await,
                    Err(e) => Err(e),
                },
                node_type::SYMLINK => dir
                    .open()
                    .and_then(|dir| restore_symlink(dir, name, node, &mut w.summary)),
                other => Err(CoreError::Corrupt {
                    key: path.display().to_string(),
                    reason: format!("unknown node type {other}"),
                }),
            };
            if let Err(e) = result {
                tracing::warn!("{}: {e}", path.display());
                w.summary.errors.push(format!("{}: {e}", path.display()));
            }
        })
    }

    /// 還原一個 FILE 節點：硬連結的第二個以後的名字先試 link 到第一個，不成
    /// 就退回複製；複製走暫存檔＋rename（ADR 019 A2）。commit 成功才計數、
    /// 才記進硬連結表——metadata 套不上時也是（內容已經對了，錯另外回）。
    async fn restore_file_entry(
        &self,
        w: &mut Walk<'_>,
        node: &Entry,
        dir: &DirHandle,
        name: &OsStr,
    ) -> Result<()> {
        let path = dir.child_path(name);
        let hardlink_key = (node.nlink.unwrap_or(0) > 1)
            .then_some((node.dev.unwrap_or(0), node.inode.unwrap_or(0)))
            .filter(|k| k.1 != 0);
        if let Some(first) = hardlink_key.and_then(|k| w.hardlinks.get(&k)) {
            match link_to_first(&w.target_dir, first, dir, name) {
                Ok(()) => {
                    w.summary.files += 1;
                    return Ok(());
                }
                Err(e) => {
                    let first_path = w.target_dir.path().join(first.iter().collect::<PathBuf>());
                    tracing::warn!(
                        "{}: cannot hard-link to {}: {e}; restoring a copy",
                        path.display(),
                        first_path.display()
                    );
                }
            }
        }
        // 硬連結失敗後的複製也走這裡。失敗時暫存檔隨錯誤被 drop 刪掉；正式名
        // 底下原有的檔沒被碰過，不能再像以前那樣 remove_file(path)（ADR 019 A2）。
        let temp = self.restore_file(dir, name, node, w.index).await?;
        #[cfg(all(test, unix))]
        meta_via_handle_tests::before_meta(&temp.temp.path(), &path);
        // metadata 經寫入內容的同一個 handle 套用（ADR 019 A3）：路徑在寫完
        // 之後被換成 symlink，也改不到外面的檔。順序與擁有者見 apply_meta
        // （ADR 019 A43）。
        let (applied, owner) = apply_meta(&temp.file, &path, node);
        // metadata 套不上（例如目標檔案系統不收 xattr）時內容仍是對的：照樣
        // rename 成正式名、記下這個錯。那時記錄的 mode 沒套上，檔案停在建檔時
        // 的權限（temp_file_mode：0o600）。先關檔再 rename：Windows 上 rename
        // 開著的檔要看共用模式，關掉最單純；unix 沒有差別。
        let TempFile { file, temp } = temp;
        drop(file);
        temp.commit(name)?;
        w.summary.files += 1;
        if let Some(k) = hardlink_key {
            w.hardlinks.insert(k, dir.child_rel(name));
        }
        applied.and(owner)
    }

    /// 還原 `parent` 這一層裡的目錄 `name`：建立（或沿用）並開著它，子項目都
    /// 經這個 handle 建立，metadata 最後也套在它上面（ADR 019 A3、A4）。這一層
    /// 之後被搬走、原位換成 symlink，子項目與 metadata 都跟著真目錄走，不會
    /// 落到 symlink 指的地方。handle 在子項目還原期間一直開著：同時開著的
    /// 數目是遞迴深度，不是 entry 數。
    ///
    /// 沒被選到的目錄（`pos.selected` 是 false，ADR 020）先不建：底下第一次有東西要寫
    /// 才沿路建上來；到最後都沒建的就不計數、不套 metadata。
    async fn restore_dir(
        &self,
        w: &mut Walk<'_>,
        parent: &mut dyn OpenDir,
        node: &Entry,
        name: &OsStr,
        depth: usize,
        pos: Pos,
    ) -> Result<()> {
        let mut dir = PendingDir::child(parent, name, node);
        if pos.selected {
            dir.create()?;
        }
        let children = self.read_tree_chain(&node.subtree).await?;
        // 既有的唯讀目錄在第一個子項目寫進去之前暫時加上擁有者寫入權（ADR 019 A2，
        // [`PendingDir::open`]）。在讀完子 tree 之後：從那裡到下面重新套 mode 之間沒有
        // 提早 return——子項目的錯誤記進 summary、不往上拋；迴圈裡的 bytes_to_name 在
        // unix（唯一會加寫入權的平台）不會失敗。
        for child in children {
            if let Err(e) = fsmeta::validate_child_name(&child.name) {
                w.summary
                    .errors
                    .push(format!("{}: {e}", dir.location().display()));
                continue;
            }
            let Some(child_pos) = w.select.visit(&pos, &child) else {
                continue;
            };
            let child_name = fsmeta::bytes_to_name(&child.name)?;
            if child_pos.selected {
                dir.open()?;
            }
            self.restore_node(w, &mut dir, &child, &child_name, depth + 1, child_pos)
                .await;
        }
        let PendingDir {
            handle, loosened, ..
        } = dir;
        let Some(dir) = handle else {
            return Ok(());
        };
        w.summary.dirs += 1;
        #[cfg(all(test, unix))]
        meta_via_handle_tests::before_meta(dir.path(), dir.path());
        // 子項目都寫完後才設目錄的 mtime，否則會被後續寫入覆蓋；順序見 apply_meta。
        // 沒有要套的 metadata（s3/generic 來源的目錄）就不動，與以前一樣。
        if node.xattrs.is_none() && node.mtime_ns.is_none() {
            return Ok(());
        }
        let handle = dir.meta_handle()?;
        let (applied, owner) = apply_meta(&handle, dir.path(), node);
        // 記錄的 mode 沒套上（xattr 或時間先失敗）：至少把暫時加的寫入權拿掉。
        // 只有擁有者設不回去時 mode 已經套上（清掉 0o6000），不動它。
        if applied.is_err() {
            if let Some(original) = loosened {
                let _ = handle.set_permissions(original);
            }
        }
        applied.and(owner)
    }

    /// 把檔案條目 `node` 的內容寫進 `dir` 這一層的暫存檔，回傳它（ADR 019 A2）：
    /// 呼叫端在同一個 handle 上套 metadata（ADR 019 A3），再 commit（rename）成
    /// 正式名 `name`。任何一步失敗，暫存檔隨錯誤被 drop 刪掉，正式名底下原有的
    /// 檔不受影響。
    async fn restore_file(
        &self,
        dir: &DirHandle,
        name: &OsStr,
        node: &Entry,
        index: &ReloadableIndex,
    ) -> Result<TempFile> {
        let path = dir.child_path(name);
        // 先看一眼正式名處：擋著 symlink 之類就不必下載內容（commit 前會再看一次）。
        refuse_in_the_way(dir, name)?;
        let chunk_ids = self
            .resolve_chunks_reloading(&node.chunks, node.content, index)
            .await?;
        // 內容寫進新建的暫存檔，不開正式名：以前以 write＋create＋truncate
        // 開正式名，失敗後呼叫端再 remove_file，連還沒開檔就失敗（間接內容的
        // 清單讀不到）的情況也會刪掉使用者原有的檔。
        let temp = TempFile::create_in(dir, temp_file_mode(dir, name, node)?)?;
        let mut writer = std::io::BufWriter::new(&temp.file);
        let mut written = 0u64;
        for id in &chunk_ids {
            let data = self.read_chunk_reloading(id, index).await?;
            writer
                .write_all(&data)
                .map_err(|e| CoreError::io(&path, e))?;
            written += data.len() as u64;
        }
        writer.flush().map_err(|e| CoreError::io(&path, e))?;
        drop(writer); // writer 借用著 temp.file；回傳 temp 之前先放掉
        if written != node.size {
            return Err(CoreError::Corrupt {
                key: path.display().to_string(),
                reason: format!("restored {written} bytes but snapshot says {}", node.size),
            });
        }
        Ok(temp)
    }

    /// 直接內容回傳原清單；間接內容先把清單 chunk 讀出來解成 ChunkList（含版本檢查）。
    pub(crate) async fn resolve_chunks<I>(
        &self,
        chunks: &[ChunkId],
        content: u8,
        index: &I,
    ) -> Result<Vec<ChunkId>>
    where
        I: ChunkLocator + Sync + ?Sized,
    {
        if content == content_type::DIRECT {
            return Ok(chunks.to_vec());
        }
        let mut bytes = Vec::new();
        for id in chunks {
            bytes.extend_from_slice(&self.read_chunk(id, index).await?);
        }
        chunk_ids_of(&bytes)
    }

    /// 同 `resolve_chunks`，但 chunk 讀取走 [`Self::read_chunk_reloading`]
    /// （restore 與 mount 用：開始得早的一方手上的 index 可能已被 prune 搬動）。
    pub async fn resolve_chunks_reloading(
        &self,
        chunks: &[ChunkId],
        content: u8,
        index: &ReloadableIndex,
    ) -> Result<Vec<ChunkId>> {
        if content == content_type::DIRECT {
            return Ok(chunks.to_vec());
        }
        let mut bytes = Vec::new();
        for id in chunks {
            bytes.extend_from_slice(&self.read_chunk_reloading(id, index).await?);
        }
        chunk_ids_of(&bytes)
    }

    /// 同 `read_chunk`，但 chunk 不在 index 或它的 pack 不見了時重新載入 index 再試一次：
    /// prune 的 repack 會把活 chunk 搬到新 pack、之後刪舊 pack，開始得比較早的 restore
    /// 手上的 index 指到舊位置。重載後還是找不到才是真的壞。
    pub async fn read_chunk_reloading(
        &self,
        id: &ChunkId,
        index: &ReloadableIndex,
    ) -> Result<Vec<u8>> {
        let first = {
            let guard = index.index.read().await;
            self.read_chunk(id, &*guard).await
        };
        match first {
            Err(CoreError::ChunkMissing(_))
            | Err(CoreError::Backend(kist_backend::BackendError::NotFound(_))) => {}
            other => return other,
        }
        index.reload(self).await?;
        let guard = index.index.read().await;
        self.read_chunk(id, &*guard).await
    }

    /// 從 pack 讀一個 chunk 的明文（range read + 解密 + 驗證）。
    pub(crate) async fn read_chunk<I>(&self, id: &ChunkId, index: &I) -> Result<Vec<u8>>
    where
        I: ChunkLocator + Sync + ?Sized,
    {
        let loc = index.get(id).ok_or(CoreError::ChunkMissing(*id))?;
        let key = keys::pack(&loc.pack);
        let end = loc
            .offset
            .checked_add(loc.length)
            .ok_or_else(|| CoreError::Corrupt {
                key: key.clone(),
                reason: format!("chunk {id} range overflows"),
            })?;
        let bytes = self.backend().get_range(&key, loc.offset..end).await?;
        let keys = Arc::clone(self.keys());
        let id = *id;
        let raw_len = loc.raw_len;
        let max_chunk = u64::from(self.config().chunker.max);
        blocking(move || decode_chunk(&keys, &id, &bytes, raw_len, max_chunk)).await
    }
}

/// 還原 symlink 條目：建連結（[`replace_with_symlink`]），再套它本身的 metadata。
fn restore_symlink(
    dir: &DirHandle,
    name: &OsStr,
    node: &Entry,
    summary: &mut RestoreSummary,
) -> Result<()> {
    let link_target = fsmeta::bytes_to_name(&node.target)?;
    replace_with_symlink(dir, name, Path::new(&link_target))?;
    summary.symlinks += 1;
    if node.xattrs.is_some() {
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

/// ADR 019 A3：metadata 必須經寫入時的 handle 套用。以路徑套用的話，內容寫完
/// 之後路徑被換成指向外面的 symlink，mode（含 setuid 位）與時間就套到外面的檔上。
#[cfg(all(test, unix))]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod meta_via_handle_tests {
    use std::cell::RefCell;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    use std::path::{Path, PathBuf};

    use crate::{BackupOptions, InitOptions, Repository, RestoreOptions, SourceSpec};

    thread_local! {
        /// 測試接縫的設定：(要換掉的路徑, symlink 指向的外部路徑)。
        /// `#[tokio::test]` 是單執行緒 runtime，restore 的走訪跑在測試自己的
        /// 執行緒上，所以 thread_local 只影響設定它的那個測試。
        static SWAP: RefCell<Option<(PathBuf, PathBuf)>> = const { RefCell::new(None) };
        /// 每次呼叫 [`before_meta`] 時 `written` 的權限位元：(path, mode)。
        static SEEN_MODES: RefCell<Vec<(PathBuf, u32)>> = const { RefCell::new(Vec::new()) };
    }

    /// restore 在內容寫完（目錄：子項目都寫完）、metadata 還沒套之前呼叫。
    /// `written` 是內容實際所在的路徑：檔案是暫存檔（ADR 019 A2），目錄就是
    /// `path`。`path` 符合設定時，把 `written` 搬到旁邊（`<path>.moved`），
    /// 原位換成指向外面的 symlink——模擬本機攻擊者在這個空檔動手。
    pub(super) fn before_meta(written: &Path, path: &Path) {
        let mode = std::fs::symlink_metadata(written).unwrap().mode() & 0o7777;
        SEEN_MODES.with(|seen| seen.borrow_mut().push((path.to_path_buf(), mode)));
        SWAP.with(|swap| {
            if let Some((victim, outside)) = swap.borrow().as_ref() {
                if victim == path {
                    std::fs::rename(written, moved(path)).unwrap();
                    std::os::unix::fs::symlink(outside, written).unwrap();
                }
            }
        });
    }

    fn moved(path: &Path) -> PathBuf {
        PathBuf::from(format!("{}.moved", path.display()))
    }

    const RECORDED_MTIME: i64 = 1_000_000;
    const OUTSIDE_MTIME: i64 = 2_000_000_000;

    struct Setup {
        dir: tempfile::TempDir,
        repo: Repository,
        snapshot: String,
        /// 還原後來源根所在的位置（`<target>/<來源的絕對路徑>`）。
        restored: PathBuf,
        target: PathBuf,
    }

    /// 來源：`f.bin` 記錄成 mode 0o4755、`d/` 記錄成 0o700，mtime 都是
    /// RECORDED_MTIME。目標之外（同一個 tempdir 的 `outside/`）：`victim.txt`
    /// 0o600、`victim_dir/` 0o750，mtime 都是 OUTSIDE_MTIME。
    async fn setup() -> Setup {
        let dir = tempfile::tempdir().unwrap();
        let backend = kist_backend::Backend::local(&dir.path().join("repo")).unwrap();
        let repo = Repository::init(
            backend,
            b"test password",
            InitOptions {
                kdf_cost: kist_crypto::KdfCost {
                    m_cost_kib: 8,
                    t_cost: 1,
                    p_cost: 1,
                },
                replicas: Some(0),
                ..InitOptions::default()
            },
        )
        .await
        .unwrap();

        let recorded = filetime::FileTime::from_unix_time(RECORDED_MTIME, 0);
        let src = dir.path().join("src");
        std::fs::create_dir_all(src.join("d")).unwrap();
        std::fs::write(src.join("d").join("inner.txt"), b"inner").unwrap();
        std::fs::write(src.join("f.bin"), b"payload").unwrap();
        std::fs::set_permissions(src.join("f.bin"), PermissionsExt::from_mode(0o4755)).unwrap();
        std::fs::set_permissions(src.join("d"), PermissionsExt::from_mode(0o700)).unwrap();
        filetime::set_file_mtime(src.join("f.bin"), recorded).unwrap();
        filetime::set_file_mtime(src.join("d"), recorded).unwrap();

        let outside = dir.path().join("outside");
        std::fs::create_dir_all(outside.join("victim_dir")).unwrap();
        std::fs::write(outside.join("victim.txt"), b"not yours").unwrap();
        std::fs::set_permissions(outside.join("victim.txt"), PermissionsExt::from_mode(0o600))
            .unwrap();
        std::fs::set_permissions(outside.join("victim_dir"), PermissionsExt::from_mode(0o750))
            .unwrap();
        let outside_time = filetime::FileTime::from_unix_time(OUTSIDE_MTIME, 0);
        filetime::set_file_mtime(outside.join("victim.txt"), outside_time).unwrap();
        filetime::set_file_mtime(outside.join("victim_dir"), outside_time).unwrap();

        let summary = repo
            .backup(
                std::slice::from_ref(&src),
                BackupOptions {
                    client_id: [0x11; 16],
                    hostname: "testhost".to_owned(),
                    username: "tester".to_owned(),
                    now: None,
                    gc_grace: crate::DEFAULT_GC_GRACE,
                    parity: 0,
                    progress: None,
                    source: SourceSpec::default(),
                    exclude: crate::Exclusions::default(),
                },
            )
            .await
            .unwrap();
        let target = dir.path().join("out");
        let restored = target.join(src.strip_prefix("/").unwrap());
        Setup {
            dir,
            repo,
            snapshot: summary.snapshot_key,
            restored,
            target,
        }
    }

    /// (八進位 mode 字串, mtime 秒)：斷言失敗時直接看得出 4755 之類的值。
    fn mode_and_mtime(path: &Path) -> (String, i64) {
        let m = std::fs::symlink_metadata(path).unwrap();
        (format!("{:o}", m.mode() & 0o7777), m.mtime())
    }

    /// 檔案：內容寫完後（寫在暫存檔，ADR 019 A2）暫存檔的路徑被換成指向外面
    /// 檔案的 symlink。外面的檔不能被改成 4755、mtime 也不能動；metadata 落在
    /// 寫入的那個 inode（被搬到旁邊的檔）上。之後 rename 把換上的 symlink 本身
    /// 搬到正式名（rename 不跟隨），外面的檔同樣不受影響。
    #[tokio::test]
    async fn file_meta_does_not_follow_a_swapped_in_symlink() {
        let s = setup().await;
        let victim = s.dir.path().join("outside").join("victim.txt");
        let path = s.restored.join("f.bin");
        SWAP.with(|swap| *swap.borrow_mut() = Some((path.clone(), victim.clone())));

        let summary = s
            .repo
            .restore(&s.snapshot, &s.target, RestoreOptions::default())
            .await
            .unwrap();

        assert_eq!(
            mode_and_mtime(&victim),
            ("600".to_owned(), OUTSIDE_MTIME),
            "目標之外的檔被改了 (mode, mtime)；summary：{summary:?}"
        );
        assert!(
            std::fs::symlink_metadata(&path)
                .unwrap()
                .file_type()
                .is_symlink(),
            "換上的 symlink 不該被動到"
        );
        assert_eq!(
            mode_and_mtime(&moved(&path)),
            ("4755".to_owned(), RECORDED_MTIME),
            "metadata 應該套在寫入的那個檔上"
        );
        assert!(summary.errors.is_empty(), "{summary:?}");
    }

    /// ADR 019 A2（審查）：內容寫進暫存檔之後、記錄的 mode 套上之前，暫存檔
    /// 不能讓群組與其他人讀得到——那段時間別的本機使用者開了它，之後的 fchmod
    /// 收不回已開的 fd。以 umask 預設（0o644）建暫存檔時，覆寫使用者 0o600 的
    /// 檔也會經過這一段。記錄的 mode（f.bin：4755）之後照常套上。
    #[tokio::test]
    async fn temp_file_is_private_until_the_recorded_mode_is_applied() {
        let s = setup().await;
        let path = s.restored.join("f.bin");

        let summary = s
            .repo
            .restore(&s.snapshot, &s.target, RestoreOptions::default())
            .await
            .unwrap();

        let before = SEEN_MODES.with(|seen| {
            seen.borrow()
                .iter()
                .find(|(p, _)| p == &path)
                .map(|(_, mode)| format!("{mode:o}"))
        });
        assert_eq!(
            before.as_deref(),
            Some("600"),
            "套 metadata 之前暫存檔的權限；summary：{summary:?}"
        );
        assert_eq!(mode_and_mtime(&path), ("4755".to_owned(), RECORDED_MTIME));
        assert!(summary.errors.is_empty(), "{summary:?}");
    }

    /// 目錄：子項目寫完後路徑被換成指向外面目錄的 symlink。外面的目錄不能被
    /// 改 mode 與 mtime。ADR 019 A4 之後目錄的 metadata 套在往下走時開的那個
    /// handle 上，不再以路徑重開：metadata 落在被搬到旁邊的真目錄，與檔案
    /// 一樣，也就沒有錯誤（A3 時重開遇到 symlink 回報擋路錯誤）。
    #[tokio::test]
    async fn dir_meta_does_not_follow_a_swapped_in_symlink() {
        let s = setup().await;
        let victim = s.dir.path().join("outside").join("victim_dir");
        let path = s.restored.join("d");
        SWAP.with(|swap| *swap.borrow_mut() = Some((path.clone(), victim.clone())));

        let summary = s
            .repo
            .restore(&s.snapshot, &s.target, RestoreOptions::default())
            .await
            .unwrap();

        assert_eq!(
            mode_and_mtime(&victim),
            ("750".to_owned(), OUTSIDE_MTIME),
            "目標之外的目錄被改了 (mode, mtime)；summary：{summary:?}"
        );
        assert_eq!(
            std::fs::read(moved(&path).join("inner.txt")).unwrap(),
            b"inner"
        );
        assert_eq!(
            mode_and_mtime(&moved(&path)),
            ("700".to_owned(), RECORDED_MTIME),
            "metadata 應該套在還原出的那個目錄上"
        );
        assert!(summary.errors.is_empty(), "{summary:?}");
    }
}
