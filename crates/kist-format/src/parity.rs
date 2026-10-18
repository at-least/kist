//! parity（選配；`docs/format-v4.md` §13）：Reed-Solomon 對**整個寫進 repo 的 pack 或 mpack**
//! 的同位，讓壞掉的 pack 能在記憶體裡修回來（v4 不覆寫，修好的 bytes 由 prune 寫進新的 pack）。
//! 排版與版本無關，`v` 由呼叫端給（[`crate::v4::parity`] 給 4）。
//!
//! 物件是明文 CBOR，欄位表順序：
//! `{v, k, m, pack_size, shard_len, hashes, parity}`。明文是有意的：
//! RS 對密文做線性組合不洩漏資訊，shard hash 是密文的 hash，而偽造或
//! 損壞的 parity 最多讓修復**失敗**、不可能修**錯**——修復結果的證明是
//! 重算 BLAKE3 必須等於 pack 自己的名字。因此 scrub 不需要 repo 密碼。
//!
//! k=16 是固定的（m 片同位的開銷固定是 pack 的 m/16，讀取端無需協商）；
//! m 1..=8。shard hash 用無 key 的 BLAKE3-256（與 pack 名稱同一函式）。
//!
//! RS 數學用 `reed-solomon-erasure`（Backblaze JavaReedSolomon 的移植，GF(2^8)、Vandermonde 矩陣）；
//! 曾以固定向量驗證與 klauspost/reedsolomon 逐 byte 相容。編碼輸出由凍結
//! golden `tests/testdata/parity-golden.txt`（v3 錄的，v4 只差 `v` 那一個 byte）與 `tests/golden-v4/parity.cbor`
//! 釘住，矩陣漂移會讓測試變紅。

use reed_solomon_erasure::galois_8::Field;
use reed_solomon_erasure::ReedSolomon;
use serde::{Deserialize, Serialize};

use crate::cbor;
use crate::ids::ObjectId;
use crate::{FormatError, Result};

/// 固定的資料 shard 數（見模組文件）。
pub const DATA_SHARDS: usize = 16;

/// m 的上限：一半資料量的冗餘之上，答案應該是第二個 repo 而不是更多同位。
pub const MAX_PARITY_SHARDS: usize = 8;

/// 單一 shard 長度的上限（16 資料片 → parity 最後支援到 1 GiB 的 pack）。
/// 擋下偽造 header 要求的大分配；`encode` 對超過此界限的 pack 直接拒絕
/// （否則會寫出 `parse` 永遠拒收、修復端用不了的 sidecar）。
/// 注意：parse 層面偽造 header 仍可要求 pack_size 到 16 × MAX_SHARD_LEN，
/// repair 的瞬時分配上界約 3 GiB——fuzz 時要知道。
const MAX_SHARD_LEN: usize = 64 << 20;

/// parity sidecar 物件。欄位宣告順序 = 規格欄位表順序（= CBOR 輸出順序）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Object {
    pub v: u32,
    pub k: u8,
    pub m: u8,
    pub pack_size: u64,
    pub shard_len: u32,
    /// 每個 shard 的 BLAKE3-256：資料片在前、同位片在後。找壞片用的。
    pub hashes: Vec<ObjectId>,
    /// m 個同位 shard（byte string，major type 2）。
    pub parity: Vec<serde_bytes::ByteBuf>,
}

