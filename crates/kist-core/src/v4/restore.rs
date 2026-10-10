//! v4 的 restore：把一個 snapshot 還原到目標目錄（§9 的落點、§10 的讀取）。
//!
//! 落點照 `Root.t`：目錄 root 的內容放在 `<目標>/<定位的所有段>`（沒有段就直接放在目標），檔案與符號連結放在
//! `<目標>/<去掉最後一段>/<名稱>`；不再從內容猜（v3 的 `is_file_root` 與第二輪判斷）。讀取經 [`Live`]：一項的
//! 持有者全部失敗時重新載入一次世代再試。部分還原（include、exclude、SNAPSHOT:PATH）與 dry run 的規則同 v3
//! （ADR 020、022、023）；SNAPSHOT:PATH 在 root 裡面時沿 Dir 的索引查找，只讀涵蓋路徑的段。
//!
//! 檔案系統這一層（暫存檔加 rename、擋路的東西、目錄 handle、硬連結、唯讀目錄、選取、metadata 的順序）與 v3
//! 共用 `crate::restore`（ADR 019），v3 移除時搬進來；這裡只有走訪。

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::io::Write;
use std::path::{Path, PathBuf};

use kist_format::v4::ids::MetaId;
use kist_format::v4::keys::Key;
use kist_format::v4::meta::{node_type, Entry};
use kist_format::v4::snapshot::{split_locator, Root};
use serde_bytes::ByteBuf;

use super::live::Live;
use super::repo::Repo;
use crate::dirhandle::DirHandle;
use crate::restore::{
    apply_meta, link_to_first, not_a_directory, refuse_in_the_way, restore_symlink,
    subtree_display, temp_file_mode, HardLinks, OpenDir, PendingDir, Plan, PlanLevel, Planned, Pos,
    RestoreEntry, RestoreOptions, RestoreSummary, Select, TempFile,
};
use crate::{fsmeta, CoreError, Result, MAX_TREE_DEPTH};

impl RestoreEntry for Entry {
    fn name(&self) -> &[u8] {
        &self.n
    }

    fn is_dir(&self) -> bool {
        self.t == node_type::DIR
    }

    fn target(&self) -> &[u8] {
        &self.target
    }

    fn mode(&self) -> Option<u32> {
        self.mode
    }

    fn mtime_ns(&self) -> Option<i64> {
        self.mtime
    }

    fn xattrs(&self) -> Option<&BTreeMap<ByteBuf, ByteBuf>> {
        self.xattrs.as_ref()
    }

    fn owner(&self) -> Option<(u32, u32)> {
        self.uid.zip(self.gid)
    }

    fn fs_meta(&self) -> fsmeta::FsMeta {
        fsmeta::FsMeta {
            mode: self.mode.unwrap_or(0),
            uid: self.uid.unwrap_or(0),
            gid: self.gid.unwrap_or(0),
            mtime_ns: self.mtime.unwrap_or(0),
            ctime_ns: self.ctime.unwrap_or(0),
            inode: self.ino.unwrap_or(0),
            dev: self.dev.unwrap_or(0),
            nlink: self.nlink.unwrap_or(0),
        }
    }
}

/// 一個 root 在這次還原裡的落點。
struct Placement {
    /// 落點目錄：目標底下的相對路徑。
    rel: PathBuf,
    /// 落點在選取上的位置。
    pos: Pos,
    content: Content,
}

/// 要放進落點的條目。
enum Content {
    /// 目錄的內容，第二輪才讀：同時只有一個 root 的條目在記憶體裡。
    Tree(MetaId),
    /// 檔案或符號連結 root 的 Entry（在 root 裡，不必讀）。
    Entry(Box<Entry>),
}

enum Placed {
    /// 這個 root 不在 SNAPSHOT:PATH 之下，也不包含它。
    Elsewhere,
    /// 與 SNAPSHOT:PATH 有關，但選取上整個不必走（被排除，或底下不可能有東西被選到）。
    Pruned,
    Here(Box<Placement>),
}

