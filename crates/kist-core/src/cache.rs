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
        let (base_records, known_blobs, mut packs): (
            Vec<TableRecord>,
            Vec<ObjectId>,
            Vec<(ObjectId, u64)>,
        ) = match (manifest, table) {
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
                let mut records: Vec<TableRecord> = t.iter()?.collect::<Result<_>>()?;
                let mut packs = m.packs.clone();
                let mut blobs = known;
                for (id, blob) in new_blobs {
                    if superseded.contains(&id) {
                        continue;
                    }
                    push_blob(&mut records, &mut packs, &blob);
                    blobs.push(id);
                }
                (records, blobs, packs)
            }
            _ => return self.rebuild(live, fetch).await,
        };
        packs.sort();
        packs.dedup();
        self.write(base_records, known_blobs, packs)
    }

    async fn rebuild<F, Fut>(&self, live: &[ObjectId], fetch: F) -> Result<ChunkIndex>
    where
        F: Fn(ObjectId) -> Fut,
        Fut: std::future::Future<Output = Result<IndexBlob>>,
    {
        let mut blobs = Vec::new();
        for id in live {
            blobs.push((*id, fetch(*id).await?));
        }
        let superseded: HashSet<ObjectId> = blobs
            .iter()
            .flat_map(|(_, b)| b.supersedes.iter().copied())
            .collect();
        let mut records = Vec::new();
        let mut packs = Vec::new();
        let mut kept = Vec::new();
        for (id, blob) in &blobs {
            if superseded.contains(id) {
                continue;
            }
            push_blob(&mut records, &mut packs, blob);
            kept.push(*id);
        }
        packs.sort();
        packs.dedup();
        self.write(records, kept, packs)
    }

    fn write(
        &self,
        records: Vec<TableRecord>,
        blobs: Vec<ObjectId>,
        packs: Vec<(ObjectId, u64)>,
    ) -> Result<ChunkIndex> {
        std::fs::create_dir_all(&self.dir).map_err(|e| CoreError::io(&self.dir, e))?;
        let table = DiskTable::build(&self.table_path(), records)?;
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
                    flags: e.flags,
                },
            });
        }
    }
}
