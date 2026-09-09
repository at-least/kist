//! `rebuild-index`：從每個 pack 的 trailer 重建 index。
//!
//! index blob 只是快取（見 `docs/format.md` §10）；遺失或損壞時可以從 pack 重建。
//! 每個 pack 只做兩次 range read（檔尾 16 bytes → trailer），不下載整個 pack。
//! 重建出來的新 blob `supersedes` 所有在**開始時**存在的 blob；重建途中別的 client
//! 寫出的新 blob 不在名單裡，會照常保留。舊 blob 不刪（M3 的 GC 才刪）。

use std::sync::Arc;

use kist_format::index::{IndexBlob, IndexPack};
use kist_format::pack::FOOTER_LEN;
use kist_format::{keys, pack, ObjectId};
use tokio::task::JoinSet;

use crate::repo::Repository;
use crate::{blocking, CoreError, Result};

/// 同時讀幾個 pack 的 trailer。
const CONCURRENCY: usize = 8;

#[derive(Debug, Clone, Copy, Default, serde::Serialize)]
pub struct RebuildSummary {
    pub packs: u64,
    pub chunks: u64,
    /// 被新 blob 取代的舊 blob 數。
    pub superseded: u64,
}

impl Repository {
    pub async fn rebuild_index(&self) -> Result<RebuildSummary> {
        let mut existing = Vec::new();
        for o in self.backend().list(keys::INDEXES_PREFIX).await? {
            existing.push(keys::object_id_from_key(&o.key)?);
        }
        existing.sort();

        let mut packs: Vec<(ObjectId, u64)> = Vec::new();
        for o in self.backend().list(keys::PACKS_PREFIX).await? {
            packs.push((keys::object_id_from_key(&o.key)?, o.size));
        }
        packs.sort();

        let mut tasks: JoinSet<Result<IndexPack>> = JoinSet::new();
        let mut done: Vec<IndexPack> = Vec::new();
        let mut queue = packs.into_iter();
        loop {
            while tasks.len() < CONCURRENCY {
                let Some((id, size)) = queue.next() else {
                    break;
                };
                let repo = self.clone();
                tasks.spawn(async move { repo.read_pack_index(id, size).await });
            }
            match tasks.join_next().await {
                Some(Ok(Ok(p))) => done.push(p),
                Some(Ok(Err(e))) => return Err(e),
                Some(Err(e)) => return Err(CoreError::Join(e.to_string())),
                None => break,
            }
        }
        done.sort_by_key(|p| p.pack);
        let summary = RebuildSummary {
            packs: done.len() as u64,
            chunks: done.iter().map(|p| p.entries.len() as u64).sum(),
            superseded: existing.len() as u64,
        };
        let mut blob = IndexBlob::new(done);
        blob.supersedes = existing;
        self.write_index(blob).await?;
        Ok(summary)
    }

    /// 讀一個 pack 的 trailer → IndexPack。只 range read 檔尾與 trailer。
    async fn read_pack_index(&self, id: ObjectId, size: u64) -> Result<IndexPack> {
        let key = keys::pack(&id);
        let corrupt = |reason: String| CoreError::Corrupt {
            key: key.clone(),
            reason,
        };
        if size < (pack::HEADER_LEN + FOOTER_LEN) as u64 {
            return Err(corrupt(format!("pack is only {size} bytes")));
        }
        let footer = self
            .backend()
            .get_range(&key, size - FOOTER_LEN as u64..size)
            .await?;
        let trailer_len = pack::parse_footer(&footer).map_err(|e| corrupt(e.to_string()))?;
        let trailer_start = size
            .checked_sub(FOOTER_LEN as u64 + trailer_len)
            .filter(|s| *s >= pack::HEADER_LEN as u64)
            .ok_or_else(|| corrupt(format!("trailer length {trailer_len} does not fit")))?;
        let trailer_bytes = self
            .backend()
            .get_range(&key, trailer_start..size - FOOTER_LEN as u64)
            .await?;
        let keys_ = Arc::clone(self.keys());
        let key_for_task = key.clone();
        let trailer = blocking(move || {
            keys_
                .open_pack_trailer(&trailer_bytes)
                .map_err(|e| CoreError::Corrupt {
                    key: key_for_task.clone(),
                    reason: format!("trailer: {e}"),
                })
                .and_then(|plain| {
                    kist_format::cbor::decode::<kist_format::pack::PackTrailer>(&plain).map_err(
                        |e| CoreError::Corrupt {
                            key: key_for_task.clone(),
                            reason: format!("trailer: {e}"),
                        },
                    )
                })
        })
        .await?;
        Ok(IndexPack {
            pack: id,
            size,
            entries: trailer.entries,
        })
    }
}