/// restore 走訪時一路帶著的東西。
struct Walk<'w> {
    /// 還原目標本身：硬連結的後續名字從這裡沿路徑重新找第一個名字。
    target_dir: DirHandle,
    live: &'w Live,
    summary: RestoreSummary,
    /// 範圍是整個 snapshot、跨 roots（§8）。
    hardlinks: HardLinks,
    select: Select<'w>,
}

/// 定位的段 → 目標底下的相對路徑。段已照 §9 切好（沒有空段、`.`，`..` 已映射）；每一段再照目錄項的規則檢查
/// （Windows 上的反斜線）。
fn relative(segs: &[Vec<u8>]) -> Result<PathBuf> {
    segs.iter()
        .map(|s| {
            fsmeta::validate_child_name(s)?;
            fsmeta::bytes_to_name(s)
        })
        .collect()
}

fn deep(path: &Path) -> CoreError {
    CoreError::corrupt(
        path.display().to_string(),
        format!(
            "directory nesting deeper than {MAX_TREE_DEPTH} levels (corrupt or hostile repository)"
        ),
    )
}

impl Repo {
    /// 還原 `snapshot` 到 `target`。目標最好是空的：既有檔案會被取代（先寫同目錄的暫存檔、成功才 rename，失敗時
    /// 原有的檔不動）。目標之下的路徑不穿過符號連結（既有的或 snapshot 自己種的都一樣），每一層以目錄 handle 為
    /// 錨往下走；`target` 本身與其上是使用者的路徑，照常跟隨（ADR 019）。單一項目失敗記在 `errors`，其他照常
    /// 還原。
    pub async fn restore(
        &self,
        snapshot: &Key,
        target: &Path,
        opts: RestoreOptions,
    ) -> Result<RestoreSummary> {
        let select = Select::new(&opts)?;
        let live = self.live().await?;
        let snap = self.read_snapshot(snapshot).await?;
        let roots = live.run(async |r| r.roots(&snap.roots).await).await?;
        // 第一輪：每個 root 的落點。SNAPSHOT:PATH 不對時在建立任何目錄之前就回錯。
        let placements = place_roots(&live, &roots, &opts.subtree, &select).await?;
        std::fs::create_dir_all(target).map_err(|e| CoreError::io(target, e))?;
        // 目標本身的 handle，整個 restore 都開著：每個 root 從它往下走；硬連結跨 roots，後續名字也從它重新找
        // 第一個名字。兩份是同一個 fd。
        let mut top = DirHandle::open_target(target)?;
        let mut w = Walk {
            target_dir: top.clone(),
            live: &live,
            summary: RestoreSummary::default(),
            hardlinks: HardLinks::new(),
            select,
        };
        for placement in placements {
            let (rel, pos, entries) = open_placement(&live, placement, &mut w.select).await?;
            // 落點建不起來（例如擋著符號連結）：整個 restore 回錯，不是某一項的錯。沒被選到的落點等到底下真的
            // 有東西要寫才建。
            let mut base = PendingDir::base(&mut top, &rel);
            if pos.selected {
                base.create()?;
            }
            for entry in entries {
                // 名稱在解碼時已照 §8 檢查；會到這裡的只有 Windows 上帶反斜線的名字。記錯、其他照常。
                if let Err(e) = fsmeta::validate_child_name(&entry.n) {
                    w.summary
                        .errors
                        .push(format!("{}: {e}", base.location().display()));
                    continue;
                }
                let Some(child) = w.select.visit(&pos, &entry) else {
                    continue;
                };
                let name = fsmeta::bytes_to_name(&entry.n)?;
                if child.selected {
                    base.open()?;
                }
                restore_node(&mut w, &mut base, &entry, &name, 1, child).await;
                // 落點在更深處才建、建不起來時，與一開始就建不起來一樣整個回錯；再建一次拿到它自己的錯誤。
                if base.create_failed {
                    base.create()?;
                }
            }
        }
        w.summary.unmatched_includes = w.select.unmatched();
        Ok(w.summary)
    }

