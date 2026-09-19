//! prune：無鎖的兩階段 GC。
//!
//! 每次執行都從頭算「什麼是需要的」，不信任上一次的結果：
//! - tree：從任一 snapshot 走得到；
//! - pack：是某個被引用 chunk 的**正本**。同一個 chunk 出現在多個 pack 時，正本是「沒被標記的 pack 裡
//!   名稱最小的那個」（都被標記就取名稱最小的）。其他副本所在的 pack 不因此算活；
//! - index blob：沒被別的 blob `supersedes`。
//!
//! 不需要的物件走兩階段：
//! 1. 標記：`gc/<object hex>` 用 conditional put 寫一個小標記（只看 key 與後端記的修改時間，
//!    內容是固定的 magic）。剛寫出不到 grace 的物件不標——那可能是進行中的 backup。
//! 2. 刪除：標記超過 grace、而且每個**活躍** client（`inactive_after` 內有 snapshot）在標記後
//!    都有新的 snapshot，才刪。刪 index 裡的 pack 之前先寫一個不含它的 index blob（supersedes 全部舊 blob），
//!    讀取端永遠不會從 index 指到不存在的 pack。刪之前再 HEAD 一次：物件在標記後又被重寫過
//!    （另一台 client 重 put 同一個 tree）就撤銷標記。
//!
//! 被標記的物件又變成需要的（backup 開始得比標記早、之後才 commit）→ 撤銷標記（復活）。
//! 標記指到的物件不存在 → 清掉標記。
//!
//! repack：需要的、比 grace 老、正本 bytes 比例低於門檻的 pack，把它是正本的 chunk（解密驗證後重新封裝）
//! 搬到新 pack。**舊 pack 留在 index 裡並打上標記**，跟其他垃圾一樣走兩階段：進行中的 backup 若在
//! prune 走訪之後才 commit、引用到舊 pack 裡「當時沒人引用」的 chunk，下一輪會發現舊 pack 又是某個
//! chunk 的正本而復活它（然後再 repack 一次），不會有東西悄悄消失。
//!
//! 拆成兩段：`prune_plan`（讀、走訪、決定）與 `PrunePlan::execute`（寫、刪）。競態測試用這個縫把
//! backup 的 commit 插進去。
//!
//! 幽靈 pack：兩個 prune 重疊、或 prune 途中跑 rebuild-index 時，兩個新 index blob 的 `supersedes`
//! 都只列自己開始時的 blob，結果兩個都有效、取聯集，被其中一個刪掉的 pack 還留在另一個 blob 裡。
//! 這種「index 有、儲存沒有」的 pack **不參加正本的選擇**（否則它可能贏過真正的持有者，害後者被當垃圾），
//! 下次重寫 index 時丟掉；被引用的 chunk 若只有幽靈持有 → 引用不完整，拒絕。
//!
//! 安全性依賴兩個假設（寫在 docs/format.md §11）：grace 長於最長的一次 backup；
//! 同一個 client id 一次只跑一個 backup（CLI 用檔案鎖保證）。
//! 引用不完整（有 snapshot / tree / index 讀不出來，或被引用的 chunk 不在 index 裡）時整個拒絕，不標也不刪。

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use kist_backend::{BackendError, ObjectInfo};
use kist_format::index::{IndexBlob, IndexPack};
use kist_format::pack::PackEntry;
use kist_format::{keys, ChunkId, ObjectId};
use time::OffsetDateTime;

use crate::index::{ChunkLocation, ChunkLocator, TableRecord};
use crate::pack::{decode_chunk, PackWriter};
use crate::repo::{IndexBlobs, Repository};
use crate::{blocking, CoreError, Result};

/// 有效（未被 supersede）index blob 超過這個數，prune 必須合併重寫為一顆
/// （format-v3-draft §10 的壓縮觸發；v2 的 ADR 005 未做項）。
pub const MAX_EFFECTIVE_BLOBS: usize = 64;

#[derive(Debug, Clone)]
pub struct PruneOptions {
    /// 標記到刪除的最短間隔。必須長於最長的一次 backup。
    pub grace: std::time::Duration,
    /// 超過這麼久沒有新 snapshot 的 client 視為 inactive，不阻擋刪除。
    pub inactive_after: std::time::Duration,
    /// prune 主機與 client 的時鐘容許差。活躍判定比較的是 client 的
    /// 備份時間與後端的標記時間；client 時鐘偏快會讓它看來「在標記後
    /// 有新 snapshot」而實際沒有。預設 1 小時（`docs/format.md` §13.2）。
    pub clock_skew: std::time::Duration,
    /// 正本 bytes 比例低於這個百分比的 pack 會被 repack。0 = 不 repack。
    pub repack_below_percent: u8,
    pub dry_run: bool,
    /// 「現在」；`None` = 系統時間。測試用。
    pub now: Option<OffsetDateTime>,
}

