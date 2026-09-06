//! pack 的寫入與讀取（v2）。
//!
//! 寫入端 [`PackWriter`] 把 chunk 一個個加進 buffer（先壓縮、再加密），滿了或 backup 結束時
//! [`PackWriter::finish`] 封上 trailer 並算出 pack 名稱；上傳由呼叫端負責。
//! 讀取端只需要 [`decode_chunk`]（單一 entry → 明文並驗證 chunk ID）與
//! [`read_trailer`]（整個 pack → trailer）。
//!
//! 壓縮的演算法 byte 是 AEAD 明文的第一個 byte（self-describing），
//! trailer entry 不需要 flags。壓縮門檻：zstd-3，沒省下 > 1/16 就存原文。

use std::sync::Arc;

use kist_crypto::RepoKeys;
use kist_format::pack::{self, PackEntry, PackTrailer};
use kist_format::{cbor, Algorithm, ChunkId, ObjectId};

use crate::{CoreError, Result};

/// chunk 壓縮等級。
const ZSTD_LEVEL: i32 = 3;

/// 壓縮 chunk 明文：回傳 `algorithm byte ‖ 資料`。
pub fn compress_chunk(raw: &[u8]) -> Result<Vec<u8>> {
    let compressed = zstd::encode_all(raw, ZSTD_LEVEL).map_err(|e| CoreError::Corrupt {
        key: "<chunk>".to_owned(),
        reason: format!("zstd failed: {e}"),
    })?;
    // 沒省下 > 1/16（6.25%）就存原文。
    if compressed.len() < raw.len() - raw.len() / 16 {
        let mut out = Vec::with_capacity(1 + compressed.len());
        out.push(Algorithm::Zstd as u8);
        out.extend_from_slice(&compressed);
        Ok(out)
    } else {
        let mut out = Vec::with_capacity(1 + raw.len());
        out.push(Algorithm::Raw as u8);
        out.extend_from_slice(raw);
        Ok(out)
    }
}

