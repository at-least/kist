//! check：驗證 repo 的一致性。
//!
//! 不讀資料（預設）：
//! - 每個 index blob 讀得出來；
//! - index 裡的每個 pack 都存在、大小正確（HEAD）；
//! - 每個 snapshot 讀得出來，其 tree 全部存在、讀得出來；
//! - tree 引用的每個 chunk 都在 index 裡。
//!
//! `--read-data`：另外把每個 pack 整個下載，驗證名稱 = hash(bytes)、trailer 解得開、
//! trailer 與 index 一致、每個 chunk 解密後 ID 相符。

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use kist_backend::BackendError;
use kist_format::{keys, ChunkId, ObjectId};

use crate::index::ChunkLocation;
use crate::pack::{decode_chunk, read_trailer};
use crate::repo::Repository;
use crate::{blocking, Result};

#[derive(Debug, Clone, Copy, Default)]
pub struct CheckOptions {
    pub read_data: bool,
    /// 用 parity sidecar 就地修復損壞的 pack（隱含 `read_data`：片內的損壞
    /// 只有讀了才看得到）。修復的重寫是 check 唯一覆寫既有物件的動作，
    /// 需要 Put 權限；S3 Object Lock 下的物件修不了，會被回報。
    pub repair: bool,
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct CheckReport {
    pub snapshots: u64,
    pub trees: u64,
    pub packs: u64,
    pub chunks: u64,
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
    /// 已從 parity 修復並重寫的 pack。
    pub repaired: Vec<String>,
    /// 損壞但修不了的 pack（沒有 parity、損壞超過 m 片、或修復結果不對）。
    pub unrepairable: Vec<String>,
}

impl CheckReport {
    pub fn is_ok(&self) -> bool {
        self.errors.is_empty()
    }
}

impl Repository {
    pub async fn check(&self, opts: CheckOptions) -> Result<CheckReport> {
        let mut report = CheckReport::default();
        let opts = if opts.repair {
            CheckOptions {
                read_data: true,
                repair: true,
            }
        } else {
            opts
        };

        // 1. index
        let mut index_errors = Vec::new();
        let index = self.load_index_lenient(&mut index_errors).await?;
        for e in index_errors {
            report.errors.push(format!("index: {e}"));
        }
        report.chunks = index.len() as u64;

        // 2. packs 存在且大小正確
        let listed: HashMap<String, u64> = self
            .backend()
            .list(keys::PACKS_PREFIX)
            .await?
            .into_iter()
            .map(|o| (o.key, o.size))
            .collect();
        let mut indexed_packs = HashSet::new();
        for (id, size) in index.packs() {
            let key = keys::pack(id);
            indexed_packs.insert(key.clone());
            match listed.get(&key) {
                None => report.errors.push(format!("{key}: pack is missing")),
                Some(actual) if actual != size => report.errors.push(format!(
                    "{key}: pack size is {actual} bytes but index says {size}"
                )),
                Some(_) => {}
            }
        }
        report.packs = index.pack_count() as u64;
        for key in listed.keys() {
            if !indexed_packs.contains(key) {
                report
                    .warnings
                    .push(format!("{key}: pack is not referenced by any index"));
            }
        }

        // 3. snapshots → trees → chunks（走訪與 prune 共用；這裡另外驗每個檔案）
        let mut file_errors = Vec::new();
        let reach = self
            .walk_references(&index, &mut |f| {
                let mut sum = 0u64;
                let mut complete = true;
                for c in f.data_chunks {
                    match index.get(c) {
                        Some(loc) => sum = sum.saturating_add(loc.raw_len),
                        None => complete = false, // 走訪已經報了「chunk 不在 index 裡」
                    }
                }
                // 不讀資料也能抓到 size 與 chunk 總長不符（例如備份中變動的檔）
                if complete && sum != f.size {
                    file_errors.push(format!(
                        "{}: file {:?} says {} bytes but its chunks total {sum}",
                        f.tree_key,
                        String::from_utf8_lossy(&f.node.name),
                        f.size
                    ));
                }
            })
            .await?;
        report.snapshots = self.list_snapshot_keys().await?.len() as u64;
        report.errors.extend(reach.errors);
        report.errors.extend(file_errors);
        report.trees = reach.live_trees.len() as u64;

        // 4. 讀資料
        if opts.read_data {
            // index 沒有反向表：先把「每個 pack 在 index 裡有哪些 chunk」整理出來
            let mut by_pack: HashMap<ObjectId, HashMap<ChunkId, ChunkLocation>> = HashMap::new();
            let mut all_ids = HashSet::new();
            for (id, loc) in index.chunks() {
                by_pack.entry(loc.pack).or_default().insert(id, loc);
                all_ids.insert(id);
            }
            let all_ids = Arc::new(all_ids);
            for (id, _) in index.packs() {
                let expected = by_pack.remove(id).unwrap_or_default();
                self.check_pack_data(id, expected, Arc::clone(&all_ids), opts.repair, &mut report)
                    .await;
            }
        }
        Ok(report)
    }

