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

use crate::index::ChunkIndex;
use crate::pack::{decode_chunk, PackWriter};
use crate::repo::Repository;
use crate::{blocking, CoreError, Result};

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
    referenced: HashSet<ChunkId>,
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
        let mut index = ChunkIndex::new();
        let mut indexed: HashMap<ObjectId, IndexPack> = HashMap::new();
        for (_, blob) in &blobs.effective {
            for p in &blob.packs {
                index.add_pack(p);
                indexed.entry(p.pack).or_insert_with(|| p.clone());
            }
        }
        let all_blob_ids: Vec<ObjectId> = blobs
            .effective
            .iter()
            .map(|(id, _)| *id)
            .chain(blobs.superseded.iter().copied())
            .collect();
        let effective_blobs: HashSet<ObjectId> =
            blobs.effective.iter().map(|(id, _)| *id).collect();

        // 2. 可達性（嚴格：連被引用的 chunk 不在 index 裡都算引用不完整）
        let reach = self.walk_references(&index, &mut |_| {}).await?;
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
        let rank = |id: &ObjectId| (marks.contains_key(id), *id);
        let mut canonical: HashMap<ChunkId, ObjectId> = HashMap::new();
        for (pid, p) in &indexed {
            if phantoms.contains(pid) {
                continue;
            }
            for e in &p.entries {
                if !reach.referenced_chunks.contains(&e.id) {
                    continue;
                }
                match canonical.get(&e.id) {
                    Some(cur) if rank(cur) <= rank(pid) => {}
                    _ => {
                        canonical.insert(e.id, *pid);
                    }
                }
            }
        }
        if let Some(c) = reach
            .referenced_chunks
            .iter()
            .find(|c| !canonical.contains_key(c))
        {
            return Err(CoreError::Unsafe(format!(
                "chunk {c} is referenced but no existing pack holds it (the index lists a pack that is gone); run `kist check` and `kist rebuild-index` first"
            )));
        }
        let needed_packs: HashSet<ObjectId> = canonical.values().copied().collect();
        // 每個 pack 的 (正本 bytes, 全部 bytes)
        let mut pack_bytes: HashMap<ObjectId, (u64, u64)> = HashMap::new();
        for (pid, p) in &indexed {
            let mut live = 0u64;
            let mut total = 0u64;
            for e in &p.entries {
                total = total.saturating_add(e.length);
                if canonical.get(&e.id) == Some(pid) {
                    live = live.saturating_add(e.length);
                }
            }
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
            referenced: reach.referenced_chunks,
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
                &self.referenced,
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
        if !deleted_packs.is_empty() || !new_packs.is_empty() || !self.phantoms.is_empty() {
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
        Ok(self.report)
    }
}

impl Repository {
    /// 把 `packs` 裡它是正本的 chunk 搬到新 pack。已在別的需要的 pack（`kept`）有副本的 chunk 不搬。
    /// 回傳新 pack 的 index 項目、搬動的 bytes、讀不出來的 pack（跳過）。
    async fn repack_packs(
        &self,
        packs: &[ObjectId],
        indexed: &HashMap<ObjectId, IndexPack>,
        referenced: &HashSet<ChunkId>,
        kept: &HashSet<ObjectId>,
        pack_target_size: u64,
    ) -> Result<(Vec<IndexPack>, u64, Vec<(ObjectId, CoreError)>)> {
        if packs.is_empty() {
            return Ok((Vec::new(), 0, Vec::new()));
        }
        let mut kept_chunks: HashSet<ChunkId> = HashSet::new();
        for id in kept {
            if let Some(p) = indexed.get(id) {
                kept_chunks.extend(p.entries.iter().map(|e| e.id));
            }
        }
        let keys_ = Arc::clone(self.keys());
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
                    referenced.contains(&e.id)
                        && !kept_chunks.contains(&e.id)
                        && !copied.contains(&e.id)
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
                        slice.and_then(|s| decode_chunk(&keys2, &e.id, s, e.raw_len));
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
