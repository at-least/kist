//! 本地 index 快取：把 repo 裡所有 index blob 的內容合併成一張 [`DiskTable`]，
//! 下次開 repo 只需要讀「新出現的 blob」。
//!
//! 目錄：`<cache_root>/<hex(BLAKE3(cache_id ‖ repo 位置))>/`，內含：
//! - `manifest.cbor`：這張表併入了哪些 index blob、pack 清單。
//! - `index.tbl`：排序的紀錄表。
//!
//! 用 `cache_id`（從 master key 派生）而不是明文 `repo_id`；再加上 repo 位置，
//! 讓「同一個 repo 被複製到第二個地方」不會共用快取（第二份可能少了東西）。
//!
//! 更新規則（open 時）：列出 repo 的 `indexes/`，
//! - 有 blob 不在 manifest 裡 → 讀那些 blob；若它們 `supersedes` 了 manifest 裡的 blob → 全部重建；
//!   否則把新紀錄合併進表。
//! - manifest 裡的 blob 在 repo 已經不存在 → 全部重建。
//! - 表或 manifest 壞掉／缺少 → 全部重建。
//!
//! 寫入一律先寫暫存檔再 rename；表先、manifest 後，中途當機最多讓下一次多讀幾個 blob。
//!
//! 快取只能省時間，不能省正確性：`check` 永遠不用快取。
//! 已知限制：pack 被刪掉但 index blob 還在，快取（和 repo 的 index 一樣）看不出來；
//! 那是 M3 的 `gc/` 標記與「inactive client 回來要重新驗證 pack 存在」規則要處理的。

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use kist_format::index::IndexBlob;
use kist_format::{cbor, ObjectId};
use serde::{Deserialize, Serialize};

use crate::index::{ChunkIndex, ChunkLocation, DiskTable, TableRecord};
use crate::{CoreError, Result};

const MANIFEST_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Manifest {
    version: u32,
    /// 已併入的 index blob（不含被 supersede 的）。
    blobs: Vec<ObjectId>,
    /// pack 名稱 → 大小。
    packs: Vec<(ObjectId, u64)>,
}

#[derive(Debug, Clone)]
pub struct IndexCache {
    dir: PathBuf,
}