    /// 下載整個 pack：名稱 = hash(bytes)、trailer 解得開、trailer 與 index 一致、每個 chunk 解得開且 ID 相符。
    async fn check_pack_data(
        &self,
        id: &ObjectId,
        expected: HashMap<ChunkId, ChunkLocation>,
        all_ids: Arc<HashSet<ChunkId>>,
        repair: bool,
        report: &mut CheckReport,
    ) {
        let key = keys::pack(id);
        let bytes = match self.backend().get(&key).await {
            Ok(b) => b,
            Err(e) => {
                report.errors.push(format!("{key}: {e}"));
                return;
            }
        };
        let bytes = match ObjectId::of(&bytes) == *id {
            true => bytes,
            false => {
                if !repair {
                    report
                        .errors
                        .push(format!("{key}: content hash does not match its name"));
                    return;
                }
                // 修復的證明是重算 hash 等於 pack 的名字，所以結果永遠不會
                // 「修錯」，只會修不成；修復成功就重抓重驗，失敗的原因與
                // 「沒有 parity」由 repair_pack 回報。
                if !self.repair_pack(id, &bytes, report).await {
                    return;
                }
                report.repaired.push(key.clone());
                // 這個 pack 先前記的錯誤（step 2 的大小不符等）是對損壞內容說的：
                // 修好了就撤掉，report 才不會同時說「修好了」與「有錯」。
                let prefix = format!("{key}:");
                report.errors.retain(|e| !e.starts_with(&prefix));
                // 修復後重抓重驗：驗不過要說出來，不能讓 report 看起來乾淨。
                match self.backend().get(&key).await {
                    Ok(b) if ObjectId::of(&b) == *id => b,
                    _ => {
                        report.errors.push(format!(
                            "{key}: repaired pack does not re-verify; the repair did not stick"
                        ));
                        return;
                    }
                }
            }
        };
        let keys = Arc::clone(self.keys());
        let _id = *id;
        let key_for_task = key.clone();
        let result: Result<Vec<String>> = blocking(move || {
            let mut errs = Vec::new();
            let trailer = match read_trailer(&keys, &bytes) {
                Ok(t) => t,
                Err(e) => {
                    errs.push(format!("{key_for_task}: trailer: {e}"));
                    return Ok(errs);
                }
            };
            let mut seen = HashSet::new();
            for entry in &trailer.entries {
                seen.insert(entry.id);
                match expected.get(&entry.id) {
                    // 同一個 chunk 也可能存在別的 pack 裡（index 只記第一個），那是重複不是錯
                    None if all_ids.contains(&entry.id) => {}
                    None => errs.push(format!(
                        "{key_for_task}: chunk {} is in the pack but not in the index (rebuild-index needed)",
                        entry.id
                    )),
                    Some(loc)
                        if loc.offset != entry.offset
                            || loc.length != entry.length
                            || loc.raw_len != entry.raw_len =>
                    {
                        errs.push(format!(
                            "{key_for_task}: chunk {} location in index differs from the pack trailer",
                            entry.id
                        ));
                    }
                    Some(_) => {}
                }
                let start = usize::try_from(entry.offset).unwrap_or(usize::MAX);
                let end = start.saturating_add(usize::try_from(entry.length).unwrap_or(usize::MAX));
                let Some(slice) = bytes.get(start..end) else {
                    errs.push(format!(
                        "{key_for_task}: chunk {} points outside the pack",
                        entry.id
                    ));
                    continue;
                };
                if let Err(e) = decode_chunk(&keys, &entry.id, slice, entry.raw_len) {
                    errs.push(format!("{key_for_task}: chunk {}: {e}", entry.id));
                }
            }
            for id in expected.keys() {
                if !seen.contains(id) {
                    errs.push(format!(
                        "{key_for_task}: index lists chunk {id} in this pack but the trailer does not"
                    ));
                }
            }
            Ok(errs)
        })
        .await;
        match result {
            Ok(errs) => report.errors.extend(errs),
            Err(e) => report.errors.push(format!("{key}: {e}")),
        }
    }

    /// 從 parity sidecar 重建一個損壞的 pack 並寫回。回傳是否成功；
    /// 失敗的原因（沒有 parity、修不了、或修好了存不回去）各自回報。
    async fn repair_pack(&self, id: &ObjectId, damaged: &[u8], report: &mut CheckReport) -> bool {
        let pack_key = keys::pack(id);
        let parity_key = kist_format::parity::key(id);
        let raw = match self.backend().get(&parity_key).await {
            Ok(r) => r,
            Err(BackendError::NotFound(_)) => {
                report.errors.push(format!(
                    "{pack_key}: content hash does not match its name; no parity to repair it from"
                ));
                report.unrepairable.push(pack_key);
                return false;
            }
            Err(e) => {
                report.errors.push(format!("{pack_key}: parity: {e}"));
                report.unrepairable.push(pack_key);
                return false;
            }
        };
        let obj = match kist_format::parity::parse(&raw) {
            Ok(o) => o,
            Err(e) => {
                report.errors.push(format!("{pack_key}: parity: {e}"));
                report.unrepairable.push(pack_key);
                return false;
            }
        };
        let repaired = {
            let id = *id;
            let damaged = damaged.to_vec();
            blocking(move || Ok(obj.repair(&id, &damaged))).await
        };
        match repaired {
            Ok(Ok(bytes)) => match self.backend().put(&pack_key, bytes).await {
                Ok(()) => true,
                // 修好了但存不回去（例如 S3 Object Lock）：不假裝成功。
                Err(e) => {
                    report
                        .errors
                        .push(format!("{pack_key}: repaired but cannot store it: {e}"));
                    report.unrepairable.push(pack_key);
                    false
                }
            },
            Ok(Err(e)) => {
                report.errors.push(format!("{pack_key}: {e}"));
                report.unrepairable.push(pack_key);
                false
            }
            Err(e) => {
                report.errors.push(format!("{pack_key}: {e}"));
                report.unrepairable.push(pack_key);
                false
            }
        }
    }
}
