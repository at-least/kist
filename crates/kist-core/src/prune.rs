//! prune：無鎖的兩階段 GC。
//!
//! 每次執行都從頭算「什麼是活的」，不信任上一次的結果：
//! - tree：從任一 snapshot 走得到；
//! - pack：在有效的 index 裡，**且**持有任一被引用的 chunk（重複的 chunk 兩邊的 pack 都算活，
//!   不挑「正本」——這樣不管讀取端用哪一版 index 解析都安全）；
//! - index blob：沒被別的 blob `supersedes`。
//!
//! 不活的物件走兩階段：
//! 1. 標記：`gc/<object hex>` 用 conditional put 寫一個小標記（只看 key 與後端記的修改時間，
//!    內容是固定的 magic）。剛寫出不到 grace 的物件不標——那可能是進行中的 backup。
//! 2. 刪除：標記超過 grace、而且每個**活躍** client（`inactive_after` 內有 snapshot）在標記後
//!    都有新的 snapshot，才刪。刪 pack 之前先寫一個不含它的 index blob（supersedes 全部舊 blob），
//!    讀取端永遠不會從 index 指到不存在的 pack。刪之前再 HEAD 一次：物件在標記後又被重寫過
//!    （另一台 client 重 put 同一個 tree）就撤銷標記。
//!
//! 標記的物件若又被引用（backup 開始得比標記早、之後才 commit）→ 撤銷標記（復活）。
//! 物件已不存在的標記 → 清掉。
//!
//! repack：活的 pack 裡活 bytes 比例低於門檻時，把活 chunk（解密驗證後重新封裝）搬到新 pack；
//! 新 index 只指新 pack，舊 pack 變成沒人指的孤兒，下一輪標記、再下一輪刪。
//!
//! 安全性依賴兩個假設（寫在 docs/format.md §11）：grace 長於最長的一次 backup；
//! 同一個 client id 一次只跑一個 backup（CLI 用檔案鎖保證）。
//! 引用不完整（有 snapshot / tree / index 讀不出來）時整個拒絕，不標也不刪。

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
    /// 活 bytes 比例低於這個百分比的 pack 會被 repack。0 = 不 repack。
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
            repack_below_percent: 50,
            dry_run: false,
            now: None,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct PruneReport {
    pub snapshots: u64,
    pub live_trees: u64,
    pub live_packs: u64,
    /// 這次新標記的物件數與 bytes。
    pub marked: u64,
    pub marked_bytes: u64,
    /// 標記被撤銷（又被引用、或在標記後被重寫）的物件數。
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
    /// repack 搬動的活 chunk bytes（壓縮後）。
    pub repacked_bytes: u64,
    pub new_packs: u64,
    /// 刪不掉的物件（Object Lock、權限）；標記保留，下次再試。
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
            Kind::Tree => keys::tree(id),
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

impl Repository {
    pub async fn prune(&self, opts: PruneOptions) -> Result<PruneReport> {
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

        // 2. 可達性（嚴格）
        let reach = self.walk_references(&index, &mut |_| {}).await?;
        if let Some(e) = reach.errors.first() {
            return Err(CoreError::Unsafe(format!(
                "{} reference(s) cannot be resolved ({e}); run `kist check` and repair first",
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

        // 4. 活的 pack 與每個 pack 的活 bytes
        let mut live_packs: HashSet<ObjectId> = HashSet::new();
        let mut pack_bytes: HashMap<ObjectId, (u64, u64)> = HashMap::new(); // (live, total)
        for (id, p) in &indexed {
            let mut live = 0u64;
            let mut total = 0u64;
            for e in &p.entries {
                total = total.saturating_add(e.length);
                if reach.referenced_chunks.contains(&e.id) {
                    live = live.saturating_add(e.length);
                }
            }
            if live > 0 {
                live_packs.insert(*id);
            }
            pack_bytes.insert(*id, (live, total));
        }
        report.live_packs = live_packs.len() as u64;
        let is_live = |kind: Kind, id: &ObjectId| match kind {
            Kind::Pack => live_packs.contains(id),
            Kind::Tree => reach.live_trees.contains(id),
            Kind::Index => effective_blobs.contains(id),
        };

        // 5. 活躍 client：每台最新 snapshot 的開始時間
        let mut latest_by_client: HashMap<Vec<u8>, OffsetDateTime> = HashMap::new();
        for (key, snap) in &reach.snapshots {
            let t = OffsetDateTime::parse(
                &snap.time,
                &time::format_description::well_known::Rfc3339,
            )
            .map_err(|e| CoreError::Unsafe(format!("{key}: bad time {:?}: {e}", snap.time)))?;
            let entry = latest_by_client.entry(snap.client_id.clone()).or_insert(t);
            if t > *entry {
                *entry = t;
            }
        }
        // 刪除條件：每個活躍 client 的最新 snapshot 都比標記晚 ⇔ 標記早於「活躍 client 最新 snapshot 的最小值」
        let min_active_latest: Option<OffsetDateTime> = latest_by_client
            .values()
            .filter(|t| **t >= now - inactive_after)
            .min()
            .copied();

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

        // 7. repack 候選：活的、沒被標記的 pack，活 bytes 比例低於門檻
        let repack: Vec<ObjectId> = if opts.repack_below_percent == 0 {
            Vec::new()
        } else {
            let mut v: Vec<ObjectId> = live_packs
                .iter()
                .filter(|id| !marks.contains_key(id) && packs_listed.contains_key(id))
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

        // 8. 新標記：不活、沒標過、而且已經比 grace 老的物件
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
        to_mark.sort_by_key(|t| (t.kind as u8, t.id));
        report.marked = to_mark.len() as u64;
        report.marked_bytes = to_mark.iter().map(|t| t.info.size).sum();
        if opts.dry_run {
            report.deleted = to_delete.len() as u64;
            report.deleted_bytes = to_delete.iter().map(|(t, _)| t.info.size).sum();
            return Ok(report);
        }

        // 9. repack：先寫新 pack
        let repack_set: HashSet<ObjectId> = repack.iter().copied().collect();
        let kept_live: HashSet<ObjectId> = live_packs.difference(&repack_set).copied().collect();
        let (new_packs, moved_bytes) = self
            .repack_packs(&repack, &indexed, &reach.referenced_chunks, &kept_live)
            .await?;
        report.repacked_bytes = moved_bytes;
        report.new_packs = new_packs.len() as u64;

        // 10. 刪 index 裡的 pack 或 repack 之前：重寫 index（不含要刪與被 repack 的 pack，含新 pack）。
        //     孤兒 pack（本來就不在 index 裡）刪掉不需要重寫。
        let deleted_packs: HashSet<ObjectId> = to_delete
            .iter()
            .filter(|(t, _)| t.kind == Kind::Pack && indexed.contains_key(&t.id))
            .map(|(t, _)| t.id)
            .collect();
        if !deleted_packs.is_empty() || !repack.is_empty() {
            let mut keep: Vec<IndexPack> = indexed
                .values()
                .filter(|p| !deleted_packs.contains(&p.pack) && !repack_set.contains(&p.pack))
                .cloned()
                .collect();
            keep.extend(new_packs);
            keep.sort_by_key(|p| p.pack);
            let mut blob = IndexBlob::new(keep);
            let mut supersedes = all_blob_ids.clone();
            supersedes.sort();
            blob.supersedes = supersedes;
            self.write_index(blob).await?;
        }

        // 11. 刪除（刪之前再看一眼：標記後被重寫過就復活）
        for (target, mark) in to_delete {
            let key = target.kind.key(&target.id);
            match self.backend().head(&key).await {
                Ok(h) if h.modified > mark.modified => {
                    tracing::info!("{key}: rewritten after it was marked; keeping it");
                    report.revived += 1;
                    marks_to_remove.push(target.id);
                    continue;
                }
                Ok(_) => {}
                Err(BackendError::NotFound(_)) => {
                    report.stale_marks += 1;
                    marks_to_remove.push(target.id);
                    continue;
                }
                Err(e) => return Err(e.into()),
            }
            match self.backend().delete(&key).await {
                Ok(()) => {
                    report.deleted += 1;
                    report.deleted_bytes += target.info.size;
                    marks_to_remove.push(target.id);
                }
                Err(e) => {
                    tracing::warn!("{key}: cannot delete: {e}; keeping its marker");
                    report.skipped.push(format!("{key}: {e}"));
                }
            }
        }
        for id in marks_to_remove {
            match self.backend().delete(&keys::gc(&id)).await {
                Ok(()) | Err(BackendError::NotFound(_)) => {}
                Err(e) => return Err(e.into()),
            }
        }

        // 12. 新標記（conditional put：已有標記就不動它的時間）
        for t in &to_mark {
            match self
                .backend()
                .put_if_absent(&keys::gc(&t.id), keys::GC_MARK_MAGIC.to_vec())
                .await
            {
                Ok(()) | Err(BackendError::AlreadyExists(_)) => {}
                Err(e) => return Err(e.into()),
            }
        }
        Ok(report)
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

    /// 把 `packs` 裡活的 chunk 搬到新 pack。已在別的活 pack（`kept_live`）有副本的 chunk 不搬。
    /// 回傳新 pack 的 index 項目與搬動的 bytes。
    async fn repack_packs(
        &self,
        packs: &[ObjectId],
        indexed: &HashMap<ObjectId, IndexPack>,
        referenced: &HashSet<ChunkId>,
        kept_live: &HashSet<ObjectId>,
    ) -> Result<(Vec<IndexPack>, u64)> {
        if packs.is_empty() {
            return Ok((Vec::new(), 0));
        }
        let mut kept_chunks: HashSet<ChunkId> = HashSet::new();
        for id in kept_live {
            if let Some(p) = indexed.get(id) {
                kept_chunks.extend(p.entries.iter().map(|e| e.id));
            }
        }
        let keys_ = Arc::clone(self.keys());
        let mut writer = Some(PackWriter::new(
            Arc::clone(self.keys()),
            self.config().pack_target_size,
        ));
        let mut copied: HashSet<ChunkId> = HashSet::new();
        let mut out = Vec::new();
        let mut moved = 0u64;
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
            copied.extend(entries.iter().map(|e| e.id));
            let key = keys::pack(pack);
            let bytes = self.backend().get(&key).await?;
            let keys2 = Arc::clone(&keys_);
            let mut w = writer
                .take()
                .ok_or_else(|| CoreError::Join("pack writer missing".into()))?;
            let (w, finished, n) = blocking(move || {
                let mut finished = Vec::new();
                let mut n = 0u64;
                for e in &entries {
                    let start = usize::try_from(e.offset).unwrap_or(usize::MAX);
                    let end = start.saturating_add(usize::try_from(e.length).unwrap_or(usize::MAX));
                    let slice = bytes.get(start..end).ok_or_else(|| CoreError::Corrupt {
                        key: key.clone(),
                        reason: format!("chunk {} points outside the pack", e.id),
                    })?;
                    let plain = decode_chunk(&keys2, &e.id, slice, e.flags, e.raw_len)?;
                    let added = w.add(e.id, &plain)?;
                    n += added.length;
                    if w.is_full() {
                        if let Some(f) = w.finish()? {
                            finished.push(f);
                        }
                    }
                }
                Ok((w, finished, n))
            })
            .await?;
            writer = Some(w);
            moved += n;
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
        Ok((out, moved))
    }
}

fn to_time_duration(d: std::time::Duration, what: &str) -> Result<time::Duration> {
    time::Duration::try_from(d).map_err(|_| CoreError::Usage(format!("{what} is too large")))
}