impl IndexCache {
    pub fn new(cache_root: &Path, cache_id: &[u8; 16], location: &str) -> Self {
        let mut h = blake3::Hasher::new();
        h.update(cache_id);
        h.update(location.as_bytes());
        let name = hex::encode(&h.finalize().as_bytes()[..16]);
        Self {
            dir: cache_root.join(name),
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn manifest_path(&self) -> PathBuf {
        self.dir.join("manifest.cbor")
    }

    fn table_path(&self) -> PathBuf {
        self.dir.join("index.tbl")
    }

    fn read_manifest(&self) -> Option<Manifest> {
        let bytes = std::fs::read(self.manifest_path()).ok()?;
        let m: Manifest = cbor::decode(&bytes).ok()?;
        (m.version == MANIFEST_VERSION).then_some(m)
    }

    /// 依 repo 目前的 blob 集合更新快取，回傳可用的 index。
    /// `fetch` 讀一個 blob；`live` 是 repo 現在有的 blob id。
    pub async fn load<F, Fut>(&self, live: &[ObjectId], fetch: F) -> Result<ChunkIndex>
    where
        F: Fn(ObjectId) -> Fut,
        Fut: std::future::Future<Output = Result<IndexBlob>>,
    {
        let live_set: HashSet<ObjectId> = live.iter().copied().collect();
        let manifest = self.read_manifest();
        let table = manifest
            .as_ref()
            .and_then(|_| DiskTable::open(&self.table_path()).ok());

        // 決定：增量、重建、或直接用
        match (manifest, table) {
            (Some(m), Some(t)) if m.blobs.iter().all(|b| live_set.contains(b)) => {
                let known = m.blobs.clone();
                let new_ids: Vec<ObjectId> = live
                    .iter()
                    .filter(|id| !known.contains(id))
                    .copied()
                    .collect();
                if new_ids.is_empty() {
                    return Ok(ChunkIndex::with_base(
                        Arc::new(t),
                        m.packs.into_iter().collect(),
                    ));
                }
                let mut new_blobs = Vec::new();
                for id in &new_ids {
                    new_blobs.push((*id, fetch(*id).await?));
                }
                let superseded: HashSet<ObjectId> = new_blobs
                    .iter()
                    .flat_map(|(_, b)| b.supersedes.iter().copied())
                    .collect();
                if known.iter().any(|k| superseded.contains(k)) {
                    // 舊 blob 被取代：增量合併會留下過期的紀錄，重建
                    return self.rebuild(live, fetch).await;
                }
                // 新 blob 的紀錄與舊表做 k-way merge（兩邊都已依 ID 排序）：
                // 同一個 chunk 兩邊都有時**新的贏**——別台 client 因為舊 pack
                // 被 GC 標記而重寫了同一個 chunk，這台才不會一直解析到被標記
                // 的 pack。整張舊表串流讀取，不物化成 Vec（100 萬 chunk ≈ 96 MiB）。
                let mut new_records: Vec<TableRecord> = Vec::new();
                let mut packs = m.packs.clone();
                let mut blobs = known;
                for (id, blob) in new_blobs {
                    if superseded.contains(&id) {
                        continue;
                    }
                    push_blob(&mut new_records, &mut packs, &blob);
                    blobs.push(id);
                }
                new_records.sort_by_key(|r| r.id);
                new_records.dedup_by(|later, earlier| later.id == earlier.id);
                packs.sort();
                packs.dedup();
                // 舊表串流 merge 在 write() 裡進行（t 的所有權移進去）
                self.write(new_records, blobs, packs, Some(t))
            }
            _ => return self.rebuild(live, fetch).await,
        }
    }

    async fn rebuild<F, Fut>(&self, live: &[ObjectId], fetch: F) -> Result<ChunkIndex>
    where
        F: Fn(ObjectId) -> Fut,
        Fut: std::future::Future<Output = Result<IndexBlob>>,
    {
        // 樂觀單遍：絕大多數 rebuild 沒有 supersedes（第一次建表、正常成長），
        // 逐 blob 讀取、合併、**立刻丟棄**。若真的有 blob 帶 supersedes
        //（repack / rebuild-index 之類），退回保守兩遍重讀——那時全部 blob
        // 才會同時在記憶體，因為 supersedes 要收齊才知道哪些紀錄要丟。
        let (mut records, kept, mut packs) = match self.rebuild_pass(live, &fetch, true).await? {
            RebuildOutcome::Done(out) => out,
            RebuildOutcome::NeedsTwoPass => match self.rebuild_pass(live, &fetch, false).await? {
                RebuildOutcome::Done(out) => out,
                // 第二遍已收齊全部 supersedes，不會再需要
                RebuildOutcome::NeedsTwoPass => {
                    return Err(CoreError::Corrupt {
                        key: "index".to_owned(),
                        reason: "index cache rebuild failed twice".to_owned(),
                    })
                }
            },
        };
        records.sort_by_key(|r| r.id);
        records.dedup_by(|later, earlier| later.id == earlier.id);
        packs.sort();
        packs.dedup();
        self.write(records, kept, packs, None)
    }

    /// 樂遍（`optimistic = true`）：blob 讀完即丟；遇到帶 supersedes 的 blob
    /// 回 `NeedsTwoPass`。保守遍：先收齊全部 blob 與 supersedes，再合併未取代者。
    async fn rebuild_pass<F, Fut>(
        &self,
        live: &[ObjectId],
        fetch: &F,
        optimistic: bool,
    ) -> Result<RebuildOutcome>
    where
        F: Fn(ObjectId) -> Fut,
        Fut: std::future::Future<Output = Result<IndexBlob>>,
    {
        let mut blobs: Vec<(ObjectId, IndexBlob)> = Vec::new();
        if !optimistic {
            for id in live {
                blobs.push((*id, fetch(*id).await?));
            }
        }
        let superseded: HashSet<ObjectId> = blobs
            .iter()
            .flat_map(|(_, b)| b.supersedes.iter().copied())
            .collect();
        let mut records = Vec::new();
        let mut packs = Vec::new();
        let mut kept = Vec::new();
        for id in live {
            if optimistic {
                let blob = fetch(*id).await?;
                if !blob.supersedes.is_empty() {
                    // 場上可能有取代關係（這個 blob 取代別人，或被別人取代）：
                    // 單遍分不清，保守重來
                    return Ok(RebuildOutcome::NeedsTwoPass);
                }
                push_blob(&mut records, &mut packs, &blob);
                kept.push(*id);
            } else {
                if superseded.contains(id) {
                    continue;
                }
                if let Some((_, b)) = blobs.iter().find(|(bid, _)| bid == id) {
                    push_blob(&mut records, &mut packs, b);
                    kept.push(*id);
                }
            }
        }
        Ok(RebuildOutcome::Done((records, kept, packs)))
    }

    /// 寫表與 manifest。`new_records` 必須已排序去重；`old` 是要合併進來的
    /// 舊表（增量），同 ID 時新紀錄贏。
    fn write(
        &self,
        new_records: Vec<TableRecord>,
        blobs: Vec<ObjectId>,
        packs: Vec<(ObjectId, u64)>,
        old: Option<DiskTable>,
    ) -> Result<ChunkIndex> {
        std::fs::create_dir_all(&self.dir).map_err(|e| CoreError::io(&self.dir, e))?;
        let records: Box<dyn Iterator<Item = Result<TableRecord>> + '_> = match &old {
            Some(t) => Box::new(MergeOldNew {
                old: t.iter()?.peekable(),
                new: new_records.into_iter().peekable(),
            }),
            None => Box::new(new_records.into_iter().map(Ok)),
        };
        let table = DiskTable::build_sorted(&self.table_path(), records)?;
        let manifest = Manifest {
            version: MANIFEST_VERSION,
            blobs,
            packs: packs.clone(),
        };
        let tmp = self.manifest_path().with_extension("cbor.tmp");
        std::fs::write(&tmp, cbor::encode(&manifest)?).map_err(|e| CoreError::io(&tmp, e))?;
        std::fs::rename(&tmp, self.manifest_path())
            .map_err(|e| CoreError::io(self.manifest_path(), e))?;
        Ok(ChunkIndex::with_base(
            Arc::new(table),
            packs.into_iter().collect(),
        ))
    }
}

/// 舊表（串流）與新紀錄的 k-way merge：兩邊都已依 ID 排序，同 ID **新的贏**。
/// 舊表的讀取錯誤原樣傳播。串流讓合併不把整張舊表物化成 Vec。
struct MergeOldNew<O, N>
where
    O: Iterator<Item = Result<TableRecord>>,
    N: Iterator<Item = TableRecord>,
{
    old: std::iter::Peekable<O>,
    new: std::iter::Peekable<N>,
}

impl<O, N> Iterator for MergeOldNew<O, N>
where
    O: Iterator<Item = Result<TableRecord>>,
    N: Iterator<Item = TableRecord>,
{
    type Item = Result<TableRecord>;

    fn next(&mut self) -> Option<Self::Item> {
        // 舊表的讀取錯誤原樣傳播（包括錯誤落在表尾、new 已耗盡的情況）
        if matches!(self.old.peek(), Some(Err(_))) {
            return self.old.next();
        }
        let o: Option<TableRecord> = match self.old.peek() {
            Some(Ok(r)) => Some(*r),
            _ => None,
        };
        let n: Option<TableRecord> = self.new.peek().copied();
        match (o, n) {
            (None, None) => None,
            (None, Some(_)) => self.new.next().map(Ok),
            (Some(_), None) => self.old.next(),
            (Some(o), Some(n)) => {
                if n.id <= o.id {
                    // 同 ID：新的贏，丟掉舊的
                    if n.id == o.id {
                        self.old.next();
                    }
                    self.new.next().map(Ok)
                } else {
                    self.old.next()
                }
            }
        }
    }
}

enum RebuildOutcome {
    Done((Vec<TableRecord>, Vec<ObjectId>, Vec<(ObjectId, u64)>)),
    NeedsTwoPass,
}

fn push_blob(records: &mut Vec<TableRecord>, packs: &mut Vec<(ObjectId, u64)>, blob: &IndexBlob) {
    for p in &blob.packs {
        packs.push((p.pack, p.size));
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kist_format::ChunkId;

    fn rec(id: u8) -> TableRecord {
        TableRecord {
            id: ChunkId::from_bytes([id; 32]),
            location: ChunkLocation {
                pack: ObjectId::from_bytes([9; 32]),
                offset: 0,
                length: 1,
                raw_len: 1,
            },
        }
    }

    fn new_at(id: u8, pack: u8) -> TableRecord {
        TableRecord {
            id: ChunkId::from_bytes([id; 32]),
            location: ChunkLocation {
                pack: ObjectId::from_bytes([pack; 32]),
                offset: 0,
                length: 1,
                raw_len: 1,
            },
        }
    }

    /// 同 ID 時新紀錄贏、其餘照 ID 序合併——backup 對被 GC 標記的舊 pack
    /// 重寫過的 chunk，不能一直解析到舊位置。
    #[test]
    fn merge_prefers_new_records_on_same_id() {
        let old: Vec<Result<TableRecord>> =
            vec![Ok(rec(1)), Ok(new_at(2, 0xAA)), Ok(rec(4)), Ok(rec(7))];
        let new = vec![new_at(2, 0xBB), rec(3)];
        let merged: Vec<TableRecord> = match (MergeOldNew {
            old: old.into_iter().peekable(),
            new: new.into_iter().peekable(),
        })
        .collect::<Result<Vec<_>>>()
        {
            Ok(v) => v,
            Err(e) => panic!("merge failed: {e}"),
        };
        let ids: Vec<u8> = merged.iter().map(|r| r.id.as_bytes()[0]).collect();
        assert_eq!(ids, vec![1, 2, 3, 4, 7]);
        // id 2 的位置是新的
        assert_eq!(merged[1].location.pack.as_bytes()[0], 0xBB);
    }

    /// 舊表的讀取錯誤要傳播，不能默默吞掉少一段紀錄。
    #[test]
    fn merge_propagates_old_read_error() {
        let err = Err(CoreError::Corrupt {
            key: "old".to_owned(),
            reason: "boom".to_owned(),
        });
        let old: Vec<Result<TableRecord>> = vec![Ok(rec(1)), err, Ok(rec(5))];
        let new: Vec<TableRecord> = vec![rec(2)];
        let merged: Result<Vec<TableRecord>> = MergeOldNew {
            old: old.into_iter().peekable(),
            new: new.into_iter().peekable(),
        }
        .collect();
        assert!(merged.is_err(), "舊表錯誤必須傳播");
    }

    /// 舊表先耗盡、錯誤落在表尾：一樣要傳播（曾有吞掉錯誤的 bug）。
    #[test]
    fn merge_propagates_error_at_old_tail() {
        let err = Err(CoreError::Corrupt {
            key: "old".to_owned(),
            reason: "tail".to_owned(),
        });
        let old: Vec<Result<TableRecord>> = vec![Ok(rec(1)), err];
        let new: Vec<TableRecord> = vec![rec(2), rec(3)];
        let merged: Result<Vec<TableRecord>> = MergeOldNew {
            old: old.into_iter().peekable(),
            new: new.into_iter().peekable(),
        }
        .collect();
        assert!(merged.is_err(), "表尾錯誤必須傳播");
    }

    /// 舊表全部 ID 都比新紀錄小（random 32-byte ID 的常見情況）：
    /// 舊表耗盡後要把剩下的新紀錄交完，不能卡住。
    #[test]
    fn merge_drains_new_after_old_exhausted() {
        let old: Vec<Result<TableRecord>> = vec![Ok(rec(1)), Ok(rec(2))];
        let new: Vec<TableRecord> = vec![rec(200), rec(201)];
        let merged: Vec<TableRecord> = match (MergeOldNew {
            old: old.into_iter().peekable(),
            new: new.into_iter().peekable(),
        })
        .collect::<Result<Vec<_>>>()
        {
            Ok(v) => v,
            Err(e) => panic!("merge failed: {e}"),
        };
        let ids: Vec<u8> = merged.iter().map(|r| r.id.as_bytes()[0]).collect();
        assert_eq!(ids, vec![1, 2, 200, 201]);
    }
}
