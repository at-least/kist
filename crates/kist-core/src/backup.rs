//! backup：走訪目錄、切塊、去重、寫 pack / tree / index / snapshot（v3）。
//!
//! 流程（寫入順序是刻意的，見 `docs/format-v3-draft.md` §13、§17）：
//! 1. 讀進所有 index。
//! 2. 找同一台 client、同一組 roots 的上一個 snapshot 當 parent：
//!    檔案的 size 與 mtime 沒變就直接沿用它的 chunk 清單，不重讀檔案。
//! 3. 依名稱排序遞迴走訪。檔案在 blocking thread 裡串流切塊、算 ID、對 index 去重、
//!    新 chunk 壓縮加密進 pack；pack 滿了就交回 async 端上傳（最多 2 個同時在飛）。
//! 4. 每個目錄結束時封成 tree 上傳：**新樹** put_if_absent；**沿用的既有樹**
//!    以覆寫式 Put 寫 `touch/<id>`（8 bytes）刷新 mtime——那是復活訊號本體
//!    （v2 每次 backup 重 put 整棵樹的 bytes，v3 樹 bytes 永不重寫）。
//! 5. 全部結束：flush 最後一個 pack、等上傳完成、寫 index blob（到這裡是 `backup_prepare`）。
//! 6. `commit`：確認引用到的每個 pack 都還在、有標記的可達樹 touch 夠新，然後
//!    寫 snapshot（replicas=1 時 `.r1` 副本先寫，主體出現＝commit）。
//!
//! v3 的 roots：每個備份來源是 `Root { path, tree }`——path 是不透明定位
//! （本機絕對路徑；遠端來源是 `sftp://…`/`s3://…`，見 Source 抽象），
//! 樹節點名**一律**是單一路徑元件（v2 的合成根已淘汰）。
//!
//! 面對 GC：
//! - 開始時列 `gc/`：被標記的 pack **不拿來去重**，裡面的 chunk 重寫一份。backup 因此不需要
//!   刪標記（維持 Put-only），prune 第二階段看到新 snapshot 引用會自己撤銷標記。
//! - commit 前重新載入 index：引用到的每個 chunk 都要解析得到，且解析到的 pack 存在、標記沒超過
//!   grace；否則失敗、不寫 snapshot。這把「backup 跑得比 grace 還久」與「prune 在 backup 途中
//!   repack 掉它去重到的 chunk」都從悄悄留下壞 snapshot 變成安全失敗（重跑會重傳）。
//! - commit 前對「帶標記的可達樹」HEAD：樹要存在、touch 要比標記新——這是對
//!   prune 單次 HEAD→DELETE TOCTOU 視窗的防線（平常是空集合，零成本）。

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use kist_backend::{Backend, BackendError};
use kist_chunker::Chunker;
use kist_crypto::RepoKeys;
use kist_format::index::{IndexBlob, IndexPack};
use kist_format::parity;
use kist_format::snapshot::{format_key_timestamp, Root, Snapshot, SnapshotStats};
use kist_format::tree::{
    content_type, meta_kind, node_type, ChunkList, Entry, Tree, MAX_INLINE_CHUNKS,
    MAX_NODES_PER_TREE,
};
use kist_format::{cbor, keys, ChunkId, ObjectId, TreeId};
use tokio::task::JoinSet;

use crate::fsmeta;
use crate::index::ChunkIndex;
use crate::pack::{FinishedPack, PackWriter};
use crate::repo::Repository;
use crate::{blocking, CoreError, Result};

/// 同時在上傳中的 pack 上限（每個最多 pack_target_size bytes 的記憶體）。
/// 1 而不是 2：每個 in-flight pack 都是整份 64 MiB，併發 2 會讓峰值記憶體
/// 多 64 MiB（M5 目標 < 512 MiB）；S3 上傳通常是瓶頸，重疊第二個上傳的
/// 吞吐收益遠小於這 64 MiB 的代價。
const MAX_INFLIGHT_UPLOADS: usize = 1;

/// GC 標記到真正刪除之間的最短時間（與 `prune` 的預設一致）。
pub const DEFAULT_GC_GRACE: std::time::Duration = std::time::Duration::from_secs(72 * 3600);

#[derive(Debug, Clone)]
pub struct BackupOptions {
    pub client_id: [u8; 16],
    pub hostname: String,
    pub username: String,
    /// snapshot 的時間（= backup 開始時間）；`None` = 現在。測試用。
    pub now: Option<time::OffsetDateTime>,
    /// 標記超過這麼久的 pack 視同已刪（commit 前的驗證）。必須與 prune 用的一致。
    pub gc_grace: std::time::Duration,
    /// 每個 pack 旁要存幾片 Reed-Solomon 同位（0..=8；0 = 不存，預設）。
    /// 同位寫失敗只警告、不讓 backup 失敗：資料已安全，缺的只是冗餘。
    pub parity: u8,
    /// 進度回報（給 UI 顯示）；`None` = 不回報。
    pub progress: Option<ProgressCallback>,
}

/// **過程計數**（v3 起不在格式裡——它們依 GC 狀態與去重順序而變，兩個實作
/// 可以「合法地」數出不同數字；不可變結構只收資料事實，見 snapshot.stats）。
/// 屬於 backup 的執行報告，給 CLI / JSON / metrics 用。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
pub struct BackupReport {
    /// 這次新寫進 repo 的 chunk 個數。
    pub chunks_new: u64,
    /// 這次讀了資料的 chunk 個數（含沿用的確認）。
    pub chunks_read: u64,
    pub packs_new: u64,
    /// 引用到被 GC 標記的 pack 而重寫資料的次數回報。
    pub packs_revived: u64,
    pub bytes_stored: u64,
    /// backup 時讀不到而被略過的項目數。snapshot 仍會寫出，CLI 以非 0 結束。
    pub errors: u64,
    /// 走快速路徑沿用 chunk 清單的檔案數。
    pub files_reused: u64,
}

/// backup 進行中的即時狀態（給 UI 顯示進度）。每處理完一個目錄項目呼叫一次 callback；結尾的
/// 驗證與 commit 階段也各呼叫一次（phase 不同）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct BackupProgress {
    /// "files" | "flush" | "verify" | "commit"
    pub phase: &'static str,
    pub stats: SnapshotStats,
    pub report: BackupReport,
    /// 目前處理的路徑（lossy UTF-8）；結尾階段為 None。
    pub current: Option<String>,
}

/// 進度 callback：`Arc` 包起來讓 `BackupOptions` 仍可 `Clone`。
/// 必須 `Send + Sync`，因為 backup 的 future 會被丟到 tokio 的多執行緒 runtime。
#[derive(Clone)]
pub struct ProgressCallback(pub Arc<dyn Fn(&BackupProgress) + Send + Sync>);

