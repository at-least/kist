//! `config` 物件：repo 參數與 master key 的封裝。
//!
//! 這是 repo 裡**唯一以明文 CBOR 存放**的物件（打開 repo 前需要它裡面的 KDF 參數），
//! 也是唯一允許覆寫的物件（換密碼時）。裡面沒有任何祕密：master key 已被 KEK 包住。
//!
//! 金鑰階層：
//! ```text
//! password ──Argon2id(salt, params)──▶ KEK ──AEAD 解開──▶ master key
//! master key ──blake3::derive_key(context)──▶ chunk key / hash key / object key / nonce key
//! ```

use serde::{Deserialize, Serialize};

use crate::{FormatError, Result, FORMAT_VERSION};

/// KDF 演算法名稱，寫進 config 讓未來可以換。
pub const KDF_ARGON2ID: &str = "argon2id";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoConfig {
    pub version: u32,
    /// 隨機 16 bytes，用來區分不同 repo（例如本地快取的命名）。
    #[serde(with = "serde_bytes")]
    pub repo_id: Vec<u8>,
    /// RFC 3339 UTC。
    pub created: String,
    pub chunker: ChunkerParams,
    /// pack 寫滿多少 bytes 就 flush。
    pub pack_target_size: u64,
    /// 第 0 個 key slot：由密碼推導的 KEK 包住的 master key。
    pub key: KeySlot,
}

/// pack 目標大小的允許範圍。
pub const MIN_PACK_TARGET_SIZE: u64 = 64 * 1024;
pub const MAX_PACK_TARGET_SIZE: u64 = 4 * 1024 * 1024 * 1024;

impl RepoConfig {
    pub fn new(repo_id: Vec<u8>, created: String, key: KeySlot) -> Self {
        Self {
            version: FORMAT_VERSION,
            repo_id,
            created,
            chunker: ChunkerParams::default(),
            pack_target_size: 64 * 1024 * 1024,
            key,
        }
    }

    /// config 是明文，讀進來的任何數字都不可信：使用前先確認在合理範圍內，
    /// 否則荒謬的值會讓 chunker 越界或配置巨量記憶體。
    pub fn validate(&self) -> Result<()> {
        if self.repo_id.len() != 16 {
            return Err(FormatError::InvalidParams(format!(
                "repo_id must be 16 bytes, got {}",
                self.repo_id.len()
            )));
        }
        self.chunker.validate()?;
        if self.pack_target_size < MIN_PACK_TARGET_SIZE
            || self.pack_target_size > MAX_PACK_TARGET_SIZE
        {
            return Err(FormatError::InvalidParams(format!(
                "pack_target_size {} is outside {MIN_PACK_TARGET_SIZE}..={MAX_PACK_TARGET_SIZE}",
                self.pack_target_size
            )));
        }
        if u64::from(self.chunker.max) > self.pack_target_size {
            return Err(FormatError::InvalidParams(
                "chunker.max must not exceed pack_target_size".to_owned(),
            ));
        }
        Ok(())
    }
}

/// FastCDC 參數（bytes）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChunkerParams {
    pub min: u32,
    pub avg: u32,
    pub max: u32,
}

impl ChunkerParams {
    /// FastCDC 的硬性限制（它在 release build 不檢查，越界會 panic）再加上合理上限。
    pub fn validate(&self) -> Result<()> {
        let bad = |msg: String| Err(FormatError::InvalidParams(msg));
        if self.min < 64 || self.min > 1024 * 1024 {
            return bad(format!("chunker.min {} is outside 64..=1 MiB", self.min));
        }
        if self.avg < 256 || self.avg > 16 * 1024 * 1024 {
            return bad(format!("chunker.avg {} is outside 256..=16 MiB", self.avg));
        }
        if self.max < 1024 || self.max > 64 * 1024 * 1024 {
            return bad(format!(
                "chunker.max {} is outside 1 KiB..=64 MiB",
                self.max
            ));
        }
        if !(self.min <= self.avg && self.avg <= self.max) {
            return bad(format!(
                "chunker sizes must satisfy min <= avg <= max, got {}/{}/{}",
                self.min, self.avg, self.max
            ));
        }
        Ok(())
    }
}

impl Default for ChunkerParams {
    fn default() -> Self {
        Self {
            min: 512 * 1024,
            avg: 2 * 1024 * 1024,
            max: 8 * 1024 * 1024,
        }
    }
}

/// 一個 key slot：某組密碼可以解開 master key。
/// slot 0 放在 `config`，其餘放 `keys/<id>`（用 envelope 包、以 master key 加密的話就失去意義，
/// 所以 `keys/<id>` 也是明文 CBOR）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeySlot {
    pub version: u32,
    /// 人看的名稱，例如 "default"、"recovery"。
    pub name: String,
    /// RFC 3339 UTC。
    pub created: String,
    pub kdf: KdfParams,
    pub wrapped_master_key: WrappedKey,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KdfParams {
    /// 目前只有 [`KDF_ARGON2ID`]。
    pub algorithm: String,
    pub m_cost_kib: u32,
    pub t_cost: u32,
    pub p_cost: u32,
    #[serde(with = "serde_bytes")]
    pub salt: Vec<u8>,
}

/// 用 KEK 做 XChaCha20-Poly1305 包住的 32-byte master key。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WrappedKey {
    #[serde(with = "serde_bytes")]
    pub nonce: Vec<u8>,
    /// 32-byte key + 16-byte tag。
    #[serde(with = "serde_bytes")]
    pub ciphertext: Vec<u8>,
}