impl Default for PruneOptions {
    fn default() -> Self {
        Self {
            grace: crate::backup::DEFAULT_GC_GRACE,
            inactive_after: std::time::Duration::from_secs(30 * 24 * 3600),
            clock_skew: std::time::Duration::from_secs(3600),
            repack_below_percent: 50,
            dry_run: false,
            now: None,
        }
    }
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct PruneReport {
    pub snapshots: u64,
    pub live_trees: u64,
    /// 是某個被引用 chunk 正本的 pack 數。
    pub live_packs: u64,
    /// 這次新標記的物件數與 bytes（含被 repack 的舊 pack）。
    pub marked: u64,
    pub marked_bytes: u64,
    /// 標記被撤銷（又被需要、或在標記後被重寫）的物件數。
    pub revived: u64,
    /// 物件已不存在的標記數。
    pub stale_marks: u64,
    /// 真的刪掉的物件數與 bytes。
    pub deleted: u64,
    pub deleted_bytes: u64,
    /// 標記已超過 grace 但被活躍 client 擋住（它在標記後還沒有新 snapshot）。
    pub blocked: u64,
    /// 標記還沒超過 grace。
    pub waiting: u64,
    pub repacked_packs: u64,
    /// repack 搬動的 chunk bytes（壓縮後）。
    pub repacked_bytes: u64,
    pub new_packs: u64,
    /// 刪不掉的物件（Object Lock、權限）或 repack 時讀不出來的 pack；標記保留，下次再試。
    pub skipped: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Pack,
    Tree,
    Index,
}

impl Kind {
    fn key(self, id: &ObjectId) -> String {
        match self {
            Kind::Pack => keys::pack(id),
            Kind::Tree => keys::tree(&kist_format::TreeId::from_bytes(*id.as_bytes())),
            Kind::Index => keys::index(id),
        }
    }
}

/// 一個待刪或待標記的物件。
struct Target {
    kind: Kind,
    id: ObjectId,
    info: ObjectInfo,
}

/// prune 專用的 chunk 索引：有效 index 裡的每個 (chunk, holder) 一筆，
/// 依 (chunk, pack) 排序。一份結構取代原本三份 100 萬級的東西——walk 用的
/// `ChunkIndex` overlay HashMap、`canonical` HashMap、`referenced` HashSet
///（ADR 005 §5 的「三份合一」；100 萬 chunk 從 ~330 MiB 降到 ~90 MiB）。
///
/// 排序鍵**不含** phantom/marked：marks 的取得時點是競態語義的一部分，
/// 正本選擇維持在原本的時點（walk 之後、list 過 gc/ 之後）由
/// [`Self::mark_and_canonicalize`] 掃組計算。
struct PruneIndex {
    records: Vec<TableRecord>,
    /// 與 records 平行：這筆 holder 的 chunk 是否被引用
    ///（`mark_and_canonicalize` 之後才有效）。
    referenced: Vec<u8>,
}

impl PruneIndex {
    fn new(mut records: Vec<TableRecord>) -> Self {
        records.sort_unstable_by(|a, b| {
            (a.id, a.location.pack, a.location.offset).cmp(&(
                b.id,
                b.location.pack,
                b.location.offset,
            ))
        });
        // 同一個 pack 可能出現在多個 effective blob 裡（兩個 prune 重疊時
        // index 取聯集）；不去重的話 live bytes 會被重複累加。同一 pack 的
        // 同一 chunk 只該有一筆，保留 offset 最小的。
        records.dedup_by(|later, earlier| {
            later.id == earlier.id && later.location.pack == earlier.location.pack
        });
        let n = records.len();
        Self {
            records,
            referenced: vec![0; n],
        }
    }

    /// 同一 chunk 的所有 holder 在 `records` 裡的區間。
    fn group(&self, id: &ChunkId) -> std::ops::Range<usize> {
        let lo = self.records.partition_point(|r| r.id < *id);
        let hi = lo + self.records[lo..].partition_point(|r| r.id == *id);
        lo..hi
    }

    /// `referenced`（未排序、可能重複）排序去重後與 records 線性合併：
    /// 把每個被引用 chunk 的所有非 phantom holder 打上旗標，並算出
    /// 正本（非 phantom holder 裡 `(marked, pack)` 最小者）的
    /// needed packs 與 live bytes。被引用 chunk 一個非 phantom holder
    /// 都沒有 → 引用不完整，回 `Unsafe`（與原本 canonical 表相同語義）。
    fn mark_and_canonicalize(
        &mut self,
        referenced: Vec<ChunkId>,
        is_marked: &dyn Fn(&ObjectId) -> bool,
        phantoms: &HashSet<ObjectId>,
    ) -> Result<(HashSet<ObjectId>, HashMap<ObjectId, u64>)> {
        let mut ref_ids = referenced;
        ref_ids.sort_unstable();
        ref_ids.dedup();
        let mut needed: HashSet<ObjectId> = HashSet::new();
        let mut live: HashMap<ObjectId, u64> = HashMap::new();
        let mut pos = 0usize;
        for chunk in &ref_ids {
            while pos < self.records.len() && self.records[pos].id < *chunk {
                pos += 1;
            }
            let mut i = pos;
            let mut best: Option<(ObjectId, u64)> = None;
            while i < self.records.len() && self.records[i].id == *chunk {
                let pid = self.records[i].location.pack;
                if !phantoms.contains(&pid) {
                    let rank = (is_marked(&pid), pid);
                    let better = match best {
                        None => true,
                        Some((bp, _)) => rank < (is_marked(&bp), bp),
                    };
                    if better {
                        best = Some((pid, self.records[i].location.length));
                    }
                    self.referenced[i] = 1;
                }
                i += 1;
            }
            match best {
                Some((pack, len)) => {
                    needed.insert(pack);
                    *live.entry(pack).or_insert(0) += len;
                }
                None => {
                    return Err(CoreError::Unsafe(format!(
                        "chunk {chunk} is referenced but no existing pack holds it (the index lists a pack that is gone); run `kist check` and `kist rebuild-index` first"
                    )));
                }
            }
        }
        Ok((needed, live))
    }

    /// 這個 chunk 是否被引用（`mark_and_canonicalize` 之後）。
    fn is_referenced(&self, id: &ChunkId) -> bool {
        self.group(id).any(|i| self.referenced[i] == 1)
    }

    /// 這個 chunk 是否有 holder 在 `packs` 裡（repack 的 kept_chunks 語義：
    /// 任一副本在 kept pack 就不用搬，不限正本）。
    fn held_by(&self, id: &ChunkId, packs: &HashSet<ObjectId>) -> bool {
        self.group(id)
            .any(|i| packs.contains(&self.records[i].location.pack))
    }
}

impl ChunkLocator for PruneIndex {
    fn contains(&self, id: &ChunkId) -> bool {
        let g = self.group(id);
        g.start < g.end
    }
    fn get(&self, id: &ChunkId) -> Option<ChunkLocation> {
        // 組內依 pack 排序，第一筆 = 名稱最小的 holder——與 ChunkIndex
        // 「同名 chunk 取名稱最小 pack」的規則一致（規格 §10）。
        let g = self.group(id);
        if g.start < g.end {
            self.records.get(g.start).map(|r| r.location)
        } else {
            None
        }
    }
}

/// `prune_plan` 的結果：所有決定都做完了，還沒寫任何東西。
pub struct PrunePlan {
    repo: Repository,
    dry_run: bool,
    report: PruneReport,
    /// 有效 index 裡的每個 pack 與它的 entries。
    indexed: HashMap<ObjectId, IndexPack>,
    /// index 裡有、儲存上沒有的 pack（見模組說明）：重寫 index 時丟掉。
    phantoms: HashSet<ObjectId>,
    /// plan 時已被標記的 pack：重寫 index 時排在後面。
    marked_packs: HashSet<ObjectId>,
    /// 目前存在的所有 index blob（有效的與被取代的），新 blob 要 supersede 它們。
    all_blob_ids: Vec<ObjectId>,
    /// 全部 (chunk, holder)，含被引用旗標；repack 的引用/kept 查詢用。
    idx: PruneIndex,
    /// 需要的 pack（是某個被引用 chunk 的正本）。
    needed_packs: HashSet<ObjectId>,
    to_delete: Vec<(Target, ObjectInfo)>,
    marks_to_remove: Vec<ObjectId>,
    repack: Vec<ObjectId>,
    to_mark: Vec<Target>,
    pack_target_size: u64,
}

impl Repository {
    pub async fn prune(&self, opts: PruneOptions) -> Result<PruneReport> {
        self.prune_plan(opts).await?.execute().await
    }