impl std::fmt::Debug for ProgressCallback {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ProgressCallback")
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct BackupSummary {
    pub snapshot_key: String,
    pub parent: Option<String>,
    pub roots: Vec<Root>,
    pub stats: SnapshotStats,
    pub report: BackupReport,
}

/// 一個檔案切塊後的結果。
struct FileResult {
    chunks: Vec<ChunkId>,
    bytes_total: u64,
    bytes_new: u64,
    chunks_new: u64,
}

/// 切塊進行到一半的狀態：在 blocking closure 之間移進移出，讓一個大檔可以分多輪處理，
/// 每輪最多封一個 pack 就交回 async 端上傳（記憶體上限才成立）。
struct ChunkState<R: std::io::Read> {
    chunks: kist_chunker::Chunks<R>,
    ids: Vec<ChunkId>,
    bytes_total: u64,
    bytes_new: u64,
    chunks_new: u64,
    reused_count: u64,
    /// 這輪沿用的既有 chunk 與它們所在的 pack。
    reused: Vec<(ChunkId, ObjectId)>,
}

struct Backup {
    repo: Repository,
    chunker: Chunker,
    keys: Arc<RepoKeys>,
    /// 走訪期間會被移進 blocking closure 再移回來，所以用 Option。
    packer: Option<PackWriter>,
    index: Option<ChunkIndex>,
    /// 這次 backup 可達的 tree（新寫或沿用）：commit 驗證的對象。
    seen_trees: HashSet<TreeId>,
    new_packs: Vec<IndexPack>,
    uploads: JoinSet<Result<()>>,
    /// 資料事實（進 snapshot）。
    stats: SnapshotStats,
    /// 過程計數（進報告，不進格式）。
    report: BackupReport,
    /// parent snapshot 的開始時間（Unix 奈秒）；沒有 parent 時快速路徑不會用到。
    parent_start_ns: i64,
    /// backup 開始時已被 GC 標記的 pack：不拿來去重。
    marked: Arc<HashSet<ObjectId>>,
    /// backup 開始時所有的標記（含 tree）：commit 時驗可達樹的 touch 新鮮度。
    marks_at_start: HashMap<ObjectId, time::OffsetDateTime>,
    /// 這次沿用的既有 chunk 各自在哪個 pack（commit 前要驗這些 pack 還在）。
    referenced: HashMap<ObjectId, Vec<ChunkId>>,
    /// 進度回報（見 `BackupOptions::progress`）。
    progress: Option<ProgressCallback>,
    /// 每個 pack 旁存幾片 Reed-Solomon 同位（0 = 不存）。
    parity: u8,
    /// 這次 backup 已看過的硬連結：(dev, inode) → 第一個名字的 chunk 清單與大小。
    /// 後續名字直接沿用，不必重讀資料。
    hardlinks: HashMap<(u64, u64), (u64, Vec<ChunkId>, u8)>,
    /// 已把 bytes 計入 stats 的硬連結群組（(dev,ino)）：`bytes` 對同一份內容
    /// 只算一次（format-v3-draft §9.1），跨 roots 也一樣。
    counted_hardlinks: HashSet<(u64, u64)>,
    /// 切塊緩衝池（每個 2×chunker.max）：整個 backup 重用同一批，不在每個
    /// 檔案各配一次。單執行緒走訪時通常只有一個；池化是為了之後並行切塊。
    chunk_bufs: Vec<Vec<u8>>,
}

/// 呼叫進度 callback（有設才做）。stats/report 是幾個 u64 的 copy，成本可忽略。
fn report_progress(
    progress: Option<&ProgressCallback>,
    phase: &'static str,
    stats: SnapshotStats,
    report: BackupReport,
    current: Option<&Path>,
) {
    if let Some(cb) = progress {
        (cb.0)(&BackupProgress {
            phase,
            stats,
            report,
            current: current.map(|p| p.to_string_lossy().into_owned()),
        });
    }
}

/// parent tree 鏈的串流游標（merge-join 用）。
///
/// 一次只持有一個 segment（≤ `MAX_NODES_PER_TREE` 個節點，約幾 MiB）。
/// 原本是把整個目錄的 parent entries 讀成 `Vec` 再建成 HashMap——
/// 100 萬檔的平面目錄光這兩個結構就要 ~450 MiB。
///
/// 段以 `prev` 串接、段內與段間都以名稱遞增（寫入端的排序合約，
/// format-v3-draft §8），所以 `take_name` 用遞增的查詢名稱做 merge-join：
/// 呼叫端（`process_dir`）本來就依名稱遞增走訪。
struct ParentStream {
    repo: Repository,
    /// 尚未讀取的段 ID（舊→新）。
    parts: std::collections::VecDeque<TreeId>,
    /// 目前段的節點。比 `next_part` 之後進來的任何名稱都小的節點已在這裡被丟掉。
    current: std::iter::Peekable<std::vec::IntoIter<Entry>>,
}

impl ParentStream {
    /// 沿 `prev` 走完鏈取得段 ID（走訪時 entries 直接丟棄，不累積）。
    /// 常見情況（目錄 ≤ 10,000 節點）只有一段。
    async fn open(repo: &Repository, last: &TreeId) -> Result<Self> {
        let mut parts = std::collections::VecDeque::new();
        let mut next = Some(*last);
        let mut seen = HashSet::new();
        while let Some(id) = next {
            if !seen.insert(id) {
                return Err(CoreError::Corrupt {
                    key: keys::tree(&id),
                    reason: "tree chain loops".to_owned(),
                });
            }
            let tree = repo.read_tree(&id).await?;
            next = tree.prev;
            parts.push_front(id);
        }
        Ok(Self {
            repo: repo.clone(),
            parts,
            current: Vec::new().into_iter().peekable(),
        })
    }

