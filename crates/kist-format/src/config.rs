//! `config` 物件（v3）：repo 參數與 master key 的封裝。
//!
//! repo 裡**唯一以明文 CBOR 存放**的物件（打開 repo 前需要裡面的 KDF 參數），
//! 也是唯一允許覆寫的物件（換密碼、調非不變式參數）。裡面沒有祕密：
//! master key 已被 KEK 包住，salt 與 KDF 參數本來就是公開的。
//!
//! v3 的防竄改方向反轉：**不變式（repo_id、chunker 參數）放在 wrapped
//! master 的密文裡**（[`Invariants`]），解鎖時從通過 Poly1305 認證的明文
//! 取出權威值，與這份明文 config 比對——不符即「config 已被竄改」的明確
//! 錯誤。v2 把欄位列舉進 AAD，每加一個不變式都要改 AAD 排版；v3 加欄位
//! 只加進 Invariants（零值省略＋忽略未知＋never-round-trip 適用）。
//!
//! `wrapped` 是 `nonce(24) ‖ AEAD密文(master(32) ‖ Invariants CBOR) ‖ tag(16)`
//! 的單一 byte string；AAD = 常數 [`crate::AAD_MASTER`]。

use serde::{Deserialize, Serialize};

use crate::{FormatError, Result, FORMAT_VERSION};

/// KDF 演算法名稱，寫進 config 讓未來可以換。
pub const KDF_ARGON2ID: &str = "argon2id";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoConfig {
    #[serde(rename = "v")]
    pub version: u32,
    /// 隨機 16 bytes，用來區分不同 repo。必須 == [`Invariants::repo_id`]。
    #[serde(rename = "repo_id", with = "serde_bytes")]
    pub repo_id: Vec<u8>,
    /// 建立時間（Unix 奈秒，UTC）。
    #[serde(rename = "created")]
    pub created_ns: i64,
    #[serde(rename = "chunker")]
    pub chunker: ChunkerParams,
    /// pack 寫滿多少 bytes 就 flush。
    #[serde(rename = "pack_target")]
    pub pack_target_size: u64,
    /// 能安全讀本 repo 的最低格式版號。讀取端版本 < min_reader → 明確拒絕
    /// （不是靠忽略未知欄位半讀）。只升不降。
    #[serde(rename = "min_reader")]
    pub min_reader: u16,
    /// trees/snapshots 是否寫 `.r1` 副本（0 或 1）。寫入端政策，非不變式。
    #[serde(rename = "replicas")]
    pub replicas: u8,
    /// 第 0 個 key slot。
    #[serde(rename = "slot")]
    pub key: KeySlot,
}

/// pack 目標大小的允許範圍。
pub const MIN_PACK_TARGET_SIZE: u64 = 64 * 1024;
pub const MAX_PACK_TARGET_SIZE: u64 = 4 * 1024 * 1024 * 1024;

/// wrapped master 攜帶的不變式：解開即權威，與明文 config 比對。
/// 加新欄位走零值省略＋忽略未知（never-round-trip 規則適用）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Invariants {
    #[serde(rename = "v")]
    pub version: u32,
    /// 必須 == `RepoConfig.repo_id`（不符 = config 被竄改）。
    #[serde(rename = "repo_id", with = "serde_bytes")]
    pub repo_id: Vec<u8>,
    /// 必須 == `RepoConfig.chunker`（同上）。
    #[serde(rename = "chunker")]
    pub chunker: ChunkerParams,
}

impl Invariants {
    pub fn new(repo_id: Vec<u8>, chunker: ChunkerParams) -> Self {
        Self {
            version: FORMAT_VERSION,
            repo_id,
            chunker,
        }
    }

    pub fn validate(&self) -> Result<()> {
        if self.version != FORMAT_VERSION {
            return Err(FormatError::UnsupportedVersion {
                what: "key invariants",
                version: self.version,
            });
        }
        if self.repo_id.len() != 16 {
            return Err(FormatError::InvalidParams(format!(
                "invariants repo_id must be 16 bytes, got {}",
                self.repo_id.len()
            )));
        }
        self.chunker.validate()
    }

    /// 與明文 config 比對：任何不符都是「config 已被竄改」的明確錯誤
    /// （不是 v2 的「默默解不開」，更不是去重悄悄失效）。
    pub fn check_matches(&self, config: &RepoConfig) -> Result<()> {
        if self.repo_id != config.repo_id || self.chunker != config.chunker {
            return Err(FormatError::InvalidParams(
                "plaintext config does not match the authenticated invariants \
                 (repo_id or chunker parameters were tampered)"
                    .to_owned(),
            ));
        }
        Ok(())
    }
}

