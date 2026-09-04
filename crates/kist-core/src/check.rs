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

use kist_format::tree::{Content, NodeKind};
use kist_format::{keys, ChunkId, ObjectId};

use crate::index::{ChunkIndex, ChunkLocation};
use crate::pack::{decode_chunk, read_trailer};
use crate::repo::Repository;
use crate::{blocking, Result};

#[derive(Debug, Clone, Copy, Default)]
pub struct CheckOptions {
    pub read_data: bool,
}

#[derive(Debug, Clone, Default)]
pub struct CheckReport {
    pub snapshots: u64,
    pub trees: u64,
    pub packs: u64,
    pub chunks: u64,
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
}

impl CheckReport {
    pub fn is_ok(&self) -> bool {
        self.errors.is_empty()
    }
}

impl Repository {
    pub async fn check(&self, opts: CheckOptions) -> Result<CheckReport> {
        let mut report = CheckReport::default();

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

        // 3. snapshots → trees → chunks
        let mut visited_trees = HashSet::new();
        for key in self.list_snapshot_keys().await? {
            report.snapshots += 1;
            let snapshot = match self.read_snapshot(&key).await {
                Ok(s) => s,
                Err(e) => {
                    report.errors.push(format!("{key}: {e}"));
                    continue;
                }
            };
            self.check_tree(
                &snapshot.root,
                &key,
                &index,
                &mut visited_trees,
                &mut report,
            )
            .await;
        }
        report.trees = visited_trees.len() as u64;

        // 4. 讀資料
        if opts.read_data {
            // index 沒有反向表：先把「每個 pack 在 index 裡有哪些 chunk」整理出來
            let mut by_pack: HashMap<ObjectId, HashMap<ChunkId, ChunkLocation>> = HashMap::new();
            let mut all_ids = HashSet::new();
            for (id, loc) in index.chunks() {
                by_pack.entry(loc.pack).or_default().insert(*id, *loc);
                all_ids.insert(*id);
            }
            let all_ids = Arc::new(all_ids);
            for (id, _) in index.packs() {
                let expected = by_pack.remove(id).unwrap_or_default();
                self.check_pack_data(id, expected, Arc::clone(&all_ids), &mut report)
                    .await;
            }
        }
        Ok(report)
    }

    fn check_tree<'a>(
        &'a self,
        id: &'a ObjectId,
        context: &'a str,
        index: &'a ChunkIndex,
        visited: &'a mut HashSet<ObjectId>,
        report: &'a mut CheckReport,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + 'a>> {
        Box::pin(async move {
            if !visited.insert(*id) {
                return;
            }
            let key = keys::tree(id);
            let tree = match self.read_tree(id).await {
                Ok(t) => t,
                Err(e) => {
                    report.errors.push(format!("{key} (from {context}): {e}"));
                    return;
                }
            };
            if let Some(prev) = tree.prev {
                self.check_tree(&prev, context, index, visited, report)
                    .await;
            }
            for node in &tree.nodes {
                match &node.kind {
                    NodeKind::Dir { subtree } => {
                        self.check_tree(subtree, context, index, visited, report)
                            .await;
                    }
                    NodeKind::File { size, content } => {
                        let ids = match content {
                            Content::Direct { chunks } => chunks.clone(),
                            Content::Indirect { chunks } => {
                                if let Some(missing) = chunks.iter().find(|c| !index.contains(c)) {
                                    report.errors.push(format!(
                                        "{key}: chunk list chunk {missing} is missing from the index"
                                    ));
                                    continue;
                                }
                                match self.resolve_content(content, index).await {
                                    Ok(ids) => ids,
                                    Err(e) => {
                                        report.errors.push(format!("{key}: chunk list: {e}"));
                                        continue;
                                    }
                                }
                            }
                        };
                        let mut sum = 0u64;
                        let mut complete = true;
                        for c in ids {
                            match index.get(&c) {
                                Some(loc) => sum = sum.saturating_add(loc.raw_len),
                                None => {
                                    complete = false;
                                    report.errors.push(format!(
                                        "{key}: chunk {c} is missing from the index"
                                    ));
                                }
                            }
                        }
                        // 不讀資料也能抓到 size 與 chunk 總長不符（例如備份中變動的檔）
                        if complete && sum != *size {
                            report.errors.push(format!(
                                "{key}: file {:?} says {size} bytes but its chunks total {sum}",
                                String::from_utf8_lossy(&node.name)
                            ));
                        }
                    }
                    NodeKind::Symlink { .. } => {}
                }
            }
        })
    }

    /// 下載整個 pack：名稱 = hash(bytes)、trailer 解得開、trailer 與 index 一致、每個 chunk 解得開且 ID 相符。
    async fn check_pack_data(
        &self,
        id: &ObjectId,
        expected: HashMap<ChunkId, ChunkLocation>,
        all_ids: Arc<HashSet<ChunkId>>,
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
        let keys = Arc::clone(self.keys());
        let id = *id;
        let key_for_task = key.clone();
        let result: Result<Vec<String>> = blocking(move || {
            let mut errs = Vec::new();
            if ObjectId::of(&bytes) != id {
                errs.push(format!(
                    "{key_for_task}: content hash does not match its name"
                ));
                return Ok(errs);
            }
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
                            || loc.raw_len != entry.raw_len
                            || loc.flags != entry.flags =>
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
                if let Err(e) = decode_chunk(&keys, &entry.id, slice, entry.flags, entry.raw_len) {
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
}