    /// 回傳名稱等於 `name` 的 parent 節點（沒有則 `None`）。
    /// 呼叫端必須用遞增的名稱查詢。段讀取失敗時 parent reuse 停擺
    ///（後續都回 `None`，檔案重讀——與原本「讀不到就重建」同一語意）。
    async fn take_name(&mut self, name: &[u8]) -> Option<Entry> {
        loop {
            while matches!(self.current.peek(), Some(e) if e.name.as_slice() < name) {
                self.current.next();
            }
            let hits = matches!(self.current.peek(), Some(e) if e.name.as_slice() == name);
            if hits {
                for e in self.current.by_ref() {
                    if e.name.as_slice() == name {
                        return Some(e);
                    }
                }
            }
            if matches!(self.current.peek(), Some(e) if e.name.as_slice() > name) {
                return None; // 目標不在鏈裡；游標留在原地給下一個（更大的）名稱
            }
            let id = self.parts.pop_front()?;
            match self.repo.read_tree(&id).await {
                Ok(tree) => self.current = tree.entries.into_iter().peekable(),
                Err(e) => {
                    tracing::warn!("cannot read parent tree {id}: {e}; parent reuse disabled");
                    self.parts.clear();
                    return None;
                }
            }
        }
    }
}

/// 除了 snapshot 之外全部寫完的 backup：`commit` 驗證引用的 pack 後寫 snapshot。
/// 拆成兩步是為了讓競態測試能在中間插入 prune。
pub struct PreparedBackup {
    repo: Repository,
    opts: BackupOptions,
    started: time::OffsetDateTime,
    roots: Vec<Root>,
    parent_key: Option<String>,
    stats: SnapshotStats,
    report: BackupReport,
    /// 引用到的 pack → 其中被引用的 chunk（這次新寫的 pack 也在內）。
    referenced: HashMap<ObjectId, Vec<ChunkId>>,
    /// 這次可達的 tree（新寫或沿用）。
    seen_trees: HashSet<TreeId>,
    marks_at_start: HashMap<ObjectId, time::OffsetDateTime>,
    /// 這次自己寫出的 pack：commit 時不需要再驗（存在與否由 BackupTooLong 保證）。
    own_packs: HashSet<ObjectId>,
}

/// backup 與 prune 的時鐘可能相差幾分鐘：BackupTooLong 提早這麼多觸發。
const GRACE_SAFETY_MARGIN: time::Duration = time::Duration::hours(1);

impl PreparedBackup {
    pub fn stats(&self) -> &SnapshotStats {
        &self.stats
    }

    pub fn report(&self) -> &BackupReport {
        &self.report
    }

    /// 驗證引用到的資料都還在，然後寫 snapshot。
    pub async fn commit(self) -> Result<BackupSummary> {
        self.commit_at(time::OffsetDateTime::now_utc()).await
    }

    /// 同 `commit`，「現在」由呼叫端給（競態測試用注入的時鐘）。
    pub async fn commit_at(self, now: time::OffsetDateTime) -> Result<BackupSummary> {
        // 跑超過 grace 的 backup 一律不 commit：它寫的 tree 可能已經被標記、刪掉、連標記都清了，
        // 下面的檢查看不到。這是「grace 必須長於最長的一次 backup」的可執行版本。
        let elapsed = now - self.started;
        let grace = time::Duration::try_from(self.opts.gc_grace)
            .map_err(|_| CoreError::Usage("gc_grace is too large".to_owned()))?;
        let margin = GRACE_SAFETY_MARGIN.min(grace / 2);
        if elapsed + margin >= grace {
            return Err(CoreError::BackupTooLong {
                elapsed_secs: elapsed.whole_seconds(),
                grace_secs: self.opts.gc_grace.as_secs(),
            });
        }
        let marks = self.repo.list_gc_marks().await?;
        report_progress(
            self.opts.progress.as_ref(),
            "verify",
            self.stats,
            self.report,
            None,
        );
        self.repo
            .verify_referenced_chunks(
                &self.referenced,
                &self.own_packs,
                &marks,
                self.opts.gc_grace,
                now,
            )
            .await?;
        self.repo
            .verify_touched_trees(
                &self.seen_trees,
                &self.marks_at_start,
                &marks,
                self.opts.gc_grace,
                now,
            )
            .await?;
        report_progress(
            self.opts.progress.as_ref(),
            "commit",
            self.stats,
            self.report,
            None,
        );
        let snapshot_key = self
            .repo
            .commit_snapshot(
                &self.opts,
                self.started,
                self.roots.clone(),
                self.parent_key.clone(),
                self.stats,
            )
            .await?;
        Ok(BackupSummary {
            snapshot_key,
            parent: self.parent_key,
            roots: self.roots,
            stats: self.stats,
            report: self.report,
        })
    }
}

impl Repository {
    /// 完整的 backup：`backup_prepare` + `commit`。
    pub async fn backup(&self, paths: &[PathBuf], opts: BackupOptions) -> Result<BackupSummary> {
        self.backup_prepare(paths, opts).await?.commit().await
    }

    /// 目前 `gc/` 底下的標記：被標記的物件 → 標記的修改時間。
    pub(crate) async fn list_gc_marks(&self) -> Result<HashMap<ObjectId, time::OffsetDateTime>> {
        let mut out = HashMap::new();
        for o in self.backend().list(keys::GC_PREFIX).await? {
            match keys::object_id_from_key(&o.key) {
                Ok(id) => {
                    out.insert(id, o.modified);
                }
                Err(e) => tracing::warn!("{}: ignoring odd gc marker: {e}", o.key),
            }
        }
        Ok(out)
    }

    /// commit 前：重新載入 index，這次引用到的**每一個 chunk**（沿用的與新寫的）都必須在目前的
    /// index 裡解析得到，而且解析到的 pack 存在、沒有超過 grace 的 GC 標記。
    ///
    /// 為什麼要逐 chunk 而不是逐 pack：prune 的 repack 只搬「有 snapshot 引用」的 chunk，
    /// 進行中的 backup 去重到的 chunk 在它看來是死的，會被丟掉；舊 pack 雖然還在（孤兒、等 grace），
    /// 但 index 已經不指它，下一輪 GC 就會刪。這裡抓到就安全失敗（不寫 snapshot），重跑會重傳那些 chunk。
    async fn verify_referenced_chunks(
        &self,
        referenced: &HashMap<ObjectId, Vec<ChunkId>>,
        own_packs: &HashSet<ObjectId>,
        marks: &HashMap<ObjectId, time::OffsetDateTime>,
        grace: std::time::Duration,
        now: time::OffsetDateTime,
    ) -> Result<()> {
        if referenced.is_empty() {
            // 沒有沿用任何舊 chunk（例如對空 repo 的第一次 backup）：
            // 沒有要驗證的引用，連 index 都不必載入——這一步在 100 萬
            // chunk 的 repo 上會觸發一次完整 index 讀取與快取重建。
            return Ok(());
        }
        let expired = |pack: &ObjectId| marks.get(pack).is_some_and(|m| *m + grace <= now);
        let fresh = self.load_index().await?;
        let mut pack_ok: HashMap<ObjectId, bool> = HashMap::new();
        for (old_pack, chunks) in referenced {
            if own_packs.contains(old_pack) {
                // 自己剛寫的 pack：index 可能把同一個 chunk 解析到別的（甚至被標記的）pack，那不代表我們的副本有問題
                continue;
            }
            for chunk in chunks {
                let Some(loc) = fresh.get(chunk) else {
                    return Err(CoreError::PackMissing {
                        pack: *old_pack,
                        chunk: Some(*chunk),
                    });
                };
                let ok = match pack_ok.get(&loc.pack) {
                    Some(ok) => *ok,
                    None => {
                        let ok = !expired(&loc.pack)
                            && self.backend().exists(&keys::pack(&loc.pack)).await?;
                        pack_ok.insert(loc.pack, ok);
                        ok
                    }
                };
                if !ok {
                    return Err(CoreError::PackMissing {
                        pack: loc.pack,
                        chunk: Some(*chunk),
                    });
                }
            }
        }
        Ok(())
    }

