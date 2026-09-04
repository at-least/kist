//! 記憶體內的 chunk index：chunk ID → 在哪個 pack 的哪裡。
//!
//! 內容來自 repo 裡所有 `indexes/*` blob；backup 過程中新寫的 chunk 也會即時加進來
//! （pack 還沒 flush 前先用零值 pack 名稱佔位，flush 後再補上真正的名稱）。

use std::collections::HashMap;

use kist_format::index::IndexPack;
use kist_format::pack::PackEntry;
use kist_format::{ChunkId, ObjectId};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkLocation {
    pub pack: ObjectId,
    pub offset: u64,
    pub length: u64,
    pub raw_len: u64,
    pub flags: u8,
}

/// 尚未 flush 的 pack 用這個名稱佔位。
pub const PENDING_PACK: ObjectId = ObjectId::from_bytes([0; 32]);

#[derive(Debug, Default, Clone)]
pub struct ChunkIndex {
    chunks: HashMap<ChunkId, ChunkLocation>,
    /// pack 名稱 → 檔案大小（`check` 用）。
    packs: HashMap<ObjectId, u64>,
}

impl ChunkIndex {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn contains(&self, id: &ChunkId) -> bool {
        self.chunks.contains_key(id)
    }

    pub fn get(&self, id: &ChunkId) -> Option<&ChunkLocation> {
        self.chunks.get(id)
    }

    pub fn len(&self) -> usize {
        self.chunks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.chunks.is_empty()
    }

    pub fn chunks(&self) -> impl Iterator<Item = (&ChunkId, &ChunkLocation)> {
        self.chunks.iter()
    }

    pub fn packs(&self) -> impl Iterator<Item = (&ObjectId, &u64)> {
        self.packs.iter()
    }

    pub fn pack_count(&self) -> usize {
        self.packs.len()
    }

    /// 把一個 index blob 裡的 pack 加進來。同一個 chunk 出現在多個 pack 時保留先加入的。
    pub fn add_pack(&mut self, pack: &IndexPack) {
        self.packs.insert(pack.pack, pack.size);
        for e in &pack.entries {
            self.chunks.entry(e.id).or_insert(location(pack.pack, e));
        }
    }

    /// backup 進行中：pack 尚未 flush，先佔位。
    pub fn add_pending(&mut self, entry: &PackEntry) {
        self.chunks
            .entry(entry.id)
            .or_insert(location(PENDING_PACK, entry));
    }

    /// pack flush 完成：把佔位的名稱換成真正的 pack 名稱。
    pub fn resolve_pending(&mut self, pack: ObjectId, size: u64, entries: &[PackEntry]) {
        self.packs.insert(pack, size);
        for e in entries {
            if let Some(loc) = self.chunks.get_mut(&e.id) {
                if loc.pack == PENDING_PACK {
                    *loc = location(pack, e);
                }
            }
        }
    }
}

fn location(pack: ObjectId, e: &PackEntry) -> ChunkLocation {
    ChunkLocation {
        pack,
        offset: e.offset,
        length: e.length,
        raw_len: e.raw_len,
        flags: e.flags,
    }
}