impl RepoConfig {
    pub fn new(repo_id: Vec<u8>, created_ns: i64, key: KeySlot) -> Self {
        Self {
            version: FORMAT_VERSION,
            repo_id,
            created_ns,
            chunker: ChunkerParams::default(),
            pack_target_size: 64 * 1024 * 1024,
            min_reader: FORMAT_VERSION.try_into().unwrap_or(3),
            replicas: 0,
            key,
        }
    }

    /// config 是明文，讀進來的任何數字都不可信：使用前先確認在合理範圍內，
    /// 否則荒謬的值會讓 chunker 越界或配置巨量記憶體。
    pub fn validate(&self) -> Result<()> {
        if self.version != FORMAT_VERSION {
            return Err(FormatError::UnsupportedVersion {
                what: "repository config",
                version: self.version,
            });
        }
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
                "pack_target {} is outside {MIN_PACK_TARGET_SIZE}..={MAX_PACK_TARGET_SIZE}",
                self.pack_target_size
            )));
        }
        if u64::from(self.chunker.max) > self.pack_target_size {
            return Err(FormatError::InvalidParams(
                "chunker.max must not exceed pack_target".to_owned(),
            ));
        }
        let min_reader = u32::from(self.min_reader);
        if !(3..=FORMAT_VERSION).contains(&min_reader) {
            return Err(FormatError::InvalidParams(format!(
                "min_reader {} is outside 3..={FORMAT_VERSION}",
                self.min_reader
            )));
        }
        if self.replicas > 1 {
            return Err(FormatError::InvalidParams(format!(
                "replicas {} is outside 0..=1",
                self.replicas
            )));
        }
        self.key.kdf.validate()?;
        Ok(())
    }
}

/// FastCDC 參數（bytes）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChunkerParams {
    #[serde(rename = "min")]
    pub min: u32,
    #[serde(rename = "avg")]
    pub avg: u32,
    #[serde(rename = "max")]
    pub max: u32,
}

impl ChunkerParams {
    /// 切塊參數的硬性限制：越界值會讓掃描越界或配置巨量記憶體。
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
/// slot 0 放在 `config`，其餘放 `keys/<id>`（都是明文 CBOR：以 master key
/// 加密的話就失去「多組密碼」的意義）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeySlot {
    #[serde(rename = "v")]
    pub version: u32,
    /// 人看的名稱，例如 "default"、"recovery"。
    #[serde(rename = "name", default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    /// 建立時間（Unix 奈秒，UTC）。
    #[serde(rename = "created")]
    pub created_ns: i64,
    #[serde(rename = "kdf")]
    pub kdf: KdfParams,
    /// KEK 包住的 master key ＋ 不變式：
    /// `nonce(24) ‖ AEAD密文(master(32) ‖ Invariants CBOR) ‖ tag(16)`。
    #[serde(rename = "wrapped", with = "serde_bytes")]
    pub wrapped: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KdfParams {
    /// 目前只有 [`KDF_ARGON2ID`]。
    #[serde(rename = "alg")]
    pub algorithm: String,
    /// 通過次數。
    #[serde(rename = "t")]
    pub t_cost: u32,
    /// 記憶體（KiB）。
    #[serde(rename = "m")]
    pub m_cost_kib: u32,
    /// 平行度。
    #[serde(rename = "p")]
    pub p_cost: u32,
    #[serde(rename = "salt", with = "serde_bytes")]
    pub salt: Vec<u8>,
}

impl KdfParams {
    /// 預設：Argon2id 64 MiB / t=3 / p=4（RFC 9106 第二組建議）。
    pub fn default_params() -> Self {
        Self {
            algorithm: KDF_ARGON2ID.to_owned(),
            t_cost: 3,
            m_cost_kib: 64 * 1024,
            p_cost: 4,
            salt: Vec::new(), // init 時填入
        }
    }

    /// 讀取端的 DoS 防護：明文參數不可信，先擋掉荒謬值。
    pub fn validate(&self) -> Result<()> {
        if self.algorithm != KDF_ARGON2ID {
            return Err(FormatError::InvalidParams(format!(
                "unsupported kdf {}",
                self.algorithm
            )));
        }
        if self.salt.len() != 16 {
            return Err(FormatError::InvalidParams(format!(
                "kdf salt must be 16 bytes, got {}",
                self.salt.len()
            )));
        }
        if self.m_cost_kib == 0 || self.m_cost_kib > 1024 * 1024 {
            return Err(FormatError::InvalidParams(format!(
                "kdf m {} is outside 1..=1 GiB",
                self.m_cost_kib
            )));
        }
        if self.t_cost == 0 || self.t_cost > 64 {
            return Err(FormatError::InvalidParams(format!(
                "kdf t {} is outside 1..=64",
                self.t_cost
            )));
        }
        if self.p_cost == 0 || self.p_cost > 64 {
            return Err(FormatError::InvalidParams(format!(
                "kdf p {} is outside 1..=64",
                self.p_cost
            )));
        }
        Ok(())
    }
}