    /// commit 前對「帶標記的可達樹」做檢查（兩個集合平常都是空的，零成本）。
    /// v3 的樹不再重 put，復活訊號是 `touch/<id>` 的 mtime（走訪時覆寫刷新）：
    /// 1. **現在**有已超過 grace 的標記的可達樹：prune 隨時會刪。樹必須存在、
    ///    touch 必須比標記新（時間整秒：同一秒算新，與 prune 的安全側一致）。
    /// 2. **開始時就已被標記**的可達樹：prune 可能在我們 touch 之後才刪它、
    ///    連標記一起清掉，事後從標記看不出來，所以同樣 HEAD：要存在，而且
    ///    touch 比開始時的標記新——這是 HEAD→DELETE TOCTOU 視窗的防線。
    async fn verify_touched_trees(
        &self,
        seen: &HashSet<TreeId>,
        marks_at_start: &HashMap<ObjectId, time::OffsetDateTime>,
        marks: &HashMap<ObjectId, time::OffsetDateTime>,
        grace: std::time::Duration,
        now: time::OffsetDateTime,
    ) -> Result<()> {
        let seen_of: HashSet<ObjectId> = seen
            .iter()
            .map(|t| ObjectId::from_bytes(*t.as_bytes()))
            .collect();
        for (id, marked_at) in marks.iter() {
            if *marked_at + grace > now || !seen_of.contains(id) {
                continue;
            }
            let tree_id = TreeId::from_bytes(*id.as_bytes());
            self.head_tree_and_touch(&tree_id, marked_at).await?;
        }
        for (id, marked_at) in marks_at_start {
            if !seen_of.contains(id) {
                continue;
            }
            let tree_id = TreeId::from_bytes(*id.as_bytes());
            self.head_tree_and_touch(&tree_id, marked_at).await?;
        }
        Ok(())
    }

    /// 樹要存在，而且 **touch** 的修改時間 ≥ 標記（同一秒算新——prune 的
    /// 比較也是 `>=`，兩邊一致才安全）。任一不成立 → 安全失敗、不 commit
    /// （重跑會沿用已上傳資料）。
    /// 樹自身的 mtime **不**參與比較：v3 的樹是 write-once，mtime 永遠停在
    /// 初寫時刻；復活訊號只有 touch（format-v3-draft §13.2/§13.3）。
    async fn head_tree_and_touch(
        &self,
        tree_id: &TreeId,
        marked_at: &time::OffsetDateTime,
    ) -> Result<()> {
        let marked_id = ObjectId::from_bytes(*tree_id.as_bytes());
        // 樹要存在（touch 在而樹不在 = prune 已刪了它——TOCTOU 視窗命中）。
        if let Err(e) = self.backend().head(&keys::tree(tree_id)).await {
            return Err(match e {
                BackendError::NotFound(_) => CoreError::TreeMarked(marked_id),
                e => e.into(),
            });
        }
        let touch = self
            .backend()
            .head(&keys::touch(tree_id))
            .await
            .map_err(|e| match e {
                BackendError::NotFound(_) => CoreError::TreeMarked(marked_id),
                e => e.into(),
            })?;
        if touch.modified < *marked_at {
            return Err(CoreError::TreeMarked(marked_id));
        }
        Ok(())
    }

    /// 除了 snapshot 以外全部寫完。
    pub async fn backup_prepare(
        &self,
        paths: &[PathBuf],
        opts: BackupOptions,
    ) -> Result<PreparedBackup> {
        // snapshot 的時間 = 開始時間：任何在這之後改動的檔案，下一次都必須重讀
        let started = opts.now.unwrap_or_else(time::OffsetDateTime::now_utc);
        let mut abs_paths = Vec::new();
        for p in paths {
            let abs = std::fs::canonicalize(p).map_err(|e| CoreError::io(p, e))?;
            abs_paths.push(abs);
        }
        // roots 依 path bytes 排序（format-v3-draft §9），不是 PathBuf 的順序
        let mut with_bytes = Vec::new();
        for p in abs_paths {
            with_bytes.push((fsmeta::path_to_bytes(&p)?, p));
        }
        with_bytes.sort();
        with_bytes.dedup();
        // `/a` 與 `/a/b` 同時給：只留 `/a`，否則 b 會備份兩次、restore 時重建同一條路徑
        let mut kept: Vec<(Vec<u8>, PathBuf)> = Vec::new();
        for (bytes, path) in with_bytes {
            if kept.iter().any(|(_, outer)| path.starts_with(outer)) {
                tracing::warn!(
                    "{}: already covered by another source path; skipped",
                    path.display()
                );
                continue;
            }
            kept.push((bytes, path));
        }
        let (path_bytes, abs_paths): (Vec<Vec<u8>>, Vec<PathBuf>) = kept.into_iter().unzip();

        // 標記先於 index（與 Go 版同一個 load-bearing 順序）：backup 的
        // index 合併需要標記集合（未標記 pack 優先，規格 §10）。
        let marks_at_start = self.list_gc_marks().await?;
        let marked: HashSet<ObjectId> = marks_at_start.keys().copied().collect();
        if !marked.is_empty() {
            tracing::info!(
                "{} object(s) are marked for deletion; their packs will not be used for deduplication",
                marked.len()
            );
        }
        let index = self.load_index_for_backup(&marked).await?;

        let parent = self.find_parent(&opts.client_id, &path_bytes).await?;
        // 每個 root 各自開 parent 串流（v3：parent 的同 path root 才是這個 root 的 parent）。
        let parent_roots: Vec<(Vec<u8>, TreeId)> = parent
            .as_ref()
            .map(|(_, snap)| {
                snap.roots
                    .iter()
                    .map(|r| (r.path.to_vec(), r.tree))
                    .collect()
            })
            .unwrap_or_default();
        let parent_start_ns = parent.as_ref().map(|(_, snap)| snap.time_ns).unwrap_or(0);

        let mut b = Backup {
            repo: self.clone(),
            chunker: Chunker::new(self.config().chunker),
            keys: Arc::clone(self.keys()),
            packer: Some(PackWriter::new(
                Arc::clone(self.keys()),
                self.config().pack_target_size,
                self.config().chunker.max,
            )),
            index: Some(index),
            seen_trees: HashSet::new(),
            new_packs: Vec::new(),
            uploads: JoinSet::new(),
            stats: SnapshotStats::default(),
            report: BackupReport::default(),
            parent_start_ns,
            hardlinks: HashMap::new(),
            counted_hardlinks: HashSet::new(),
            marked: Arc::new(marked),
            marks_at_start,
            referenced: HashMap::new(),
            progress: opts.progress.clone(),
            parity: opts.parity,
            chunk_bufs: Vec::new(),
        };

        // v3 roots：每個來源路徑的**內容**走訪成自己的 tree（沒有合成根，
        // 根目錄本身不是 entry——stats 的 dirs 不計 roots，規格 §9.1）。
        // 檔案（或 symlink）當來源：root tree 含單一 entry（名稱 = 路徑的
        // 最後元件）——mount/restore 瀏覽成 `base/<name>`。
        let mut roots = Vec::new();
        for (path, pb) in abs_paths.iter().zip(path_bytes.iter()) {
            let parent_subtree = parent_roots
                .iter()
                .find(|(pp, _)| pp.as_slice() == pb.as_slice())
                .map(|(_, t)| *t);
            let meta = std::fs::symlink_metadata(path).map_err(|e| CoreError::io(path, e))?;
            let tree = if meta.is_dir() {
                b.walk_dir(path, parent_subtree).await?
            } else {
                let name = pb
                    .rsplit(|&b| b == b'/')
                    .next()
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| {
                        CoreError::Usage(format!(
                            "cannot derive a name for source path {}",
                            path.display()
                        ))
                    })?
                    .to_vec();
                let parent_entry = match parent_roots
                    .iter()
                    .find(|(pp, _)| pp.as_slice() == pb.as_slice())
                {
                    Some((_, t)) => match ParentStream::open(self, t).await {
                        Ok(mut s) => s.take_name(&name).await,
                        Err(_) => None,
                    },
                    None => None,
                };
                let Some(entry) = b
                    .process_entry(path, name, &meta, parent_entry.as_ref())
                    .await?
                else {
                    return Err(CoreError::Usage(format!(
                        "{}: source could not be read",
                        path.display()
                    )));
                };
                b.write_tree(Tree::new(vec![entry], None)).await?
            };
            roots.push(Root {
                path: serde_bytes::ByteBuf::from(pb.clone()),
                tree,
            });
        }

