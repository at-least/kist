//! pack 的寫入與讀取。
//!
//! 寫入端 [`PackWriter`] 把 chunk 一個個加進 buffer（先壓縮、再加密），滿了或 backup 結束時
//! [`PackWriter::finish`] 封上 trailer 並算出 pack 名稱；上傳由呼叫端負責。
//! 讀取端只需要 [`decode_chunk`]（單一 entry → 明文並驗證 chunk ID）與
//! [`read_trailer`]（整個 pack → trailer）。

use std::sync::Arc;

use kist_crypto::RepoKeys;
use kist_format::envelope::{Compression, ObjectKind};
use kist_format::pack::{self, PackEntry, PackTrailer, FLAG_ZSTD};
use kist_format::{cbor, ChunkId, ObjectId};

use crate::{CoreError, Result};

/// chunk 壓縮等級。
const ZSTD_LEVEL: i32 = 3;

/// 壓縮後沒有小於原大小的 97% 就視為不可壓縮，存原文。
pub fn compress_chunk(raw: &[u8]) -> Result<(Vec<u8>, u8)> {
    let compressed = zstd::encode_all(raw, ZSTD_LEVEL).map_err(|e| CoreError::Corrupt {
        key: "<chunk>".to_owned(),
        reason: format!("zstd failed: {e}"),
    })?;
    if compressed.len() * 100 < raw.len() * 97 {
        Ok((compressed, FLAG_ZSTD))
    } else {
        Ok((raw.to_vec(), 0))
    }
}

/// 一個已封好、待上傳的 pack。
#[derive(Debug)]
pub struct FinishedPack {
    pub id: ObjectId,
    pub bytes: Vec<u8>,
    pub entries: Vec<PackEntry>,
}

pub struct PackWriter {
    keys: Arc<RepoKeys>,
    target_size: usize,
    buf: Vec<u8>,
    entries: Vec<PackEntry>,
}

impl PackWriter {
    pub fn new(keys: Arc<RepoKeys>, target_size: u64) -> Self {
        Self {
            keys,
            target_size: usize::try_from(target_size).unwrap_or(usize::MAX),
            buf: pack::begin(),
            entries: Vec::new(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn is_full(&self) -> bool {
        self.buf.len() >= self.target_size
    }

    /// 壓縮、加密並加入 buffer。回傳這個 chunk 在 pack 裡的位置。
    pub fn add(&mut self, id: ChunkId, plaintext: &[u8]) -> Result<PackEntry> {
        let (payload, flags) = compress_chunk(plaintext)?;
        let sealed = self.keys.seal_chunk(&id, &payload)?;
        let entry = PackEntry {
            id,
            offset: self.buf.len() as u64,
            length: sealed.len() as u64,
            raw_len: plaintext.len() as u64,
            flags,
        };
        self.buf.extend_from_slice(&sealed);
        self.entries.push(entry);
        Ok(entry)
    }

    /// 封上 trailer，交出完整的 pack；writer 自己重置成空的。沒有任何 chunk 時回 `None`。
    pub fn finish(&mut self) -> Result<Option<FinishedPack>> {
        if self.entries.is_empty() {
            return Ok(None);
        }
        let entries = std::mem::take(&mut self.entries);
        let buf = std::mem::replace(&mut self.buf, pack::begin());
        let trailer = cbor::encode(&PackTrailer::new(entries.clone()))?;
        let trailer_env =
            self.keys
                .seal_object(ObjectKind::PackTrailer, Compression::Zstd, &trailer)?;
        let bytes = pack::finish(buf, &trailer_env);
        let id = ObjectId::of(&bytes);
        Ok(Some(FinishedPack { id, bytes, entries }))
    }
}

/// 解開一個 pack entry：解密、（必要時）解壓、驗證長度與 chunk ID。
pub fn decode_chunk(
    keys: &RepoKeys,
    id: &ChunkId,
    entry_bytes: &[u8],
    flags: u8,
    raw_len: u64,
) -> Result<Vec<u8>> {
    let payload = keys.open_chunk(id, entry_bytes)?;
    let plaintext = if flags & FLAG_ZSTD != 0 {
        zstd::decode_all(payload.as_slice()).map_err(|e| CoreError::Corrupt {
            key: format!("chunk {id}"),
            reason: format!("zstd decode failed: {e}"),
        })?
    } else {
        payload
    };
    if plaintext.len() as u64 != raw_len {
        return Err(CoreError::Corrupt {
            key: format!("chunk {id}"),
            reason: format!(
                "length {} does not match index ({raw_len})",
                plaintext.len()
            ),
        });
    }
    let actual = keys.chunk_id(&plaintext);
    if actual != *id {
        return Err(CoreError::Corrupt {
            key: format!("chunk {id}"),
            reason: format!("content hash mismatch (got {actual})"),
        });
    }
    Ok(plaintext)
}

/// 從整個 pack 的 bytes 讀出 trailer。
pub fn read_trailer(keys: &RepoKeys, pack_bytes: &[u8]) -> Result<PackTrailer> {
    let env = pack::trailer_bytes(pack_bytes)?;
    let plain = keys.open_object(ObjectKind::PackTrailer, env)?;
    Ok(cbor::decode(&plain)?)
}