    /// dry run：與 [`Repo::restore`] 同樣的落點與選取，但什麼都不寫，連 `target` 都不建。每個會被還原的節點依
    /// 走訪順序交給 `on_node`（路徑是 `target` 底下的完整路徑，目錄在它的內容之前）；計數與
    /// `unmatched_includes` 與真的還原相同。只讀目錄、不讀檔案內容，所以只有真的還原才會遇到的錯誤（壞掉的
    /// chunk、目標處擋著的東西）這裡看不到。
    pub async fn plan_restore(
        &self,
        snapshot: &Key,
        target: &Path,
        opts: RestoreOptions,
        on_node: &mut dyn FnMut(&Path, Planned),
    ) -> Result<RestoreSummary> {
        let select = Select::new(&opts)?;
        let live = self.live().await?;
        let snap = self.read_snapshot(snapshot).await?;
        let roots = live.run(async |r| r.roots(&snap.roots).await).await?;
        let placements = place_roots(&live, &roots, &opts.subtree, &select).await?;
        let mut p = Plan {
            select,
            summary: RestoreSummary::default(),
            on_node,
            levels: Vec::new(),
        };
        for placement in placements {
            let (rel, pos, entries) = open_placement(&live, placement, &mut p.select).await?;
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
                if let Err(e) = fsmeta::validate_child_name(&entry.n) {
                    p.summary.errors.push(format!("{}: {e}", base.display()));
                    continue;
                }
                let Some(child) = p.select.visit(&pos, &entry) else {
                    continue;
                };
                let path = base.join(fsmeta::bytes_to_name(&entry.n)?);
                if child.selected {
                    p.open();
                }
                plan_node(&live, &mut p, &entry, path, 1, child).await;
            }
            p.levels.pop();
        }
        p.summary.unmatched_includes = p.select.unmatched();
        Ok(p.summary)
    }
}