        // flush 最後一個 pack。走訪結束，去重用的 overlay（100 萬 chunk ≈
        // 185 MiB）不再需要：上傳收尾後立刻丟掉，讓 index blob 的編碼
        // 階段不跟它疊在同一個峰值。
        b.report_phase("flush", None);
        b.flush_pack().await?;
        b.index = None;
        b.wait_uploads(0).await?;

        // index blob（只包含這次新寫的 pack）
        let mut own_packs = HashSet::new();
        if !b.new_packs.is_empty() {
            let packs = std::mem::take(&mut b.new_packs);
            own_packs.extend(packs.iter().map(|p| p.pack));
            self.write_index(IndexBlob::new(packs)).await?;
        }

        Ok(PreparedBackup {
            repo: self.clone(),
            roots,
            parent_key: parent.as_ref().map(|(k, _)| k.clone()),
            stats: b.stats,
            report: b.report,
            referenced: std::mem::take(&mut b.referenced),
            seen_trees: std::mem::take(&mut b.seen_trees),
            marks_at_start: std::mem::take(&mut b.marks_at_start),
            own_packs,
            opts,
            started,
        })
    }

    /// 同一台 client 最新的 snapshot，且備份的 roots 相同（path 集合與順序），才當 parent。
    async fn find_parent(
        &self,
        client_id: &[u8; 16],
        paths: &[Vec<u8>],
    ) -> Result<Option<(String, Snapshot)>> {
        let prefix = keys::snapshot_prefix(client_id);
        let mut keys: Vec<String> = self
            .backend()
            .list(&prefix)
            .await?
            .into_iter()
            .map(|o| o.key)
            .collect();
        keys.sort();
        let Some(latest) = keys.pop() else {
            return Ok(None);
        };
        let snap = match self.read_snapshot(&latest).await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(
                    "cannot read previous snapshot {latest}: {e}; not using it as parent"
                );
                return Ok(None);
            }
        };
        let same_roots = snap.roots.len() == paths.len()
            && snap
                .roots
                .iter()
                .zip(paths.iter())
                .all(|(r, p)| r.path.as_slice() == p.as_slice());
        Ok(same_roots.then_some((latest, snap)))
    }

    async fn commit_snapshot(
        &self,
        opts: &BackupOptions,
        started: time::OffsetDateTime,
        roots: Vec<Root>,
        parent: Option<String>,
        stats: SnapshotStats,
    ) -> Result<String> {
        // 同一奈秒撞 key 幾乎不可能，但 conditional put 失敗時把時間戳往後推 1 ns 再試。
        for attempt in 0..3i64 {
            let now = started + time::Duration::nanoseconds(attempt);
            let key = keys::snapshot(&opts.client_id, &format_key_timestamp(now)?);
            let snapshot = Snapshot {
                version: Snapshot::VERSION,
                client_id: opts.client_id.to_vec(),
                host: opts.hostname.clone(),
                user: opts.username.clone(),
                time_ns: i64::try_from(now.unix_timestamp_nanos()).map_err(|_| {
                    CoreError::Corrupt {
                        key: key.clone(),
                        reason: "timestamp out of range".to_owned(),
                    }
                })?,
                roots: roots.clone(),
                parent: parent.clone(),
                stats,
            };
            match self.write_snapshot(&key, snapshot).await {
                Ok(()) => return Ok(key),
                Err(CoreError::Backend(BackendError::AlreadyExists(_))) => continue,
                Err(e) => return Err(e),
            }
        }
        Err(CoreError::Backend(BackendError::AlreadyExists(
            "snapshot key collided three times".to_owned(),
        )))
    }
}

impl Backup {
    /// 回報目前進度（有 callback 才做）。
    fn report_phase(&self, phase: &'static str, current: Option<&Path>) {
        report_progress(
            self.progress.as_ref(),
            phase,
            self.stats,
            self.report,
            current,
        );
    }

    /// 處理一個目錄項目，回傳它的 tree 節點；不支援的類型回 `None`（略過並警告）。
    /// 不論結果如何（寫進 tree、略過、記成錯誤），做完都回報一次進度。
    async fn process_entry(
        &mut self,
        path: &Path,
        name: Vec<u8>,
        meta: &std::fs::Metadata,
        parent: Option<&Entry>,
    ) -> Result<Option<Entry>> {
        let node = self.process_entry_inner(path, name, meta, parent).await?;
        self.report_phase("files", Some(path));
        Ok(node)
    }