/// 對一個完整的 pack（`pack` 的 BLAKE3 必須等於 `pack_id`）算出 m 片同位的物件 bytes，`v` 填
/// `version`。確定性：同樣的 pack、m 與版本永遠得到同樣的物件。呼叫端是 [`crate::v4::parity::encode`]。
pub fn encode_version(pack_id: &ObjectId, pack: &[u8], m: usize, version: u32) -> Result<Vec<u8>> {
    let corrupt = |msg: String| FormatError::ParityCorrupt(msg);
    if !(1..=MAX_PARITY_SHARDS).contains(&m) {
        return Err(corrupt(format!(
            "{m} parity shards, want 1..{MAX_PARITY_SHARDS}"
        )));
    }
    if pack.is_empty() {
        return Err(corrupt("empty pack".to_owned()));
    }
    if ObjectId::of(pack) != *pack_id {
        return Err(corrupt(format!(
            "pack hashes to {}, not {pack_id}",
            ObjectId::of(pack)
        )));
    }
    let shard_len = pack.len().div_ceil(DATA_SHARDS);
    if shard_len > MAX_SHARD_LEN {
        // 再大就沒有能讀它的修復端：不寫比寫出廢物好（呼叫端只會少冗餘，
        // 不會少資料）。
        return Err(corrupt(format!(
            "pack of {} bytes needs {shard_len}-byte shards, over the {} limit",
            pack.len(),
            MAX_SHARD_LEN
        )));
    }
    let mut shards = split(pack, shard_len, m);
    ReedSolomon::<Field>::new(DATA_SHARDS, m)
        .map_err(|e| corrupt(format!("coder: {e}")))?
        .encode(&mut shards)
        .map_err(|e| corrupt(format!("encode: {e}")))?;

    let obj = Object {
        v: version,
        k: DATA_SHARDS as u8,
        m: m as u8,
        pack_size: pack.len() as u64,
        shard_len: shard_len as u32,
        hashes: shards.iter().map(|s| ObjectId::of(s)).collect(),
        parity: shards[DATA_SHARDS..]
            .iter()
            .map(|s| serde_bytes::ByteBuf::from(s.clone()))
            .collect(),
    };
    cbor::encode(&obj)
}

/// 解碼並驗證 parity 物件，`v` 必須等於 `version`。每個 bound 都在依 header 配置記憶體**之前**檢查。
/// 呼叫端是 [`crate::v4::parity::parse`]。
pub fn parse_version(data: &[u8], version: u32) -> Result<Object> {
    let obj: Object = cbor::decode(data)?;
    obj.validate(version)?;
    Ok(obj)
}

impl Object {
    pub fn parity_shards(&self) -> usize {
        self.m as usize
    }

    fn validate(&self, version: u32) -> Result<()> {
        let fail = |msg: String| Err(FormatError::ParityCorrupt(msg));
        let shard_len = self.shard_len as usize;
        if self.v != version {
            return fail(format!("version {}, this build reads {version}", self.v));
        }
        if self.k as usize != DATA_SHARDS {
            return fail(format!("{} data shards, want {DATA_SHARDS}", self.k));
        }
        let m = self.m as usize;
        if !(1..=MAX_PARITY_SHARDS).contains(&m) {
            return fail(format!("{m} parity shards, want 1..{MAX_PARITY_SHARDS}"));
        }
        if shard_len == 0 || shard_len > MAX_SHARD_LEN {
            return fail(format!("shard length {shard_len}"));
        }
        if self.pack_size == 0
            || self.pack_size.div_ceil(DATA_SHARDS as u64) != self.shard_len as u64
        {
            // shard_len 必須恰好 = ceil(pack_size/16)：不一致就是 header 自相矛盾。
            return fail(format!(
                "pack size {} does not fit {DATA_SHARDS} shards of {shard_len} bytes",
                self.pack_size
            ));
        }
        if self.hashes.len() != DATA_SHARDS + m {
            return fail(format!(
                "{} hashes, want {}",
                self.hashes.len(),
                DATA_SHARDS + m
            ));
        }
        if self.parity.len() != m {
            return fail(format!(
                "{} parity shards, header says {m}",
                self.parity.len()
            ));
        }
        for (i, p) in self.parity.iter().enumerate() {
            if p.as_ref().len() != shard_len {
                return fail(format!(
                    "parity shard {i} is {} bytes, want {shard_len}",
                    p.as_ref().len()
                ));
            }
        }
        Ok(())
    }