/// 第一輪：每個 root 的落點。SNAPSHOT:PATH 不在任何 root 裡 → 錯誤；這時還沒建任何東西。
async fn place_roots(
    live: &Live,
    roots: &[Root],
    subtree: &[Vec<u8>],
    select: &Select<'_>,
) -> Result<Vec<Placement>> {
    let mut placements = Vec::new();
    let mut found = subtree.is_empty();
    for root in roots {
        match place_root(live, root, subtree, select).await? {
            Placed::Elsewhere => {}
            Placed::Pruned => found = true,
            Placed::Here(p) => {
                found = true;
                placements.push(*p);
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

/// 一個 root 的落點（§9）。SNAPSHOT:PATH 不是空的時，落點是去掉它之後的部分；它在目錄 root 裡面時見
/// [`place_inside`]。
async fn place_root(
    live: &Live,
    root: &Root,
    subtree: &[Vec<u8>],
    select: &Select<'_>,
) -> Result<Placed> {
    let segs = split_locator(&root.path)?;
    // 定位與 SNAPSHOT:PATH 必須有一個是另一個的開頭，否則這個 root 跟這次還原無關。
    let shared = segs
        .iter()
        .zip(subtree)
        .take_while(|(seg, wanted)| seg == wanted)
        .count();
    if shared < segs.len().min(subtree.len()) {
        return Ok(Placed::Elsewhere);
    }
    let corrupt = |why: &str| CoreError::corrupt(String::from_utf8_lossy(&root.path), why);
    if root.t == node_type::DIR {
        let tree = root
            .tree
            .ok_or_else(|| corrupt("a directory root without a tree"))?;
        let Some((last, above)) = segs.split_last().filter(|_| subtree.len() < segs.len()) else {
            return place_inside(live, tree, &segs, subtree, select).await;
        };
        // SNAPSHOT:PATH 是定位的上層。定位或它的上層被排除、或底下不可能有東西被選到：連目錄都不讀。
        let Some(parent) = select.walk(above.iter().map(Vec::as_slice)) else {
            return Ok(Placed::Pruned);
        };
        let pos = match select.child(&parent, last) {
            Some(pos) if Select::worth_walking(&pos) => pos,
            _ => return Ok(Placed::Pruned),
        };
        return Ok(Placed::Here(Box::new(Placement {
            rel: relative(&segs[subtree.len()..])?,
            pos,
            content: Content::Tree(tree),
        })));
    }
    // 檔案或符號連結：放在去掉最後一段的目錄，名稱是 entry 的（§9 保證等於最後一段）。
    let entry = root
        .entry
        .clone()
        .ok_or_else(|| corrupt("a file or symlink root without its entry"))?;
    let dir = &segs[..segs.len().saturating_sub(1)];
    if subtree.len() > dir.len() {
        // SNAPSHOT:PATH 是這個檔案本身，或穿過它
        return Err(not_a_directory(subtree));
    }
    match select.walk(dir.iter().map(Vec::as_slice)) {
        Some(pos) if Select::worth_walking(&pos) => Ok(Placed::Here(Box::new(Placement {
            rel: relative(&dir[subtree.len()..])?,
            pos,
            content: Content::Entry(Box::new(entry)),
        }))),
        _ => Ok(Placed::Pruned),
    }
}

/// SNAPSHOT:PATH 是這個目錄 root 本身或在它底下：先沿目錄確認那個路徑存在、是目錄（每一層只讀涵蓋名稱的段），
/// 再看選取：打錯的路徑不能因為 root 被選取剪掉，就以「什麼都沒還原」成功結束。它的內容直接放進目標本身。
async fn place_inside(
    live: &Live,
    tree: MetaId,
    segs: &[Vec<u8>],
    subtree: &[Vec<u8>],
    select: &Select<'_>,
) -> Result<Placed> {
    let mut tree = tree;
    for wanted in &subtree[segs.len()..] {
        let current = tree;
        let Some(entry) = live.run(async |r| r.lookup(&current, wanted).await).await? else {
            return Ok(Placed::Elsewhere);
        };
        if entry.t != node_type::DIR {
            return Err(not_a_directory(subtree));
        }
        tree = entry.tree.ok_or_else(|| {
            CoreError::corrupt(subtree_display(subtree), "a directory without a tree")
        })?;
    }
    match select.walk(subtree.iter().map(Vec::as_slice)) {
        Some(pos) if Select::worth_walking(&pos) => Ok(Placed::Here(Box::new(Placement {
            rel: PathBuf::new(),
            pos,
            content: Content::Tree(tree),
        }))),
        _ => Ok(Placed::Pruned),
    }
}

/// 第二輪：讀落點的條目。落點被選到就算還原了（空的目錄 root 也是），在這裡記 include 選到東西，真的還原與
/// dry run 一致。
async fn open_placement(
    live: &Live,
    placement: Placement,
    select: &mut Select<'_>,
) -> Result<(PathBuf, Pos, Vec<Entry>)> {
    let entries = match placement.content {
        Content::Tree(id) => live.run(async |r| r.entries(&id).await).await?,
        Content::Entry(e) => vec![*e],
    };
    select.credit(&placement.pos);
    Ok((placement.rel, placement.pos, entries))
}

/// dry run 的一個節點，對應 [`restore_node`]（同樣的分派與錯誤記錄）。
fn plan_node<'a, 'p: 'a, 'o: 'a>(
    live: &'a Live,
    p: &'a mut Plan<'p, 'o>,
    node: &'a Entry,
    path: PathBuf,
    depth: usize,
    pos: Pos,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + 'a>> {
    Box::pin(async move {
        let result = match node.t {
            node_type::DIR if depth >= MAX_TREE_DEPTH => Err(deep(&path)),
            node_type::DIR => plan_dir(live, p, node, &path, depth, pos).await,
            _ => {
                p.open();
                let planned = if node.t == node_type::FILE {
                    p.summary.files += 1;
                    Planned::File
                } else {
                    p.summary.symlinks += 1;
                    Planned::Symlink
                };
                (p.on_node)(&path, planned);
                Ok(())
            }
        };
        if let Err(e) = result {
            p.summary.errors.push(format!("{}: {e}", path.display()));
        }
    })
}

/// dry run 的目錄，對應 [`restore_dir`]：被選到的一進來就算建了，沒被選到的等底下有東西才算；建了的在走完之後
/// 計數。
async fn plan_dir(
    live: &Live,
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
    let result = plan_children(live, p, node, path, depth, &pos).await;
    let created = p.levels.pop().is_some_and(|level| level.created);
    result?;
    if created {
        p.summary.dirs += 1;
    }
    Ok(())
}

async fn plan_children(
    live: &Live,
    p: &mut Plan<'_, '_>,
    node: &Entry,
    path: &Path,
    depth: usize,
    pos: &Pos,
) -> Result<()> {
    let tree = dir_tree(node, path)?;
    let children = live.run(async |r| r.entries(&tree).await).await?;
    for child in children {
        if let Err(e) = fsmeta::validate_child_name(&child.n) {
            p.summary.errors.push(format!("{}: {e}", path.display()));
            continue;
        }
        let Some(child_pos) = p.select.visit(pos, &child) else {
            continue;
        };
        let child_path = path.join(fsmeta::bytes_to_name(&child.n)?);
        if child_pos.selected {
            p.open();
        }
        plan_node(live, p, &child, child_path, depth + 1, child_pos).await;
    }
    Ok(())
}

/// 目錄 entry 的 tree（§8：目錄一定有；解碼時已檢查）。
fn dir_tree(node: &Entry, path: &Path) -> Result<MetaId> {
    node.tree
        .ok_or_else(|| CoreError::corrupt(path.display().to_string(), "a directory without a tree"))
}

/// 還原一個節點：`dir` 這一層裡的 `name`。錯誤記進 summary、不往上拋：一個壞掉的 chunk 不該讓其他檔案也拿不
/// 回來。`depth` 是目錄的巢狀深度（root 的子項目是 1），超過 [`MAX_TREE_DEPTH`] 只能出自腐壞或敵意的 repo：
/// 記錯、不深入，否則遞迴會把 stack 用光；它同時是同時開著的目錄 handle 數的上限。`pos` 是這個節點在選取上的
/// 位置（檔案與符號連結一定是被選到的，目錄可能只是底下也許有東西被選到）。
fn restore_node<'a, 'w: 'a>(
    w: &'a mut Walk<'w>,
    dir: &'a mut dyn OpenDir,
    node: &'a Entry,
    name: &'a OsStr,
    depth: usize,
    pos: Pos,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + 'a>> {
    Box::pin(async move {
        // 完整路徑只用在錯誤訊息；實際操作都經 `dir`。
        let path = dir.location().join(name);
        let result = match node.t {
            node_type::DIR if depth >= MAX_TREE_DEPTH => Err(deep(&path)),
            node_type::DIR => restore_dir(w, dir, node, name, depth, pos).await,
            node_type::FILE => match dir.open() {
                Ok(dir) => restore_file_entry(w, node, dir, name).await,
                Err(e) => Err(e),
            },
            _ => dir
                .open()
                .and_then(|dir| restore_symlink(dir, name, node, &mut w.summary)),
        };
        if let Err(e) = result {
            tracing::warn!("{}: {e}", path.display());
            w.summary.errors.push(format!("{}: {e}", path.display()));
        }
    })
}

/// 還原一個檔案：硬連結的第二個以後的名字先試 link 到第一個，不成就退回複製；複製走暫存檔加 rename。commit
/// 成功才計數、才記進硬連結表——metadata 套不上時也是（內容已經對了，錯另外回）。
async fn restore_file_entry(
    w: &mut Walk<'_>,
    node: &Entry,
    dir: &DirHandle,
    name: &OsStr,
) -> Result<()> {
    let path = dir.child_path(name);
    let hardlink_key = (node.nlink.unwrap_or(0) > 1)
        .then_some((node.dev.unwrap_or(0), node.ino.unwrap_or(0)))
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
    let temp = restore_file(w.live, dir, name, node).await?;
    // metadata 經寫入內容的同一個 handle 套用：路徑在寫完之後被換成符號連結，也改不到外面的檔。
    let (applied, owner) = apply_meta(&temp.file, &path, node);
    // metadata 套不上時內容仍是對的：照樣 rename 成正式名、記下這個錯。先關檔再 rename（Windows）。
    let TempFile { file, temp } = temp;
    drop(file);
    temp.commit(name)?;
    w.summary.files += 1;
    if let Some(k) = hardlink_key {
        w.hardlinks.insert(k, dir.child_rel(name));
    }
    applied.and(owner)
}

/// 還原 `parent` 這一層裡的目錄 `name`：建立（或沿用）並開著它，子項目都經這個 handle 建立，metadata 最後也套
/// 在它上面。沒被選到的目錄先不建：底下第一次有東西要寫才沿路建上來；到最後都沒建的就不計數、不套 metadata。
async fn restore_dir(
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
    let tree = dir_tree(node, dir.location())?;
    let children = w.live.run(async |r| r.entries(&tree).await).await?;
    // 既有的唯讀目錄在第一個子項目寫進去之前暫時加上擁有者寫入權（PendingDir::open）。在讀完子目錄之後：從那裡
    // 到下面重新套 mode 之間沒有提早 return。
    for child in children {
        if let Err(e) = fsmeta::validate_child_name(&child.n) {
            w.summary
                .errors
                .push(format!("{}: {e}", dir.location().display()));
            continue;
        }
        let Some(child_pos) = w.select.visit(&pos, &child) else {
            continue;
        };
        let child_name = fsmeta::bytes_to_name(&child.n)?;
        if child_pos.selected {
            dir.open()?;
        }
        restore_node(w, &mut dir, &child, &child_name, depth + 1, child_pos).await;
    }
    // 套 metadata 之前，既有的唯讀目錄也要先暫時加寫入權（設 user.* xattr 要寫入權）：一個子項目都沒寫進去時
    // 還沒加過。
    if dir.handle.is_some() {
        dir.open()?;
    }
    let PendingDir {
        handle, loosened, ..
    } = dir;
    let Some(dir) = handle else {
        return Ok(());
    };
    w.summary.dirs += 1;
    // 子項目都寫完後才設目錄的 mtime；沒有要套的 metadata（s3／generic 來源的目錄）就不動。
    if node.xattrs.is_none() && node.mtime.is_none() {
        return Ok(());
    }
    let handle = dir.meta_handle()?;
    let (applied, owner) = apply_meta(&handle, dir.path(), node);
    // 記錄的 mode 沒套上（xattr 或時間先失敗）：至少把暫時加的寫入權拿掉。
    if applied.is_err() {
        if let Some(original) = loosened {
            let _ = handle.set_permissions(original);
        }
    }
    applied.and(owner)
}

/// 把檔案 `node` 的內容寫進 `dir` 這一層的暫存檔，回傳它：呼叫端在同一個 handle 上套 metadata，再 rename 成
/// 正式名。任何一步失敗，暫存檔隨錯誤被 drop 刪掉，正式名底下原有的檔不受影響。
async fn restore_file(
    live: &Live,
    dir: &DirHandle,
    name: &OsStr,
    node: &Entry,
) -> Result<TempFile> {
    let path = dir.child_path(name);
    // 先看一眼正式名處：擋著符號連結之類就不必下載內容（commit 前會再看一次）。
    refuse_in_the_way(dir, name)?;
    // chunk 清單與 §7 的大小檢查（每段的 size、合計等於 Entry 的 size）
    let chunks = live.run(async |r| r.file_chunks(node).await).await?;
    let temp = TempFile::create_in(dir, temp_file_mode(dir, name, node)?)?;
    let mut writer = std::io::BufWriter::new(&temp.file);
    let mut written = 0u64;
    for id in &chunks {
        let data = live.run(async |r| r.chunk(id).await).await?;
        writer
            .write_all(&data)
            .map_err(|e| CoreError::io(&path, e))?;
        written += data.len() as u64;
    }
    writer.flush().map_err(|e| CoreError::io(&path, e))?;
    drop(writer);
    if written != node.size {
        return Err(CoreError::corrupt(
            path.display().to_string(),
            format!(
                "restored {written} bytes but the snapshot says {}",
                node.size
            ),
        ));
    }
    Ok(temp)
}