    async fn process_entry_inner(
        &mut self,
        path: &Path,
        name: Vec<u8>,
        meta: &std::fs::Metadata,
        parent: Option<&Entry>,
    ) -> Result<Option<Entry>> {
        let ft = meta.file_type();
        let fs = fsmeta::capture(meta);
        // v3：本機走訪一律是 posix kind（mode/uid/gid/mtime 必填——uid 0 = root
        // 是真實值；ctime/dev/ino/nlink 有才記）。
        let mut entry = Entry {
            name,
            kind: 0,
            meta_kind: meta_kind::POSIX,
            size: 0,
            target: Vec::new(),
            content: content_type::DIRECT,
            chunks: Vec::new(),
            subtree: TreeId::ZERO,
            mode: Some(fs.mode),
            uid: Some(fs.uid),
            gid: Some(fs.gid),
            mtime_ns: Some(fs.mtime_ns),
            ctime_ns: Some(fs.ctime_ns).filter(|v| *v != 0),
            dev: None,
            inode: None,
            nlink: None,
            xattrs: None,
            etag: None,
            vern: None,
        };
        if ft.is_symlink() {
            let target = match std::fs::read_link(path) {
                Ok(t) => t,
                Err(e) => return Ok(self.skip(path, &e.to_string())),
            };
            self.stats.symlinks += 1;
            entry.kind = node_type::SYMLINK;
            entry.target = fsmeta::path_to_bytes(&target)?;
        } else if ft.is_dir() {
            let parent_subtree = match parent {
                Some(e) if e.kind == node_type::DIR && !e.subtree.is_zero() => Some(e.subtree),
                _ => None,
            };
            let subtree = self.walk_dir(path, parent_subtree).await?;
            self.stats.dirs += 1;
            entry.kind = node_type::DIR;
            entry.subtree = subtree;
        } else if ft.is_file() {
            let Some((size, chunks, content)) =
                self.process_file(path, &fs, meta.len(), parent).await?
            else {
                return Ok(None); // 讀不到，已記錄
            };
            self.stats.files += 1;
            entry.kind = node_type::FILE;
            entry.size = size;
            entry.chunks = chunks;
            entry.content = content;
            // 硬連結：記下識別，restore 才能重建連結而不是第二份複本。
            if fs.nlink > 1 {
                entry.dev = Some(fs.dev);
                entry.inode = Some(fs.inode);
                entry.nlink = Some(fs.nlink);
            }
        } else {
            tracing::warn!("{}: unsupported file type, skipped", path.display());
            return Ok(None);
        }
        entry.xattrs = fsmeta::read_xattrs(path);
        Ok(Some(entry))
    }

