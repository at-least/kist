//! restore：把一個 snapshot 還原到目標目錄。
//!
//! 目標目錄底下會重建完整的絕對路徑（`<target>/home/user/data/...`），
//! 這樣一個 snapshot 含多個來源路徑時不會互相覆蓋。

use std::ffi::{OsStr, OsString};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::{Mutex, RwLock};

use kist_format::tree::{content_type, node_type, parse_chunk_list, Entry};
use kist_format::{keys, ChunkId};

use crate::dirhandle::{DirHandle, Kind};
use crate::fsmeta;
use crate::index::{ChunkIndex, ChunkLocator};
use crate::pack::decode_chunk;
use crate::repo::Repository;
use crate::{blocking, CoreError, Result, MAX_TREE_DEPTH};

#[derive(Debug, Clone, Default)]
pub struct RestoreOptions {}

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
    /// 別人放的東西）。
    fn create_in(dir: &DirHandle) -> Result<Self> {
        let name = temp_name()?;
        // 開檔成功才交給 TempPath：失敗時那個名字不是我們的，drop 不能去刪它。
        let file = dir
            .create_new_file(&name)
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
/// （ADR 019 A2 裁定：擋路的 symlink 維持拒絕）。
fn refuse_in_the_way(dir: &DirHandle, name: &OsStr) -> Result<()> {
    let what = match dir.kind_of(name)? {
        None | Some(Kind::File) => return Ok(()),
        Some(Kind::Symlink) => "a symlink",
        Some(Kind::Dir) => "a directory",
        Some(Kind::Special) => "a special file",
    };
    Err(CoreError::Corrupt {
        key: dir.child_path(name).display().to_string(),
        reason: format!("{what} is in the way of a restored file"),
    })
}

/// ADR 019 A2：還原到既有、擁有者不可寫的目錄（例如同一個 snapshot 先前還原
/// 出的 0o555）時，子項目的暫存檔建不進去。這個目錄記錄的 mode 會在子項目
/// 寫完後重新套上（[`apply_meta`]），所以先暫時加上擁有者寫入權，回傳原本的
/// 權限（套 metadata 失敗時放回去）。不會被重新套 mode 的目錄（s3/generic
/// 來源沒有 mode 或 mtime）不動。fstat 與 chmod 都經往下走時開的那個 handle
/// （ADR 019 A3、A4）。
#[cfg(unix)]
fn make_owner_writable(dir: &DirHandle, node: &Entry) -> Result<Option<std::fs::Permissions>> {
    use std::os::unix::fs::PermissionsExt;
    let mode_will_be_applied = node.mtime_ns.is_some() && node.mode.is_some_and(|m| m != 0);
    if !mode_will_be_applied {
        return Ok(None);
    }
    let handle = dir.meta_handle()?;
    let original = handle
        .metadata()
        .map_err(|e| CoreError::io(dir.path(), e))?
        .permissions();
    if original.mode() & 0o200 != 0 {
        return Ok(None);
    }
    let writable = std::fs::Permissions::from_mode((original.mode() & 0o7777) | 0o200);
    // chmod 失敗（例如目錄不是我們的）不擋整個目錄：群組或其他人的寫入權
    // 也許就夠；不夠的話，每個子項目各自回報寫不進去。
    if let Err(e) = handle.set_permissions(writable) {
        tracing::warn!(
            "{}: cannot make the existing directory writable for the restore: {e}",
            dir.path().display()
        );
        return Ok(None);
    }
    Ok(Some(original))
}

/// 非 unix 沒有 mode 可還原（fsmeta 的 apply_mode 是空的），不需要。
#[cfg(not(unix))]
fn make_owner_writable(_: &DirHandle, _: &Entry) -> Result<Option<std::fs::Permissions>> {
    Ok(None)
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
        _opts: RestoreOptions,
    ) -> Result<RestoreSummary> {
        let snapshot = self.read_snapshot(snapshot_key).await?;
        let index = ReloadableIndex::new(self.load_index().await?);
        std::fs::create_dir_all(target).map_err(|e| CoreError::io(target, e))?;
        // 目標本身的 handle，整個 restore 都開著：每個 root 從它往下走；硬連結
        // 跨 roots，後續名字也從它重新找第一個名字。
        let target_dir = DirHandle::open_target(target)?;
        let mut summary = RestoreSummary::default();
        // 範圍是**整個 snapshot、跨 roots**（docs/format.md §8.3）。
        let mut hardlinks = HardLinks::new();
        for root in &snapshot.roots {
            let rel = fsmeta::locator_to_relative(root.path.as_slice())?;
            let entries = self.read_tree_chain(&root.tree).await?;
            // v3 的 restore 映射（docs/format.md §9）：目錄來源的 entries 放在
            // `target/<locator>` 之下；**檔案/symlink 來源**（root tree 恰好一個
            // 非目錄 entry、名稱 = 定位的末段）落在 `target/<locator 去掉末段>/`
            // ——與 v2「絕對路徑還原」的落點完全一致，而「目錄恰好只含一個同名
            // 檔案」的還原結果也相同，所以這個判別沒有歧義代價。
            let file_root = entries.len() == 1
                && entries[0].kind != node_type::DIR
                && rel.file_name().map(|f| f.as_encoded_bytes().to_vec())
                    == Some(entries[0].name.clone());
            let base_rel = if file_root {
                rel.parent().unwrap_or(Path::new(""))
            } else {
                rel.as_path()
            };
            let base = target_dir.create_dirs(base_rel)?;
            for entry in entries {
                // v3：節點名一律是單一路徑元件（合成根已淘汰）。
                fsmeta::validate_child_name(&entry.name)?;
                let name = fsmeta::bytes_to_name(&entry.name)?;
                self.restore_node(
                    &entry,
                    &base,
                    &name,
                    1,
                    &target_dir,
                    &index,
                    &mut summary,
                    &mut hardlinks,
                )
                .await;
            }
        }
        Ok(summary)
    }

    /// 還原一個節點：`dir` 這一層裡的 `name`。錯誤記進 summary，不往上拋：一個
    /// 壞掉的 chunk 不該讓其他 99% 的檔案也拿不回來。
    /// `depth`：DIR 巢狀深度（root 的子女 = 1）；超過 [`MAX_TREE_DEPTH`] 的
    /// chain 只能出自腐壞或敵意 repo——記錄錯誤、不深入，否則遞迴會把
    /// process 墊進 stack overflow。這個上限同時是同時開著的目錄 handle 數的
    /// 上限（每層一個，ADR 019 A4）。
    #[allow(clippy::too_many_arguments)] // depth 與目標的 handle 都得帶進遞迴
    fn restore_node<'a>(
        &'a self,
        node: &'a Entry,
        dir: &'a DirHandle,
        name: &'a OsStr,
        depth: usize,
        target_dir: &'a DirHandle,
        index: &'a ReloadableIndex,
        summary: &'a mut RestoreSummary,
        hardlinks: &'a mut HardLinks,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + 'a>> {
        Box::pin(async move {
            // 完整路徑只用在錯誤訊息；實際操作都經 `dir`（ADR 019 A4）。
            let path = dir.child_path(name);
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
                        self.restore_dir(
                            node, dir, name, depth, target_dir, index, summary, hardlinks,
                        )
                        .await
                    }
                }
                node_type::FILE => {
                    let hardlink_key = (node.nlink.unwrap_or(0) > 1)
                        .then_some((node.dev.unwrap_or(0), node.inode.unwrap_or(0)))
                        .filter(|k| k.1 != 0);
                    if let Some(k) = hardlink_key {
                        if let Some(first) = hardlinks.get(&k) {
                            match link_to_first(target_dir, first, dir, name) {
                                Ok(()) => {
                                    summary.files += 1;
                                    return;
                                }
                                Err(e) => {
                                    let first_path =
                                        target_dir.path().join(first.iter().collect::<PathBuf>());
                                    tracing::warn!(
                                        "{}: cannot hard-link to {}: {e}; restoring a copy",
                                        path.display(),
                                        first_path.display()
                                    );
                                }
                            }
                        }
                    }
                    // 硬連結失敗後的複製也走這裡：同樣是暫存檔＋rename（ADR 019 A2）。
                    match self
                        .restore_file(dir, name, node.size, &node.chunks, node.content, index)
                        .await
                    {
                        Ok(temp) => {
                            #[cfg(all(test, unix))]
                            meta_via_handle_tests::before_meta(&temp.temp.path(), &path);
                            // metadata 經寫入內容的同一個 handle 套用（ADR 019 A3）：
                            // 路徑在寫完之後被換成 symlink，也改不到外面的檔。
                            // 順序與擁有者見 apply_meta（ADR 019 A43）。
                            let (applied, owner) = apply_meta(&temp.file, &path, node);
                            let meta = applied.and(owner);
                            // metadata 套不上（例如目標檔案系統不收 xattr）時內容仍是
                            // 對的：照樣 rename 成正式名、記下這個錯——與以前在正式名
                            // 上原地寫時一樣。先關檔再 rename：Windows 上 rename 開著
                            // 的檔要看共用模式，關掉最單純；unix 沒有差別。
                            let TempFile { file, temp } = temp;
                            drop(file);
                            match temp.commit(name) {
                                Ok(()) => {
                                    summary.files += 1;
                                    if let Some(k) = hardlink_key {
                                        hardlinks.insert(k, dir.child_rel(name));
                                    }
                                    meta
                                }
                                Err(e) => Err(e),
                            }
                        }
                        // 暫存檔已隨錯誤被 drop 刪掉；正式名底下原有的檔沒被碰過，
                        // 不能再像以前那樣 remove_file(path)（ADR 019 A2）。
                        Err(e) => Err(e),
                    }
                }
                node_type::SYMLINK => match fsmeta::bytes_to_name(&node.target) {
                    Ok(link_target) => {
                        match replace_with_symlink(dir, name, Path::new(&link_target)) {
                            Ok(()) => {
                                summary.symlinks += 1;
                                if node.xattrs.is_some() {
                                    tracing::warn!(
                                        "{}: snapshot has extended attributes for this symlink;                                      Linux cannot set user.* on a symlink and following it would                                      write to the target, so they are not restored",
                                        path.display()
                                    );
                                }
                                apply_symlink_meta(dir, name, node)
                            }
                            Err(e) => Err(e),
                        }
                    }
                    Err(e) => Err(e),
                },
                other => Err(CoreError::Corrupt {
                    key: path.display().to_string(),
                    reason: format!("unknown node type {other}"),
                }),
            };
            if let Err(e) = result {
                tracing::warn!("{}: {e}", path.display());
                summary.errors.push(format!("{}: {e}", path.display()));
            }
        })
    }

    /// 還原 `parent` 這一層裡的目錄 `name`：建立（或沿用）並開著它，子項目都
    /// 經這個 handle 建立，metadata 最後也套在它上面（ADR 019 A3、A4）。這一層
    /// 之後被搬走、原位換成 symlink，子項目與 metadata 都跟著真目錄走，不會
    /// 落到 symlink 指的地方。handle 在子項目還原期間一直開著：同時開著的
    /// 數目是遞迴深度，不是 entry 數。
    #[allow(clippy::too_many_arguments)] // 同 restore_node
    async fn restore_dir(
        &self,
        node: &Entry,
        parent: &DirHandle,
        name: &OsStr,
        depth: usize,
        target_dir: &DirHandle,
        index: &ReloadableIndex,
        summary: &mut RestoreSummary,
        hardlinks: &mut HardLinks,
    ) -> Result<()> {
        let dir = parent.create_child_dir(name)?;
        let children = self.read_tree_chain(&node.subtree).await?;
        // 既有的唯讀目錄先暫時加上擁有者寫入權（ADR 019 A2）。放在讀完子 tree
        // 之後：從這裡到下面重新套 mode 之間沒有提早 return——子項目的錯誤記進
        // summary、不往上拋；迴圈裡的 bytes_to_name 在 unix（唯一會加寫入權的
        // 平台）不會失敗。
        let loosened = make_owner_writable(&dir, node)?;
        for child in children {
            if let Err(e) = fsmeta::validate_child_name(&child.name) {
                summary
                    .errors
                    .push(format!("{}: {e}", dir.path().display()));
                continue;
            }
            let child_name = fsmeta::bytes_to_name(&child.name)?;
            self.restore_node(
                &child,
                &dir,
                &child_name,
                depth + 1,
                target_dir,
                index,
                summary,
                hardlinks,
            )
            .await;
        }
        summary.dirs += 1;
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

    /// 把檔案內容寫進 `dir` 這一層的暫存檔，回傳它（ADR 019 A2）：呼叫端在同一個
    /// handle 上套 metadata（ADR 019 A3），再 commit（rename）成正式名 `name`。
    /// 任何一步失敗，暫存檔隨錯誤被 drop 刪掉，正式名底下原有的檔不受影響。
    async fn restore_file(
        &self,
        dir: &DirHandle,
        name: &OsStr,
        size: u64,
        chunks: &[ChunkId],
        content: u8,
        index: &ReloadableIndex,
    ) -> Result<TempFile> {
        let path = dir.child_path(name);
        // 先看一眼正式名處：擋著 symlink 之類就不必下載內容（commit 前會再看一次）。
        refuse_in_the_way(dir, name)?;
        let chunk_ids = if content == content_type::DIRECT {
            chunks.to_vec()
        } else {
            let mut bytes = Vec::new();
            for id in chunks {
                bytes.extend_from_slice(&self.read_chunk_reloading(id, index).await?);
            }
            let list = parse_chunk_list(&bytes).map_err(|e| CoreError::Corrupt {
                key: "<chunk list>".to_owned(),
                reason: e.to_string(),
            })?;
            list.chunks
        };
        // 內容寫進新建的暫存檔，不開正式名：以前以 write＋create＋truncate
        // 開正式名，失敗後呼叫端再 remove_file，連還沒開檔就失敗（間接內容的
        // 清單讀不到）的情況也會刪掉使用者原有的檔。
        let temp = TempFile::create_in(dir)?;
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
        if written != size {
            return Err(CoreError::Corrupt {
                key: path.display().to_string(),
                reason: format!("restored {written} bytes but snapshot says {size}"),
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
        let list = parse_chunk_list(&bytes).map_err(|e| CoreError::Corrupt {
            key: "<chunk list>".to_owned(),
            reason: e.to_string(),
        })?;
        Ok(list.chunks)
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

/// 建 symlink 前先移除既有的檔案或 symlink（第二次 restore 到同一目錄）；既有的是
/// 目錄則回錯。都經 `dir` 這一層的 handle（ADR 019 A4）。
fn replace_with_symlink(dir: &DirHandle, name: &OsStr, link_target: &Path) -> Result<()> {
    match dir.kind_of(name) {
        Ok(Some(Kind::Dir)) => {
            return Err(CoreError::Corrupt {
                key: dir.child_path(name).display().to_string(),
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
    }

    /// restore 在內容寫完（目錄：子項目都寫完）、metadata 還沒套之前呼叫。
    /// `written` 是內容實際所在的路徑：檔案是暫存檔（ADR 019 A2），目錄就是
    /// `path`。`path` 符合設定時，把 `written` 搬到旁邊（`<path>.moved`），
    /// 原位換成指向外面的 symlink——模擬本機攻擊者在這個空檔動手。
    pub(super) fn before_meta(written: &Path, path: &Path) {
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