    /// 讀、走訪、決定；不寫。
    pub async fn prune_plan(&self, opts: PruneOptions) -> Result<PrunePlan> {
        let now = opts.now.unwrap_or_else(OffsetDateTime::now_utc);
        let grace = to_time_duration(opts.grace, "grace")?;
        let inactive_after = to_time_duration(opts.inactive_after, "inactive_after")?;
        let mut report = PruneReport::default();

        // 1. index（嚴格：任何一個 blob 壞掉就不做）
        let mut errors = Vec::new();
        let blobs = self.load_index_blobs(&mut errors).await?;
        if let Some(e) = errors.first() {
            return Err(CoreError::Unsafe(format!(
                "{} index object(s) cannot be read ({e}); run `kist rebuild-index` first",
                errors.len()
            )));
        }
        let IndexBlobs {
            effective: blob_list,
            superseded: superseded_ids,
        } = blobs;
        let all_blob_ids: Vec<ObjectId> = blob_list
            .iter()
            .map(|(id, _)| *id)
            .chain(superseded_ids.iter().copied())
            .collect();
        let effective_blobs: HashSet<ObjectId> = blob_list.iter().map(|(id, _)| *id).collect();
        let mut indexed: HashMap<ObjectId, IndexPack> = HashMap::new();
        let mut records: Vec<TableRecord> = Vec::new();
        // 把 entries 從 blob move 出來：blob 解碼結果整份留著會讓 entries 在
        // 記憶體裡多一份（100 萬 chunk ≈ 48 MiB）。同時把 (chunk, holder) 攤平
        // 進 PruneIndex——walk 的 contains/get、正本選擇、repack 的引用查詢
        // 都查這一份，不再各自建 HashMap/HashSet。
        for (_, blob) in blob_list {
            for p in blob.packs {
                for e in &p.entries {
                    records.push(TableRecord {
                        id: e.id,
                        location: ChunkLocation {
                            pack: p.pack,
                            offset: e.offset,
                            length: e.length,
                            raw_len: e.raw_len,
                        },
                    });
                }
                indexed.entry(p.pack).or_insert(p);
            }
        }
        let mut idx = PruneIndex::new(records);

        // 2. 可達性（嚴格：連被引用的 chunk 不在 index 裡都算引用不完整）
        let reach = self.walk_references(&idx, &mut |_| {}).await?;
        if let Some(e) = reach.errors.first() {
            return Err(CoreError::Unsafe(format!(
                "{} reference(s) cannot be resolved ({e}); run `kist check` (and `kist rebuild-index` if packs are missing from the index) first",
                reach.errors.len()
            )));
        }
        report.snapshots = reach.snapshots.len() as u64;
        report.live_trees = reach.live_trees.len() as u64;

        // 3. 列出所有東西
        let packs_listed = self.list_ids(keys::PACKS_PREFIX).await?;
        let trees_listed = self.list_ids(keys::TREES_PREFIX).await?;
        let indexes_listed = self.list_ids(keys::INDEXES_PREFIX).await?;
        let marks = self.list_ids(keys::GC_PREFIX).await?;

        // 4. 每個被引用 chunk 的正本 pack：只在儲存上存在的 pack 裡選，沒被標記者優先，其次名稱小者
        let phantoms: HashSet<ObjectId> = indexed
            .keys()
            .filter(|id| !packs_listed.contains_key(id))
            .copied()
            .collect();
        if !phantoms.is_empty() {
            tracing::warn!(
                "{} pack(s) are in the index but not in storage; the index will be rewritten without them",
                phantoms.len()
            );
        }
        // 掃 PruneIndex 的每個 chunk 組（組均 1–2 筆），同時把被引用 chunk 的 holder 打上旗標。
        let is_marked = |id: &ObjectId| marks.contains_key(id);
        let (needed_packs, live_bytes) =
            idx.mark_and_canonicalize(reach.referenced_chunks, &is_marked, &phantoms)?;
        // 每個 pack 的 (正本 bytes, 全部 bytes)：全部 bytes 從 indexed 累計，
        // 正本 bytes 是 mark_and_canonicalize 算出的正本表。
        let mut pack_bytes: HashMap<ObjectId, (u64, u64)> = HashMap::new();
        for (pid, p) in &indexed {
            let mut total = 0u64;
            for e in &p.entries {
                total = total.saturating_add(e.length);
            }
            let live = live_bytes.get(pid).copied().unwrap_or(0);
            pack_bytes.insert(*pid, (live, total));
        }
        report.live_packs = needed_packs.len() as u64;
        let is_live = |kind: Kind, id: &ObjectId| match kind {
            Kind::Pack => needed_packs.contains(id),
            Kind::Tree => reach.live_trees.contains(id),
            Kind::Index => effective_blobs.contains(id),
        };

        // 5. 活躍 client：每台最新 snapshot 的開始時間
        let mut latest_by_client: HashMap<Vec<u8>, OffsetDateTime> = HashMap::new();
        for (key, snap) in &reach.snapshots {
            let t = OffsetDateTime::from_unix_timestamp_nanos(i128::from(snap.time_ns))
                .map_err(|e| CoreError::Unsafe(format!("{key}: bad time {}: {e}", snap.time_ns)))?;
            let entry = latest_by_client.entry(snap.client_id.clone()).or_insert(t);
            if t > *entry {
                *entry = t;
            }
        }
        // 刪除條件：每個活躍 client 的最新 snapshot 都比標記晚 ⇔ 標記早於「活躍 client 最新 snapshot 的最小值」。
        // snapshot 的時間來自 client 的時鐘、標記來自後端：比較時把 client 時間往前修一個
        // clock_skew，時鐘偏快的 client 才不會虛報「標記後有新 snapshot」。
        let skew = to_time_duration(opts.clock_skew, "clock_skew")?;
        let min_active_latest: Option<OffsetDateTime> = latest_by_client
            .values()
            .filter(|t| **t >= now - inactive_after)
            .map(|t| *t - skew)
            .min();

        // 6. 逐個標記決定：復活 / 過期 / 可刪 / 等待 / 被擋
        let mut to_delete: Vec<(Target, ObjectInfo)> = Vec::new();
        let mut marks_to_remove: Vec<ObjectId> = Vec::new();
        for (id, mark) in &marks {
            let found = [
                (Kind::Pack, packs_listed.get(id)),
                (Kind::Tree, trees_listed.get(id)),
                (Kind::Index, indexes_listed.get(id)),
            ]
            .into_iter()
            .find_map(|(kind, info)| info.map(|i| (kind, i.clone())));
            let Some((kind, info)) = found else {
                report.stale_marks += 1;
                marks_to_remove.push(*id);
                continue;
            };
            if is_live(kind, id) {
                report.revived += 1;
                marks_to_remove.push(*id);
                continue;
            }
            if mark.modified + grace > now {
                report.waiting += 1;
                continue;
            }
            if min_active_latest.is_some_and(|m| mark.modified >= m) {
                report.blocked += 1;
                continue;
            }
            to_delete.push((
                Target {
                    kind,
                    id: *id,
                    info,
                },
                mark.clone(),
            ));
        }
        to_delete.sort_by_key(|(t, _)| (t.kind as u8, t.id));

        // 7. repack 候選：需要的、沒被標記、比 grace 老的 pack（年輕的可能是進行中的 backup 剛寫的），
        //    正本 bytes 比例低於門檻
        let repack: Vec<ObjectId> = if opts.repack_below_percent == 0 {
            Vec::new()
        } else {
            let mut v: Vec<ObjectId> = needed_packs
                .iter()
                .filter(|id| !marks.contains_key(id))
                .filter(|id| {
                    packs_listed
                        .get(id)
                        .is_some_and(|info| info.modified + grace <= now)
                })
                .filter(|id| {
                    let (live, total) = pack_bytes.get(id).copied().unwrap_or((0, 0));
                    total > 0
                        && live.saturating_mul(100)
                            < total.saturating_mul(u64::from(opts.repack_below_percent))
                })
                .copied()
                .collect();
            v.sort();
            v
        };
        report.repacked_packs = repack.len() as u64;

        // 8. 新標記：不需要、沒標過、而且已經比 grace 老的物件；被 repack 的 pack 在 execute 時加進來
        let mut to_mark: Vec<Target> = Vec::new();
        let candidates = packs_listed
            .iter()
            .map(|(id, info)| (Kind::Pack, id, info))
            .chain(trees_listed.iter().map(|(id, info)| (Kind::Tree, id, info)))
            .chain(
                indexes_listed
                    .iter()
                    .map(|(id, info)| (Kind::Index, id, info)),
            );
        for (kind, id, info) in candidates {
            if is_live(kind, id) || marks.contains_key(id) || info.modified + grace > now {
                continue;
            }
            to_mark.push(Target {
                kind,
                id: *id,
                info: info.clone(),
            });
        }
        for id in &repack {
            if let Some(info) = packs_listed.get(id) {
                to_mark.push(Target {
                    kind: Kind::Pack,
                    id: *id,
                    info: info.clone(),
                });
            }
        }
        to_mark.sort_by_key(|t| (t.kind as u8, t.id));
        report.marked = to_mark.len() as u64;
        report.marked_bytes = to_mark.iter().map(|t| t.info.size).sum();
        if opts.dry_run {
            report.deleted = to_delete.len() as u64;
            report.deleted_bytes = to_delete.iter().map(|(t, _)| t.info.size).sum();
        }

        Ok(PrunePlan {
            repo: self.clone(),
            dry_run: opts.dry_run,
            report,
            marked_packs: marks
                .keys()
                .filter(|id| indexed.contains_key(id))
                .copied()
                .collect(),
            phantoms,
            indexed,
            all_blob_ids,
            idx,
            needed_packs,
            to_delete,
            marks_to_remove,
            repack,
            to_mark,
            pack_target_size: self.config().pack_target_size,
        })
    }