    /// 遞迴走訪一個目錄的**內容**，回傳它（最後一段）tree 的名稱。
    /// v3 的根目錄也走這裡（roots[].tree = 根目錄內容的 tree）。
    /// `Send`：讓整個 backup 的 future 能被 `tokio::spawn`（daemon 在別的 task 上跑工作）。
    fn walk_dir<'a>(
        &'a mut self,
        path: &'a Path,
        parent_subtree: Option<TreeId>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<TreeId>> + Send + 'a>> {
        Box::pin(async move {
            let mut parent_stream = match parent_subtree {
                Some(id) => match ParentStream::open(&self.repo, &id).await {
                    Ok(s) => Some(s),
                    Err(e) => {
                        tracing::warn!(
                            "cannot read parent tree {id}: {e}; re-reading this directory"
                        );
                        None
                    }
                },
                None => None,
            };

            // 只留名稱 bytes，完整路徑處理到該項目時才 join：1M 檔的平面
            // 目錄，名稱 bytes ≈ 45 MiB；存 (bytes, OsString, PathBuf) 之類
            // 的重複欄位會多出上百 MiB。
            let mut names: Vec<Vec<u8>> = Vec::new();
            match std::fs::read_dir(path) {
                Ok(rd) => {
                    for entry in rd {
                        let entry = match entry {
                            Ok(e) => e,
                            Err(e) => {
                                self.skip(path, &e.to_string());
                                continue;
                            }
                        };
                        match fsmeta::name_to_bytes(&entry.file_name()) {
                            Ok(name) => names.push(name),
                            Err(e) => {
                                self.skip(&entry.path(), &e.to_string());
                            }
                        }
                    }
                }
                Err(e) => {
                    // 讀不到的目錄：記錄並以空目錄寫出，其他部分照常備份
                    self.skip(path, &e.to_string());
                }
            }
            names.sort();

            let mut children: Vec<Entry> = Vec::new();
            let mut prev = None;
            for name in names {
                let child_name = fsmeta::bytes_to_name(&name)?;
                let child_path = path.join(&child_name);
                let meta = match std::fs::symlink_metadata(&child_path) {
                    Ok(m) => m,
                    Err(e) => {
                        // 走訪途中被刪掉的檔案：略過，不讓整個 backup 失敗
                        tracing::warn!("{}: {e}, skipped", child_path.display());
                        continue;
                    }
                };
                let parent_entry = match parent_stream.as_mut() {
                    Some(s) => s.take_name(&name).await,
                    None => None,
                };
                let entry = self
                    .process_entry(&child_path, name, &meta, parent_entry.as_ref())
                    .await?;
                if let Some(entry) = entry {
                    children.push(entry);
                }
                if children.len() >= MAX_NODES_PER_TREE {
                    let part = std::mem::take(&mut children);
                    prev = Some(self.write_tree(Tree::new(part, prev)).await?);
                }
            }
            self.write_tree(Tree::new(children, prev)).await
        })
    }

    /// tree 是 content-addressed：**新樹**以 put_if_absent 寫入；已存在的
    /// （沿用的）以覆寫式 Put 寫 `touch/<id>` 刷新 mtime——那是 GC 復活
    /// 訊號（format-v3-draft §13.1）。同一次 backup 內同內容的目錄只處理一次。
    /// `replicas=1` 時順手寫 `.r1` 副本（同 bytes、冪等；已存在則略過）。
    async fn write_tree(&mut self, tree: Tree) -> Result<TreeId> {
        let (id, bytes) = self.repo.seal_tree(tree).await?;
        if self.seen_trees.insert(id) {
            let replica_bytes = (self.repo.config().replicas > 0).then(|| bytes.clone());
            let primary = match self
                .repo
                .backend()
                .put_if_absent(&keys::tree(&id), bytes.clone())
                .await
            {
                Ok(()) => true,
                Err(BackendError::AlreadyExists(_)) => {
                    // 已存在：驗證現有 bytes（自我修復）。v2 靠無條件重 put
                    // 順手療癒壞樹；v3 正常路徑零額外寫入，只在驗證失敗時
                    // 覆寫——同名必同內容（名稱 = 明文 keyed hash），所以
                    // 覆寫絕不會蓋掉「不同的合法樹」。
                    if self
                        .repo
                        .read_tree_once(&keys::tree(&id), &id)
                        .await
                        .is_err()
                    {
                        tracing::warn!("tree {id} is corrupt; healing with good bytes");
                        self.repo.backend().put(&keys::tree(&id), bytes).await?;
                    }
                    false
                }
                Err(e) => return Err(e.into()),
            };
            if let Some(replica) = replica_bytes {
                match self
                    .repo
                    .backend()
                    .put_if_absent(&keys::tree_replica(&id), replica)
                    .await
                {
                    Ok(()) | Err(BackendError::AlreadyExists(_)) => {}
                    Err(e) => return Err(e.into()),
                }
            }
            if !primary {
                // 沿用的既有樹：touch（覆寫式 Put，mtime 必須刷新）。
                self.repo.touch_tree(&id).await?;
            }
        }
        Ok(id)
    }

    /// 記錄一個讀不到的項目：警告、計數、不寫進 tree。回傳 `None` 方便呼叫端直接 return。
    fn skip(&mut self, path: &Path, reason: &str) -> Option<Entry> {
        tracing::warn!("{}: {reason}; skipped", path.display());
        self.report.errors += 1;
        None
    }

    /// 處理一個檔案：parent 的快速路徑 → 硬連結的重用 → 讀檔切塊。
    /// 回傳 (size, chunks, content 型態)；`None` 表示讀不到、已記錄略過。
    async fn process_file(
        &mut self,
        path: &Path,
        fs: &fsmeta::FsMeta,
        current_size: u64,
        parent: Option<&Entry>,
    ) -> Result<Option<(u64, Vec<ChunkId>, u8)>> {
        if let Some(reused) = self.try_reuse(fs, current_size, parent).await? {
            self.report.files_reused += 1;
            return Ok(Some(reused));
        }
        // 硬連結：同一個 (dev, inode) 在這次 backup 已經讀過 → 直接沿用 chunk 清單。
        if fs.nlink > 1 {
            if let Some((size, chunks, content)) = self.hardlinks.get(&(fs.dev, fs.inode)).cloned()
            {
                return Ok(Some((size, chunks, content)));
            }
        }

        let file = match File::open(path) {
            Ok(f) => f,
            Err(e) => {
                self.skip(path, &e.to_string());
                return Ok(None);
            }
        };
        // 直接把 File 交給 chunker：fill() 會讀滿自己的 2×max 緩衝，
        // BufReader 只是多一層 1 MiB 的 memcpy 與每檔一次的大配置。
        let Some(result) = self.chunk_reader(file, path.to_path_buf()).await? else {
            return Ok(None);
        };
        // size 用實際讀到的長度，不用讀檔前的 metadata：備份途中被 append 的檔案兩者會不同
        let size = result.bytes_total;
        if fs.nlink > 1 {
            // bytes 對同一份硬連結內容只算一次（§9.1；跨 roots）。
            if self.counted_hardlinks.insert((fs.dev, fs.inode)) {
                self.stats.bytes += result.bytes_total;
            }
        } else {
            self.stats.bytes += result.bytes_total;
        }
        self.report.bytes_stored += result.bytes_new;
        self.report.chunks_new += result.chunks_new;

        let out = if result.chunks.len() <= MAX_INLINE_CHUNKS {
            (size, result.chunks, content_type::DIRECT)
        } else {
            // 大檔：chunk 清單本身當資料存
            let list_bytes = cbor::encode(&ChunkList::new(result.chunks))?;
            let list_result = self
                .chunk_reader(
                    std::io::Cursor::new(list_bytes),
                    PathBuf::from("<chunk list>"),
                )
                .await?
                .ok_or_else(|| CoreError::Join("chunk list read failed".into()))?;
            self.report.chunks_new += list_result.chunks_new;
            (size, list_result.chunks, content_type::INDIRECT)
        };
        if fs.nlink > 1 {
            self.hardlinks.insert((fs.dev, fs.inode), out.clone());
        }
        Ok(Some(out))
    }

    /// parent 快速路徑：metadata 沒變、而且它引用的**資料** chunk 全都在 index 裡才沿用。
    async fn try_reuse(
        &mut self,
        fs: &fsmeta::FsMeta,
        current_size: u64,
        parent: Option<&Entry>,
    ) -> Result<Option<(u64, Vec<ChunkId>, u8)>> {
        let Some(pentry) = parent else {
            return Ok(None);
        };
        if pentry.kind != node_type::FILE {
            return Ok(None);
        }
        let pmeta = fsmeta::meta_of_entry(pentry);
        if !fsmeta::file_unchanged(&pmeta, pentry.size, fs, current_size, self.parent_start_ns) {
            return Ok(None);
        }
        let content = pentry.content;
        let index = self
            .index
            .as_ref()
            .ok_or_else(|| CoreError::Join("index missing".into()))?;
        // Indirect 的 chunks 只是清單；真正要驗的是清單解開後的資料 chunk
        let data_ids = if content == content_type::DIRECT {
            pentry.chunks.clone()
        } else {
            if !pentry.chunks.iter().all(|id| index.contains(id)) {
                return Ok(None);
            }
            match self
                .repo
                .resolve_chunks(&pentry.chunks, content, index)
                .await
            {
                Ok(ids) => ids,
                Err(e) => {
                    tracing::warn!("cannot read previous chunk list: {e}; re-reading file");
                    return Ok(None);
                }
            }
        };
        let mut packs: Vec<(ChunkId, ObjectId)> = Vec::with_capacity(data_ids.len());
        for id in &data_ids {
            match index.get(id) {
                Some(loc) if !self.marked.contains(&loc.pack) => packs.push((*id, loc.pack)),
                _ => return Ok(None),
            }
        }
        if content == content_type::INDIRECT {
            for id in &pentry.chunks {
                match index.get(id) {
                    Some(loc) if !self.marked.contains(&loc.pack) => packs.push((*id, loc.pack)),
                    _ => return Ok(None),
                }
            }
        }
        self.record_referenced(packs);
        // bytes：非硬連結每個名字都算；硬連結同一份內容只算一次（§9.1）。
        let count_bytes = if fs.nlink > 1 {
            self.counted_hardlinks.insert((fs.dev, fs.inode))
        } else {
            true
        };
        if count_bytes {
            self.stats.bytes += pentry.size;
        }
        self.report.chunks_read += data_ids.len() as u64;
        Ok(Some((pentry.size, pentry.chunks.clone(), content)))
    }

    /// 記下沿用的 chunk 在哪個 pack。這次新寫的 pack（佔位或已 flush）另外在最後加。
    fn record_referenced(&mut self, chunks: Vec<(ChunkId, ObjectId)>) {
        for (id, pack) in chunks {
            if pack == crate::index::PENDING_PACK {
                continue;
            }
            self.referenced.entry(pack).or_default().push(id);
        }
    }

    /// 切塊、去重、打包。每輪 blocking 最多封一個 pack 就回到 async 端上傳，
    /// 所以不管檔案多大，在飛的 pack 數都受 `MAX_INFLIGHT_UPLOADS` 限制。
    /// 讀取途中出錯回 `None`（已記錄略過；已寫進 pack 的 chunk 留著無害）。
    async fn chunk_reader<R>(&mut self, reader: R, path: PathBuf) -> Result<Option<FileResult>>
    where
        R: std::io::Read + Send + 'static,
    {
        // 先確認 packer/index 都在（內部不變量破損要在拿到緩衝之前返回），
        // 緩衝進了 state 之後，所有錯誤路徑都要回收它。
        if self.packer.is_none() || self.index.is_none() {
            return Err(CoreError::Join("packer/index missing".into()));
        }
        let buf = self.chunk_bufs.pop().unwrap_or_default();
        let mut state = ChunkState {
            chunks: self.chunker.chunks_with_buf(reader, buf),
            ids: Vec::new(),
            bytes_total: 0,
            bytes_new: 0,
            chunks_new: 0,
            reused_count: 0,
            reused: Vec::new(),
        };
        loop {
            let mut packer = self
                .packer
                .take()
                .ok_or_else(|| CoreError::Join("packer missing".into()))?;
            let mut index = self
                .index
                .take()
                .ok_or_else(|| CoreError::Join("index missing".into()))?;
            let keys = Arc::clone(&self.keys);
            let marked = Arc::clone(&self.marked);

            // 錯誤一律帶著 state 出來：chunker 的 2×max 緩衝是整個 backup
            // 共用的，不能在錯誤路徑上把它連同 state 一起丟掉。
            let (packer, index, state_back, finished, done, read_error, hard_error) =
                blocking(move || {
                    let mut finished = None;
                    let mut done = false;
                    let mut read_error = None;
                    let mut hard_error = None;
                    loop {
                        let chunk = match state.chunks.next() {
                            None => {
                                done = true;
                                break;
                            }
                            Some(Err(e)) => {
                                read_error = Some(e.to_string());
                                done = true;
                                break;
                            }
                            Some(Ok(c)) => c,
                        };
                        let id = keys.chunk_id(&chunk);
                        state.bytes_total += chunk.len() as u64;
                        state.ids.push(id);
                        let mut in_marked_pack = false;
                        if let Some(loc) = index.get(&id) {
                            if marked.contains(&loc.pack) {
                                in_marked_pack = true;
                            } else {
                                state.reused_count += 1;
                                state.reused.push((id, loc.pack));
                                continue;
                            }
                        }
                        match packer.add(id, &chunk) {
                            Ok(entry) => {
                                if in_marked_pack {
                                    index.replace_pending(&entry);
                                } else {
                                    index.add_pending(&entry);
                                }
                                state.bytes_new += chunk.len() as u64;
                                state.chunks_new += 1;
                            }
                            Err(e) => {
                                hard_error = Some(e);
                                done = true;
                                break;
                            }
                        }
                        if packer.is_full() {
                            match packer.finish() {
                                Ok(Some(p)) => {
                                    index.resolve_pending(p.id, p.bytes.len() as u64, &p.entries);
                                    finished = Some(p);
                                }
                                Ok(None) => {}
                                Err(e) => {
                                    hard_error = Some(e);
                                    done = true;
                                    break;
                                }
                            }
                            break;
                        }
                    }
                    Ok((packer, index, state, finished, done, read_error, hard_error))
                })
                .await?;
            self.packer = Some(packer);
            self.index = Some(index);
            state = state_back;
            self.record_referenced(std::mem::take(&mut state.reused));
            if let Some(p) = finished {
                self.handle_finished(vec![p]).await?;
            }
            if let Some(e) = hard_error {
                self.chunk_bufs.push(state.chunks.take_buf());
                return Err(e);
            }
            if let Some(reason) = read_error {
                self.chunk_bufs.push(state.chunks.take_buf());
                self.skip(&path, &reason);
                return Ok(None);
            }
            if done {
                self.chunk_bufs.push(state.chunks.take_buf());
                self.report.chunks_read += state.reused_count;
                return Ok(Some(FileResult {
                    chunks: state.ids,
                    bytes_total: state.bytes_total,
                    bytes_new: state.bytes_new,
                    chunks_new: state.chunks_new,
                }));
            }
        }
    }

    async fn flush_pack(&mut self) -> Result<()> {
        let mut packer = self
            .packer
            .take()
            .ok_or_else(|| CoreError::Join("packer missing".into()))?;
        let finished = blocking(move || {
            let p = packer.finish()?;
            Ok((packer, p))
        })
        .await;
        let (packer, finished) = finished?;
        self.packer = Some(packer);
        if let Some(p) = finished {
            if let Some(index) = self.index.as_mut() {
                index.resolve_pending(p.id, p.bytes.len() as u64, &p.entries);
            }
            self.handle_finished(vec![p]).await?;
        }
        Ok(())
    }

    /// 把封好的 pack 丟去上傳，並記進 index blob 的內容。
    async fn handle_finished(&mut self, packs: Vec<FinishedPack>) -> Result<()> {
        for p in packs {
            self.wait_uploads(MAX_INFLIGHT_UPLOADS - 1).await?;
            self.new_packs.push(IndexPack {
                pack: p.id,
                size: p.bytes.len() as u64,
                entries: p.entries,
            });
            self.report.packs_new += 1;
            let backend: Backend = self.repo.backend().clone();
            let id = p.id;
            let key = keys::pack(&id);
            let parity_m = usize::from(self.parity);
            let bytes = p.bytes;
            self.uploads.spawn(async move {
                // 同位是 sidecar：算不出來或上不去只警告——pack 本身已安全，
                // 缺的只是冗餘（與 Go pack.Writer.writeParity 相同語意）。
                // RS 對整個 pack（可到 64 MiB+）做線性組合是重 CPU，丟 blocking；
                // 閉式把 bytes 帶回來上傳，不為了算同位多抄一份。
                let (bytes, parity_bytes) = if parity_m > 0 {
                    blocking(move || {
                        let pb = match parity::encode(&id, &bytes, parity_m) {
                            Ok(b) => Some(b),
                            Err(e) => {
                                tracing::warn!("pack {id} is stored but its parity is not: {e}");
                                None
                            }
                        };
                        Ok((bytes, pb))
                    })
                    .await?
                } else {
                    (bytes, None)
                };
                backend.put(&key, bytes).await.map_err(CoreError::from)?;
                if let Some(parity_bytes) = parity_bytes {
                    let parity_key = keys::parity(&id);
                    match backend.put_if_absent(&parity_key, parity_bytes).await {
                        Ok(()) | Err(BackendError::AlreadyExists(_)) => {}
                        Err(e) => {
                            tracing::warn!("pack {id} is stored but its parity is not: {e}");
                        }
                    }
                }
                Ok(())
            });
        }
        Ok(())
    }

    /// 等到在飛的上傳數 ≤ `max`。
    async fn wait_uploads(&mut self, max: usize) -> Result<()> {
        while self.uploads.len() > max {
            match self.uploads.join_next().await {
                Some(Ok(r)) => r?,
                Some(Err(e)) => return Err(CoreError::Join(e.to_string())),
                None => break,
            }
        }
        Ok(())
    }
}