    /// 從受損的 pack bytes 與這份 parity 重建完整 pack。
    ///
    /// hash 對不上的 shard 是 erasure；pack 過長或過短的部分視同損壞。
    /// 結果只在重算 hash 等於 `pack_id` 時回傳——parity 裡沒有任何東西
    /// 被信到那個程度之外。
    pub fn repair(&self, pack_id: &ObjectId, damaged: &[u8]) -> Result<Vec<u8>> {
        let m = self.m as usize;
        let shard_len = self.shard_len as usize;
        let pack_size = self.pack_size as usize;
        let padded = pad_or_trim(damaged, pack_size);
        let mut shards = split(&padded, shard_len, m);
        for (i, p) in self.parity.iter().enumerate() {
            shards[DATA_SHARDS + i].copy_from_slice(p.as_ref());
        }

        let mut erasures = 0usize;
        for (i, s) in shards.iter_mut().enumerate() {
            if ObjectId::of(s) != self.hashes[i] {
                *s = Vec::new(); // 標成要重建；長度等下由 reconstruct 填
                erasures += 1;
            }
        }
        if erasures == 0 {
            // shard 層面看不出任何問題，但呼叫端說 pack 壞了：parity 不屬於
            // 這個 pack，或 hash 被偽造。兩者都不修。
            if ObjectId::of(&padded) == *pack_id {
                return Ok(padded);
            }
            return Err(FormatError::Unrepairable(
                "every shard matches the parity's hashes, but the pack does not hash to its name; the parity is not for this pack".to_owned(),
            ));
        }
        if erasures > m {
            return Err(FormatError::Unrepairable(format!(
                "{erasures} shards are damaged, parity can rebuild {m}"
            )));
        }

        let mut options: Vec<Option<Vec<u8>>> = shards
            .into_iter()
            .map(|s| if s.is_empty() { None } else { Some(s) })
            .collect();
        // 空 Vec 只會是上面依 hash 標成 erasure 的位置：split 出來的 shard
        // 長度都是 shard_len（validate 保證 > 0），內容全 0 的 shard 也不是空的。
        ReedSolomon::<Field>::new(DATA_SHARDS, m)
            .map_err(|e| FormatError::Unrepairable(format!("coder: {e}")))?
            .reconstruct(&mut options)
            .map_err(|e| FormatError::Unrepairable(format!("reconstruct: {e}")))?;
        let mut out = Vec::with_capacity(pack_size);
        for s in options.into_iter().take(DATA_SHARDS).flatten() {
            out.extend_from_slice(&s);
        }
        out.truncate(pack_size);
        if ObjectId::of(&out) != *pack_id {
            return Err(FormatError::Unrepairable(format!(
                "reconstruction hashes to {}, not {pack_id}; the parity is wrong or forged",
                ObjectId::of(&out)
            )));
        }
        Ok(out)
    }
}

/// 把 pack 切成 DATA_SHARDS 個 shard_len 長的資料片（最後一片補零），
/// 後面接 m 個全 0 的同位片給 encoder 填。
fn split(pack: &[u8], shard_len: usize, m: usize) -> Vec<Vec<u8>> {
    let mut padded = vec![0u8; DATA_SHARDS * shard_len];
    padded[..pack.len()].copy_from_slice(pack);
    let mut shards: Vec<Vec<u8>> = (0..DATA_SHARDS)
        .map(|i| padded[i * shard_len..(i + 1) * shard_len].to_vec())
        .collect();
    shards.extend((0..m).map(|_| vec![0u8; shard_len]));
    shards
}

/// 讓 damaged 恰好是 pack_size 長：過短補零、過長截斷。差異會以損壞 shard
/// 的形式在 hash 檢查中現形。
fn pad_or_trim(damaged: &[u8], size: usize) -> Vec<u8> {
    let mut out = vec![0u8; size];
    let n = damaged.len().min(size);
    out[..n].copy_from_slice(&damaged[..n]);
    out
}