/// 解開 `algorithm byte ‖ 資料`。
fn decompress_chunk(payload: &[u8]) -> Result<Vec<u8>> {
    let Some((algorithm, data)) = payload.split_first() else {
        return Err(CoreError::Corrupt {
            key: "<chunk>".to_owned(),
            reason: "chunk payload is empty (no algorithm byte)".to_owned(),
        });
    };
    match Algorithm::from_u8(*algorithm)? {
        Algorithm::Raw => Ok(data.to_vec()),
        Algorithm::Zstd => zstd::decode_all(data).map_err(|e| CoreError::Corrupt {
            key: "<chunk>".to_owned(),
            reason: format!("zstd decode failed: {e}"),
        }),
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
    /// 單一 sealed chunk 的上限（`chunker.max` + 1 演算法 byte + nonce/tag）。
    /// buf 一次配到 `target + 上限`，extend 永遠不會觸發 Vec 倍增——
    /// 否則 64 MiB 的 target 會長出 128 MiB 級的容量（backup 峰值大戶）。
    max_sealed: usize,
    buf: Vec<u8>,
    entries: Vec<PackEntry>,
}

impl PackWriter {
    /// `max_chunk` = repo config 的 chunker.max：sealed 後單一 chunk 最長
    /// `max_chunk + 1 + 40` bytes，buf 已含 target 之外的一份，塞入不會溢位。
    pub fn new(keys: Arc<RepoKeys>, target_size: u64, max_chunk: u32) -> Self {
        let target_size = usize::try_from(target_size).unwrap_or(usize::MAX);
        let max_sealed = max_chunk as usize + 1 + (pack::CHUNK_NONCE_LEN + pack::TAG_LEN);
        let buf = Self::empty_buf(target_size, max_sealed);
        Self {
            keys,
            target_size,
            max_sealed,
            buf,
            entries: Vec::new(),
        }
    }

    fn empty_buf(target: usize, max_sealed: usize) -> Vec<u8> {
        // 檔頭 magic 先種好：offset 從 magic 之後開始算（原本由 pack::begin() 提供）。
        let mut b = Vec::with_capacity(target.saturating_add(max_sealed));
        b.extend_from_slice(pack::magic().as_slice());
        b
    }

    fn fresh_buf(&self) -> Vec<u8> {
        Self::empty_buf(self.target_size, self.max_sealed)
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn is_full(&self) -> bool {
        self.buf.len() >= self.target_size
    }

    /// 壓縮、加密並加入 buffer。回傳這個 chunk 在 pack 裡的位置。
    pub fn add(&mut self, id: ChunkId, plaintext: &[u8]) -> Result<PackEntry> {
        let payload = compress_chunk(plaintext)?;
        let sealed = self.keys.seal_chunk(&id, &payload)?;
        let entry = PackEntry {
            id,
            offset: self.buf.len() as u64,
            length: sealed.len() as u64,
            raw_len: plaintext.len() as u64,
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
        let fresh = self.fresh_buf();
        let buf = std::mem::replace(&mut self.buf, fresh);
        let trailer = cbor::encode(&PackTrailer::new(entries.clone()))?;
        let trailer_sealed = self.keys.seal_pack_trailer(&trailer)?;
        let bytes = pack::finish(buf, &trailer_sealed);
        let id = ObjectId::of(&bytes);
        Ok(Some(FinishedPack { id, bytes, entries }))
    }
}

/// 解開一個 pack entry：解密、（必要時）解壓、驗證長度與 chunk ID。
pub fn decode_chunk(
    keys: &RepoKeys,
    id: &ChunkId,
    entry_bytes: &[u8],
    raw_len: u64,
) -> Result<Vec<u8>> {
    let payload = keys.open_chunk(id, entry_bytes)?;
    let plaintext = decompress_chunk(&payload)?;
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

/// 從整個 pack 的 bytes 讀出 trailer，並驗證版本與 trailer 的一致性
/// （規格 §7：entries 從檔頭 magic 之後連續排列、完整覆蓋資料區、無重複
/// ID、長度在格式上限內）。trailer 是認證過的，但「認證」不等於「一致」：
/// 有 bug 的 client 寫出的 pack 一樣有有效 tag。
pub fn read_trailer(keys: &RepoKeys, pack_bytes: &[u8]) -> Result<PackTrailer> {
    let sealed = pack::trailer_bytes(pack_bytes)?;
    let plain = keys.open_pack_trailer(sealed)?;
    let data_end = pack_bytes.len() - pack::FOOTER_LEN - sealed.len();
    let trailer: PackTrailer = cbor::decode(&plain)?;
    if trailer.version != kist_format::FORMAT_VERSION {
        return Err(CoreError::Corrupt {
            key: "<pack trailer>".to_owned(),
            reason: format!(
                "trailer declares version {}, this build reads {}",
                trailer.version,
                kist_format::FORMAT_VERSION
            ),
        });
    }
    validate_trailer(&trailer, data_end)?;
    Ok(trailer)
}

/// trailer 一致性檢查。`max_entry_len` 的格式上限 = chunker.max
/// (最大 64 MiB) + 1 (algorithm byte) + 40 (nonce+tag)；這裡用格式允許
/// 的最大 chunker.max 推導，避免 trailer 檢查反過來依賴每個 repo 的 config。
pub fn validate_trailer(trailer: &PackTrailer, data_end: usize) -> Result<()> {    const MAX_CHUNKER_MAX: u64 = 64 * 1024 * 1024;
    const MAX_ENTRY_LEN: u64 = MAX_CHUNKER_MAX + 1 + (pack::CHUNK_NONCE_LEN + pack::TAG_LEN) as u64;
    if trailer.entries.is_empty() {
        return Err(CoreError::Corrupt {
            key: "<pack trailer>".to_owned(),
            reason: "trailer lists no chunks".to_owned(),
        });
    }
    let mut next = pack::HEADER_LEN as u64;
    let mut seen = std::collections::HashSet::with_capacity(trailer.entries.len());
    for (i, e) in trailer.entries.iter().enumerate() {
        if e.offset != next {
            return Err(CoreError::Corrupt {
                key: "<pack trailer>".to_owned(),
                reason: format!("entry {i} starts at {}, expected {next}", e.offset),
            });
        }
        if e.length < pack::MIN_ENTRY_LEN as u64 {
            return Err(CoreError::Corrupt {
                key: "<pack trailer>".to_owned(),
                reason: format!(
                    "entry {i} is {} bytes, shorter than an empty sealed chunk",
                    e.length
                ),
            });
        }
        if e.length > MAX_ENTRY_LEN {
            return Err(CoreError::Corrupt {
                key: "<pack trailer>".to_owned(),
                reason: format!(
                    "entry {i} is {} bytes, over the {MAX_ENTRY_LEN} a sealed chunk can be",
                    e.length
                ),
            });
        }
        let end = e.offset + e.length;
        if end > data_end as u64 {
            return Err(CoreError::Corrupt {
                key: "<pack trailer>".to_owned(),
                reason: format!("entry {i} ends at {end}, past the {data_end} bytes of chunk data"),
            });
        }
        if !seen.insert(e.id) {
            return Err(CoreError::Corrupt {
                key: "<pack trailer>".to_owned(),
                reason: format!("chunk {} is listed twice", e.id),
            });
        }
        next = end;
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use kist_crypto::MasterKey;

    fn test_keys() -> Arc<RepoKeys> {
        Arc::new(RepoKeys::from_master(&MasterKey::from_bytes([0x42; 32])))
    }

    fn max_sealed_of(max_chunk: u32) -> usize {
        max_chunk as usize + 1 + (pack::CHUNK_NONCE_LEN + pack::TAG_LEN)
    }

    /// 不可壓縮的偽隨機資料（xorshift64*）：zstd 壓不動，sealed 後長度
    /// 正好是 len + 41，entries 數字才可控。
    fn incompressible(seed: u64, len: usize) -> Vec<u8> {
        let mut x = seed | 1;
        (0..len)
            .map(|_| {
                x ^= x >> 12;
                x ^= x << 25;
                x ^= x >> 27;
                (x.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 24) as u8
            })
            .collect()
    }

    /// buf 的容量要一次配到 `target + max_sealed`，跨多輪 finish 不變——
    /// 之前 Vec 從 8 bytes 倍增到 128 MiB 級，是 backup 峰值記憶體的大戶。
    #[test]
    fn buf_capacity_is_exact_and_stable_across_packs() {
        let keys = test_keys();
        let target = 16 * 1024 * 1024u64;
        let max_chunk = 1024 * 1024u32;
        let mut w = PackWriter::new(Arc::clone(&keys), target, max_chunk);
        let cap0 = w.buf.capacity();
        assert_eq!(
            cap0,
            target as usize + max_sealed_of(max_chunk),
            "容量應該一次配到位"
        );
        let chunk = incompressible(7, 1024 * 1024);
        let id = keys.chunk_id(&chunk);
        for round in 0..3 {
            while !w.is_full() {
                w.add(id, &chunk).unwrap();
            }
            let fp = w.finish().unwrap().unwrap();
            assert!(
                fp.bytes.capacity() <= cap0,
                "round {round}: 交出去的 pack bytes 容量 {} 超過上限 {cap0}",
                fp.bytes.capacity()
            );
            assert_eq!(w.buf.capacity(), cap0, "round {round}: 新 buf 容量走樣");
        }
    }

    /// 貼著 target 塞入一個 max-size chunk（實際流程裡 `is_full` 在 add 之後
    /// 才檢查，所以 len 可以到 `target + max_sealed`）：不能觸發重配。
    #[test]
    fn oversized_last_chunk_stays_within_capacity() {
        let keys = test_keys();
        let target = 3 * 1024 * 1024u64;
        let max_chunk = 1024 * 1024u32;
        let mut w = PackWriter::new(Arc::clone(&keys), target, max_chunk);
        let cap = w.buf.capacity();
        let big = incompressible(1, max_chunk as usize);
        let small = incompressible(2, 64 * 1024);
        // 先用小 chunk 墊到 len < target（實際流程裡 ≥ target 就會 flush）
        while w.buf.len() < target as usize - max_chunk as usize {
            let id = keys.chunk_id(&small);
            w.add(id, &small).unwrap();
        }
        assert!(w.buf.len() < target as usize);
        let id = keys.chunk_id(&big);
        w.add(id, &big).unwrap();
        assert_eq!(w.buf.capacity(), cap, "超標 chunk 觸發了重配");
        let fp = w.finish().unwrap().unwrap();
        // trailer 貼著容量塞時放不下會重配一次（倍增上界）
        assert!(fp.bytes.capacity() <= 2 * cap);
    }

    #[test]
    fn finish_without_chunks_resets_cleanly() {
        let keys = test_keys();
        let mut w = PackWriter::new(keys, 64 * 1024 * 1024, 8 * 1024 * 1024);
        assert!(w.finish().unwrap().is_none());
        assert!(w.is_empty());
    }
}