    /// 某個 prefix 底下所有物件：id → 資訊。名稱不合法的略過（警告）。
    async fn list_ids(&self, prefix: &str) -> Result<HashMap<ObjectId, ObjectInfo>> {
        let mut out = HashMap::new();
        for o in self.backend().list(prefix).await? {
            match keys::object_id_from_key(&o.key) {
                Ok(id) => {
                    out.insert(id, o);
                }
                Err(e) => tracing::warn!("{}: ignoring: {e}", o.key),
            }
        }
        Ok(out)
    }
}

impl PrunePlan {
    /// 目前為止的報告（dry-run 時就是最終報告）。
    pub fn report(&self) -> &PruneReport {
        &self.report
    }

    /// 寫、刪。順序：新 pack → 新 index → 刪物件 → 刪標記 → 新標記。
    /// 任何一步 crash 之後 repo 都一致（index 永遠先於刪除、標記永遠晚於刪除），下一次 prune 會收拾。
    pub async fn execute(mut self) -> Result<PruneReport> {
        if self.dry_run {
            return Ok(self.report);
        }
        let repo = self.repo.clone();

        // 9. repack：先寫新 pack。讀不出來的 pack 跳過（記進 skipped），不影響其他步驟
        let repack_set: HashSet<ObjectId> = self.repack.iter().copied().collect();
        let kept: HashSet<ObjectId> = self.needed_packs.difference(&repack_set).copied().collect();
        let (new_packs, moved_bytes, failed) = repo
            .repack_packs(
                &self.repack,
                &self.indexed,
                &self.idx,
                &kept,
                self.pack_target_size,
            )
            .await?;
        self.report.repacked_bytes = moved_bytes;
        self.report.new_packs = new_packs.len() as u64;
        for (id, e) in &failed {
            self.report
                .skipped
                .push(format!("{}: repack: {e}", keys::pack(id)));
        }
        let failed_ids: HashSet<ObjectId> = failed.iter().map(|(id, _)| *id).collect();
        self.report.repacked_packs -= failed_ids.len() as u64;
        // repack 失敗的 pack 不標記（它還是正本）
        self.to_mark
            .retain(|t| !(t.kind == Kind::Pack && failed_ids.contains(&t.id)));
        self.report.marked = self.to_mark.len() as u64;
        self.report.marked_bytes = self.to_mark.iter().map(|t| t.info.size).sum();

        // 10. 刪 index 裡的 pack 之前、或有新 pack 時：重寫 index（不含要刪的 pack；被 repack 的舊 pack 留著）。
        //     孤兒 pack（本來就不在 index 裡）刪掉不需要重寫。
        let deleted_packs: HashSet<ObjectId> = self
            .to_delete
            .iter()
            .filter(|(t, _)| t.kind == Kind::Pack && self.indexed.contains_key(&t.id))
            .map(|(t, _)| t.id)
            .collect();
        if !deleted_packs.is_empty()
            || !new_packs.is_empty()
            || !self.phantoms.is_empty()
            // v3（format-v3-draft §10）：有效 blob 超過上限就必須合併——
            // 只增不刪的 repo 每次 backup 多一顆 blob，讀取端的 Get＋合併
            // 成本隨之增長；64 顆以內的增量合併代價可忽略。
            || self.all_blob_ids.len() > MAX_EFFECTIVE_BLOBS
        {
            // 順序有意義：讀取端同一個 chunk 取第一個位置，所以新 pack 在前、被標記的（含被 repack 的舊 pack）
            // 在最後，之後的 backup 才不會把 chunk 解析到被標記的 pack 而白白重傳。幽靈 pack 丟掉。
            let dropped =
                |p: &IndexPack| deleted_packs.contains(&p.pack) || self.phantoms.contains(&p.pack);
            let marked_now =
                |p: &IndexPack| repack_set.contains(&p.pack) || self.marked_packs.contains(&p.pack);
            let mut kept: Vec<IndexPack> = self
                .indexed
                .values()
                .filter(|p| !dropped(p) && !marked_now(p))
                .cloned()
                .collect();
            kept.sort_by_key(|p| p.pack);
            let mut old: Vec<IndexPack> = self
                .indexed
                .values()
                .filter(|p| !dropped(p) && marked_now(p))
                .cloned()
                .collect();
            old.sort_by_key(|p| p.pack);
            let mut keep = new_packs;
            keep.extend(kept);
            keep.extend(old);
            let mut blob = IndexBlob::new(keep);
            let mut supersedes = self.all_blob_ids.clone();
            supersedes.sort();
            blob.supersedes = supersedes;
            repo.write_index(blob).await?;
        }

        // 11. 刪除（刪之前再看一眼：標記後被重寫過就復活。S3 的時間是秒級，同一秒算重寫過——安全那邊）
        //     v3 的樹是 write-once，復活訊號在 `touch/<id>`：touch 不存在或
        //     **嚴格小於**標記才算死（touch ≥ mark＝活，同秒取安全側，
        //     與 backup 端的比較一致——format-v3-draft §13.2）。
        let mut marks_to_remove = std::mem::take(&mut self.marks_to_remove);
        for (target, mark) in std::mem::take(&mut self.to_delete) {
            let key = target.kind.key(&target.id);
            match repo.backend().head(&key).await {
                Ok(h) if h.modified >= mark.modified => {
                    tracing::info!("{key}: rewritten after it was marked; keeping it");
                    self.report.revived += 1;
                    marks_to_remove.push(target.id);
                    continue;
                }
                Ok(_) => {}
                Err(BackendError::NotFound(_)) => {
                    self.report.stale_marks += 1;
                    marks_to_remove.push(target.id);
                    continue;
                }
                Err(e) => return Err(e.into()),
            }
            if target.kind == Kind::Tree {
                // touch 檢查：活的 touch（≥ 標記）＝復活。
                let tree_id = kist_format::TreeId::from_bytes(*target.id.as_bytes());
                let revived_by_touch = match repo.backend().head(&keys::touch(&tree_id)).await {
                    Ok(t) => t.modified >= mark.modified,
                    Err(BackendError::NotFound(_)) => false,
                    Err(e) => return Err(e.into()),
                };
                if revived_by_touch {
                    tracing::info!("{key}: touched after it was marked; keeping it");
                    self.report.revived += 1;
                    marks_to_remove.push(target.id);
                    continue;
                }
            }
            match repo.backend().delete(&key).await {
                Ok(()) => {
                    self.report.deleted += 1;
                    self.report.deleted_bytes += target.info.size;
                    marks_to_remove.push(target.id);
                    // pack 的 parity sidecar 一起走（grace 期間 pack 還在，
                    // sidecar 也留著可修復；這裡是 pack 真的刪掉的時刻）。
                    if target.kind == Kind::Pack {
                        let parity_key = kist_format::parity::key(&target.id);
                        match repo.backend().delete(&parity_key).await {
                            Ok(()) | Err(BackendError::NotFound(_)) => {}
                            Err(e) => return Err(e.into()),
                        }
                    }
                    // 樹的成組生命週期（format-v3-draft §13.2/§13.5）：
                    // `.r1` 副本與 touch 訊號隨主體一起走。
                    if target.kind == Kind::Tree {
                        let tree_id = kist_format::TreeId::from_bytes(*target.id.as_bytes());
                        for group_key in [keys::tree_replica(&tree_id), keys::touch(&tree_id)] {
                            match repo.backend().delete(&group_key).await {
                                Ok(()) | Err(BackendError::NotFound(_)) => {}
                                Err(e) => return Err(e.into()),
                            }
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!("{key}: cannot delete: {e}; keeping its marker");
                    self.report.skipped.push(format!("{key}: {e}"));
                }
            }
        }
        for id in marks_to_remove {
            match repo.backend().delete(&keys::gc(&id)).await {
                Ok(()) | Err(BackendError::NotFound(_)) => {}
                Err(e) => return Err(e.into()),
            }
        }

        // 12. 新標記（conditional put：已有標記就不動它的時間）
        for t in &self.to_mark {
            match repo
                .backend()
                .put_if_absent(&keys::gc(&t.id), keys::GC_MARK_MAGIC.to_vec())
                .await
            {
                Ok(()) | Err(BackendError::AlreadyExists(_)) => {}
                Err(e) => return Err(e.into()),
            }
        }

        // 13. parity sidecar 清掃：pack 已不在的 sidecar 一併刪——本 run 刪掉的
        //     已在刪除時帶走，這裡清的是之前死掉的 run 或手動刪 pack 留下的。
        //     m=8 時 sidecar 是 pack 的一半大，孤兒很燒空間。
        if !self.dry_run {
            let stored: HashSet<String> = repo
                .backend()
                .list(keys::PACKS_PREFIX)
                .await?
                .into_iter()
                .map(|o| o.key)
                .collect();
            let sidecars: Vec<String> = repo
                .backend()
                .list(kist_format::parity::PREFIX)
                .await?
                .into_iter()
                .map(|o| o.key)
                .collect();
            for key in sidecars {
                let orphan = match keys::object_id_from_key(&key) {
                    Ok(id) => !stored.contains(&keys::pack(&id)),
                    Err(_) => true, // 不是 parity 命名：當垃圾清
                };
                if orphan {
                    // 名單是剛才列的：並發 backup 可能「pack 已 Put、parity 尚未
                    // 列進我們的名單但已存在」。刪之前再看一眼 pack 還在不在，
                    // pack 在就不動它的 sidecar。
                    let pack_gone = match keys::object_id_from_key(&key) {
                        Ok(id) => repo.backend().head(&keys::pack(&id)).await.is_err(),
                        Err(_) => true,
                    };
                    if !pack_gone {
                        continue;
                    }
                    match repo.backend().delete(&key).await {
                        Ok(()) | Err(BackendError::NotFound(_)) => {}
                        Err(e) => return Err(e.into()),
                    }
                }
            }
        }

        // 14. 孤兒 touch 清掃（v3 §13.2）：`touch/<hex>` 在而 `trees/<hex>`
        //     不在——之前死掉的 run 留下的，刪。**孤兒 `.r1`（主體不在、無標記）
        //     刻意不清理**：那是「主體意外遺失」的災難訊號，副本存在的目的
        //     就是它；交給 `check` 回報。
        if !self.dry_run {
            let touches: Vec<String> = repo
                .backend()
                .list(keys::TOUCH_PREFIX)
                .await?
                .into_iter()
                .map(|o| o.key)
                .collect();
            for key in touches {
                let Some(hex) = key.strip_prefix(&format!("{}/", keys::TOUCH_PREFIX)) else {
                    continue;
                };
                let Ok(tree_id) = kist_format::TreeId::from_hex(hex) else {
                    // 不是 touch 命名：當垃圾清
                    let _ = repo.backend().delete(&key).await;
                    continue;
                };
                if repo.backend().head(&keys::tree(&tree_id)).await.is_err() {
                    match repo.backend().delete(&key).await {
                        Ok(()) | Err(BackendError::NotFound(_)) => {}
                        Err(e) => return Err(e.into()),
                    }
                }
            }
        }
        Ok(self.report)
    }
}

impl Repository {
    /// 把 `packs` 裡它是正本的 chunk 搬到新 pack。已在別的需要的 pack（`kept`）
    /// 有副本的 chunk 不搬（任一 holder 命中就算，不限正本）。
    /// 回傳新 pack 的 index 項目、搬動的 bytes、讀不出來的 pack（跳過）。
    async fn repack_packs(
        &self,
        packs: &[ObjectId],
        indexed: &HashMap<ObjectId, IndexPack>,
        idx: &PruneIndex,
        kept: &HashSet<ObjectId>,
        pack_target_size: u64,
    ) -> Result<(Vec<IndexPack>, u64, Vec<(ObjectId, CoreError)>)> {
        if packs.is_empty() {
            return Ok((Vec::new(), 0, Vec::new()));
        }
        let keys_ = Arc::clone(self.keys());
        let max_chunk = u64::from(self.config().chunker.max);
        let mut writer = Some(PackWriter::new(
            Arc::clone(self.keys()),
            pack_target_size,
            self.config().chunker.max,
        ));
        let mut copied: HashSet<ChunkId> = HashSet::new();
        let mut out = Vec::new();
        let mut moved = 0u64;
        let mut failed = Vec::new();
        for pack in packs {
            let Some(p) = indexed.get(pack) else {
                continue;
            };
            let entries: Vec<PackEntry> = p
                .entries
                .iter()
                .filter(|e| {
                    idx.is_referenced(&e.id) && !idx.held_by(&e.id, kept) && !copied.contains(&e.id)
                })
                .cloned()
                .collect();
            if entries.is_empty() {
                continue;
            }
            let key = keys::pack(pack);
            let bytes = match self.backend().get(&key).await {
                Ok(b) => b,
                Err(e) => {
                    failed.push((*pack, e.into()));
                    continue;
                }
            };
            let keys2 = Arc::clone(&keys_);
            let w = writer
                .take()
                .ok_or_else(|| CoreError::Join("pack writer missing".into()))?;
            let entries_for_task = entries.clone();
            let result = blocking(move || {
                let mut w = w;
                // 先全部解出來驗證，任何一個壞就整個 pack 跳過（writer 不動）
                let mut plains = Vec::with_capacity(entries_for_task.len());
                for e in &entries_for_task {
                    let start = usize::try_from(e.offset).unwrap_or(usize::MAX);
                    let end = start.saturating_add(usize::try_from(e.length).unwrap_or(usize::MAX));
                    let slice = bytes.get(start..end).ok_or_else(|| CoreError::Corrupt {
                        key: key.clone(),
                        reason: format!("chunk {} points outside the pack", e.id),
                    });
                    let plain =
                        slice.and_then(|s| decode_chunk(&keys2, &e.id, s, e.raw_len, max_chunk));
                    match plain {
                        Ok(p) => plains.push((e.id, p)),
                        Err(err) => return Ok((w, Vec::new(), 0, Some(err))),
                    }
                }
                let mut finished = Vec::new();
                let mut n = 0u64;
                for (id, plain) in plains {
                    let added = w.add(id, &plain)?;
                    n += added.length;
                    if w.is_full() {
                        if let Some(f) = w.finish()? {
                            finished.push(f);
                        }
                    }
                }
                Ok((w, finished, n, None))
            })
            .await?;
            let (w, finished, n, err) = result;
            writer = Some(w);
            if let Some(e) = err {
                tracing::warn!("{}: cannot repack: {e}; skipped", keys::pack(pack));
                failed.push((*pack, e));
                continue;
            }
            moved += n;
            copied.extend(entries.iter().map(|e| e.id));
            for f in finished {
                self.backend()
                    .put(&keys::pack(&f.id), f.bytes.clone())
                    .await?;
                out.push(IndexPack {
                    pack: f.id,
                    size: f.bytes.len() as u64,
                    entries: f.entries,
                });
            }
        }
        let mut w = writer.ok_or_else(|| CoreError::Join("pack writer missing".into()))?;
        let last = blocking(move || w.finish()).await?;
        if let Some(f) = last {
            self.backend()
                .put(&keys::pack(&f.id), f.bytes.clone())
                .await?;
            out.push(IndexPack {
                pack: f.id,
                size: f.bytes.len() as u64,
                entries: f.entries,
            });
        }
        Ok((out, moved, failed))
    }
}

fn to_time_duration(d: std::time::Duration, what: &str) -> Result<time::Duration> {
    time::Duration::try_from(d).map_err(|_| CoreError::Usage(format!("{what} is too large")))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::index::{ChunkLocation, TableRecord};

    fn oid(b: u8) -> ObjectId {
        ObjectId::from_bytes([b; 32])
    }
    fn cid(b: u8) -> ChunkId {
        ChunkId::from_bytes([b; 32])
    }
    fn rec(chunk: u8, pack: u8, len: u64) -> TableRecord {
        TableRecord {
            id: cid(chunk),
            location: ChunkLocation {
                pack: oid(pack),
                offset: 0,
                length: len,
                raw_len: len,
            },
        }
    }

    fn lcg(state: &mut u64) -> u64 {
        *state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        *state >> 33
    }

    /// model：與舊實作相同的語義。canonical = 非 phantom holder 裡
    /// `(marked, pack)` 最小者；不存在 → 該 chunk 進 rejects。
    fn model_canonical(
        holders: &HashMap<u8, Vec<(u8, u64)>>, // chunk → [(pack, length)]
        marks: &HashSet<u8>,
        phantoms: &HashSet<u8>,
        referenced: &[u8],
    ) -> (HashSet<u8>, HashMap<u8, u64>, HashSet<u8>) {
        let rank = |p: u8| (marks.contains(&p), p);
        let mut needed = HashSet::new();
        let mut live = HashMap::new();
        let mut rejects = HashSet::new();
        for c in referenced {
            let mut best: Option<(u8, u64)> = None;
            // 完全沒有 holder 的被引用 chunk 也算 rejects（同舊語義：Unsafe）
            for (p, len) in holders.get(c).map_or(&[][..], |v| v.as_slice()) {
                if phantoms.contains(p) {
                    continue;
                }
                let better = match best {
                    None => true,
                    Some((bp, _)) => rank(*p) < rank(bp),
                };
                if better {
                    best = Some((*p, *len));
                }
            }
            match best {
                Some((p, len)) => {
                    needed.insert(p);
                    *live.entry(p).or_insert(0) += len;
                }
                None => {
                    rejects.insert(*c);
                }
            }
        }
        (needed, live, rejects)
    }

    #[test]
    fn canonical_selection_matches_model() {
        let mut state = 0x5eed_u64;
        for _case in 0..300 {
            let n_packs = 1 + (lcg(&mut state) % 6) as u8;
            let n_chunks = 1 + (lcg(&mut state) % 30) as u8;
            let mut holders: HashMap<u8, Vec<(u8, u64)>> = HashMap::new();
            let mut records = Vec::new();
            for c in 0..n_chunks {
                let mut used = HashSet::new();
                for p in 0..n_packs {
                    if lcg(&mut state).is_multiple_of(3) && used.insert(p) {
                        // 每個 holder 各自的 length（同 chunk 在不同 pack 的
                        // 紀錄不必同長——雖然內容相同）
                        let len = 1 + lcg(&mut state) % 1000;
                        holders.entry(c).or_default().push((p, len));
                        records.push(rec(c, p, len));
                    }
                }
            }
            // 兩個 prune 重疊時同一 blob 聯集裡會有重複的 (chunk, pack)：
            // 隨機複製幾筆，語義必須與不重複時一致
            for _ in 0..(lcg(&mut state) % 4) {
                if !records.is_empty() {
                    let i = (lcg(&mut state) % records.len() as u64) as usize;
                    records.push(records[i]);
                }
            }
            let mut marks = HashSet::new();
            let mut phantoms = HashSet::new();
            for p in 0..n_packs {
                if lcg(&mut state).is_multiple_of(3) {
                    marks.insert(p);
                }
                if lcg(&mut state).is_multiple_of(4) {
                    phantoms.insert(p);
                }
            }
            let referenced: Vec<u8> = (0..n_chunks)
                .filter(|_| lcg(&mut state).is_multiple_of(2))
                .collect();

            let (m_needed, m_live, m_rejects) =
                model_canonical(&holders, &marks, &phantoms, &referenced);
            let to_oid = |p: u8| oid(p);
            let m_needed: HashSet<ObjectId> = m_needed.iter().map(|p| to_oid(*p)).collect();
            let m_live: HashMap<ObjectId, u64> =
                m_live.iter().map(|(p, l)| (to_oid(*p), *l)).collect();

            let mut idx = PruneIndex::new(records.clone());
            let marks_set = marks.clone();
            let is_marked = move |id: &ObjectId| marks_set.contains(&id.as_bytes()[0]);
            let phantoms_oid: HashSet<ObjectId> = phantoms.iter().map(|p| to_oid(*p)).collect();
            let ref_ids: Vec<ChunkId> = referenced.iter().map(|c| cid(*c)).collect();
            let out = idx.mark_and_canonicalize(ref_ids, &is_marked, &phantoms_oid);
            if m_rejects.is_empty() {
                let (needed, live) = out.unwrap();
                assert_eq!(needed, m_needed, "case packs={n_packs} chunks={n_chunks} marks={marks:?} phantoms={phantoms:?} referenced={referenced:?}");
                assert_eq!(live, m_live, "live bytes mismatch");
                for c in 0..n_chunks {
                    let referenced = referenced.contains(&c);
                    assert_eq!(
                        idx.is_referenced(&cid(c)),
                        referenced,
                        "is_referenced chunk {c}"
                    );
                }
            } else {
                assert!(out.is_err(), "phantom-only case must be rejected");
            }
        }
    }

    #[test]
    fn unmarked_beats_marked_and_smallest_pack_wins() {
        let mut idx = PruneIndex::new(vec![rec(1, 9, 100), rec(1, 4, 100), rec(1, 7, 100)]);
        let is_marked = |id: &ObjectId| id.as_bytes()[0] == 4;
        let phantoms = HashSet::new();
        let (needed, live) = idx
            .mark_and_canonicalize(vec![cid(1)], &is_marked, &phantoms)
            .unwrap();
        // 4 被標記：7 與 9 未標記，7 名稱最小 → 正本
        assert_eq!(needed, [oid(7)].into_iter().collect());
        assert_eq!(live, [(oid(7), 100u64)].into_iter().collect());
    }

    #[test]
    fn phantom_only_referenced_chunk_is_rejected() {
        let mut idx = PruneIndex::new(vec![rec(1, 3, 100), rec(2, 3, 50)]);
        let is_marked = |_: &ObjectId| false;
        let phantoms: HashSet<ObjectId> = [3u8].iter().map(|p| oid(*p)).collect();
        let err = idx
            .mark_and_canonicalize(vec![cid(1), cid(2)], &is_marked, &phantoms)
            .unwrap_err();
        assert!(err.to_string().contains("no existing pack holds it"));
    }

    #[test]
    fn referenced_flags_cover_all_holders() {
        // 同一 chunk 三個 holder（其中一個 phantom）：flag 只落在非 phantom，
        // 但 is_referenced 對整個 chunk 都是 true
        let mut idx = PruneIndex::new(vec![rec(1, 2, 10), rec(1, 5, 10), rec(1, 8, 10)]);
        let phantoms: HashSet<ObjectId> = [8u8].iter().map(|p| oid(*p)).collect();
        let is_marked = |_: &ObjectId| false;
        let (needed, _live) = idx
            .mark_and_canonicalize(vec![cid(1)], &is_marked, &phantoms)
            .unwrap();
        assert_eq!(needed, [oid(2)].into_iter().collect());
        assert!(idx.is_referenced(&cid(1)));
        assert!(!idx.is_referenced(&cid(9)));
    }

    #[test]
    fn held_by_matches_kept_union() {
        // 舊語義：kept_chunks = kept pack 的所有 entries 聯集，任一 holder 命中即免搬
        let idx = PruneIndex::new(vec![rec(1, 2, 10), rec(1, 5, 10), rec(2, 5, 10)]);
        let kept: HashSet<ObjectId> = [2u8].iter().map(|p| oid(*p)).collect();
        assert!(idx.held_by(&cid(1), &kept)); // holder 在 kept
        assert!(!idx.held_by(&cid(2), &kept)); // 只有 holder 在非 kept
        assert!(!idx.held_by(&cid(3), &kept)); // 不存在
    }

    #[test]
    fn duplicate_pack_records_counted_once() {
        // 同一 pack 經兩個 effective blob 出現兩次：live bytes 只能算一次
        let mut idx = PruneIndex::new(vec![rec(1, 5, 100), rec(1, 5, 100), rec(2, 5, 40)]);
        let is_marked = |_: &ObjectId| false;
        let (needed, live) = idx
            .mark_and_canonicalize(vec![cid(1), cid(2)], &is_marked, &HashSet::new())
            .unwrap();
        assert_eq!(needed, [oid(5)].into_iter().collect());
        assert_eq!(live, [(oid(5), 140u64)].into_iter().collect());
        assert_eq!(idx.group(&cid(1)).len(), 1);
    }

    #[test]
    fn locator_get_matches_chunk_index_min_pack_rule() {
        use crate::index::{ChunkIndex, ChunkLocator as _};
        // 隨機 holder 順序下，PruneIndex::get 必須與 ChunkIndex 的
        // 「名稱最小 pack 勝」規則一致
        let mut state = 0xbeef_u64;
        for _case in 0..100 {
            let n_packs = 1 + (lcg(&mut state) % 5) as u8;
            let n_chunks = 1 + (lcg(&mut state) % 20) as u8;
            let mut entries: Vec<(u8, u8)> = Vec::new();
            for c in 0..n_chunks {
                for p in 0..n_packs {
                    if lcg(&mut state).is_multiple_of(3) {
                        entries.push((c, p));
                    }
                }
            }
            let mut index = ChunkIndex::new();
            let mut records = Vec::new();
            // 以隨機順序餵 pack（每個 pack 一次 add_pack），打亞 holder 順序
            let mut packs: Vec<u8> = (0..n_packs).collect();
            packs.sort();
            for p in &packs {
                let p = *p;
                let mut pack_entries = Vec::new();
                for c in 0..n_chunks {
                    if entries.contains(&(c, p)) {
                        pack_entries.push(kist_format::pack::PackEntry {
                            id: cid(c),
                            offset: 0,
                            length: 10,
                            raw_len: 10,
                        });
                    }
                }
                if pack_entries.is_empty() {
                    continue;
                }
                index.add_pack(&kist_format::index::IndexPack {
                    pack: oid(p),
                    size: 100,
                    entries: pack_entries,
                });
            }
            // records 依隨機順序塞
            let mut shuffled = entries.clone();
            for i in (1..shuffled.len()).rev() {
                let j = (lcg(&mut state) % (i as u64 + 1)) as usize;
                shuffled.swap(i, j);
            }
            for (c, p) in &shuffled {
                records.push(rec(*c, *p, 10));
            }
            let idx = PruneIndex::new(records);
            for c in 0..n_chunks {
                assert_eq!(
                    idx.get(&cid(c)),
                    index.get(&cid(c)),
                    "get mismatch chunk {c}"
                );
                assert_eq!(idx.contains(&cid(c)), index.contains(&cid(c)));
            }
            assert!(!idx.contains(&cid(200)));
        }
    }
}
