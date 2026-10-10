//! §3：config 與金鑰槽的結構。金鑰推導（KEK、槽的名稱與金鑰、master 的子金鑰）在 `kist-crypto` 的 `v4`。
//!
//! config 是明文 CBOR，init 時寫一次、之後不變；金鑰槽 `keys/<名稱>` 的 AAD 綁住 config 從 repo 讀到的
//! bytes，所以 config 的任何一個 byte 被換掉，所有槽都開不起來。讀取端驗證：版本、chunker 與 KDF 參數的
//! 上限（明文參數不可信，荒謬的值不真的去跑）、槽的長度。

use serde::{Deserialize, Serialize};

use super::ids::{id_type, RepoId};
use super::{check_version, corrupt, invalid, Result, VERSION};
use crate::cbor;

/// 唯一支援的 KDF。
pub const KDF_ARGON2ID: &str = "argon2id";

/// 槽的 `wrapped`：nonce(24) ‖ AEAD(master 32 bytes) ‖ tag(16)。
pub const SLOT_WRAPPED_LEN: usize = 24 + 32 + 16;

id_type!(
    KdfSalt,
    16,
    "KDF 的 salt：init 時以 OS 亂數產生的 16 bytes。"
);

/// `config`：§4 的欄位順序 v, repo_id, created, chunker, kdf。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    pub v: u32,
    pub repo_id: RepoId,
    /// 建立時間，Unix 奈秒（UTC）。
    pub created: i64,
    pub chunker: Chunker,
    pub kdf: Kdf,
}

impl Config {
    pub fn new(repo_id: RepoId, created: i64, chunker: Chunker, kdf: Kdf) -> Self {
        Self {
            v: VERSION,
            repo_id,
            created,
            chunker,
            kdf,
        }
    }

    /// 寫入端：參數不合法就不寫。
    pub fn encode(&self) -> Result<Vec<u8>> {
        self.validate()
            .map_err(|e| invalid("config", e.to_string()))?;
        Ok(cbor::encode(self)?)
    }

    /// 讀取端：`v` 不是 4、參數越界都拒絕。槽的 AAD 要用這份原始 bytes，不要用重新編碼的結果。
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let c: Config = cbor::decode(bytes)?;
        check_version("config", c.v)?;
        c.validate()?;
        Ok(c)
    }

    fn validate(&self) -> Result<()> {
        check_version("config", self.v)?;
        self.chunker.validate()?;
        self.kdf.validate()
    }
}

/// FastCDC 參數（bytes）：§4 的欄位順序 min, avg, max。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Chunker {
    pub min: u32,
    pub avg: u32,
    pub max: u32,
}

impl Default for Chunker {
    fn default() -> Self {
        Self {
            min: 512 * 1024,
            avg: 2 * 1024 * 1024,
            max: 8 * 1024 * 1024,
        }
    }
}

impl Chunker {
    /// 越界的值會讓掃描越界或配置巨量記憶體（與 v3 相同的上限）。
    pub fn validate(&self) -> Result<()> {
        let bad = |msg: String| Err(corrupt("config.chunker", msg));
        if !(64..=1024 * 1024).contains(&self.min) {
            return bad(format!("min {} is outside 64..=1 MiB", self.min));
        }
        if !(256..=16 * 1024 * 1024).contains(&self.avg) {
            return bad(format!("avg {} is outside 256..=16 MiB", self.avg));
        }
        if !(1024..=64 * 1024 * 1024).contains(&self.max) {
            return bad(format!("max {} is outside 1 KiB..=64 MiB", self.max));
        }
        if !(self.min <= self.avg && self.avg <= self.max) {
            return bad(format!(
                "sizes must satisfy min <= avg <= max, got {}/{}/{}",
                self.min, self.avg, self.max
            ));
        }
        Ok(())
    }
}

/// KDF 參數：§4 的欄位順序 alg, t, m, p, salt。`m` 的單位是 KiB。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Kdf {
    pub alg: String,
    pub t: u32,
    pub m: u32,
    pub p: u32,
    pub salt: KdfSalt,
}

impl Kdf {
    /// init 的預設：Argon2id 64 MiB、t=3、p=4（RFC 9106 的第二組建議，與 v3 相同）。
    pub fn recommended(salt: KdfSalt) -> Self {
        Self {
            alg: KDF_ARGON2ID.to_owned(),
            t: 3,
            m: 64 * 1024,
            p: 4,
            salt,
        }
    }

    /// 讀取端的 DoS 防護：明文參數不可信，先擋掉荒謬值（與 v3 相同的上限）。Argon2 要求 `m ≥ 8p`。
    pub fn validate(&self) -> Result<()> {
        let bad = |msg: String| Err(corrupt("config.kdf", msg));
        if self.alg != KDF_ARGON2ID {
            return bad(format!("unsupported kdf {:?}", self.alg));
        }
        if !(1..=64).contains(&self.t) {
            return bad(format!("t {} is outside 1..=64", self.t));
        }
        if !(1..=64).contains(&self.p) {
            return bad(format!("p {} is outside 1..=64", self.p));
        }
        if self.m < 8 * self.p || self.m > 1024 * 1024 {
            return bad(format!("m {} KiB is outside 8p..=1 GiB", self.m));
        }
        Ok(())
    }
}

/// 金鑰槽 `keys/<名稱>`：§4 的欄位順序 v, wrapped。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Slot {
    pub v: u32,
    #[serde(with = "serde_bytes")]
    pub wrapped: Vec<u8>,
}

impl Slot {
    pub fn new(wrapped: Vec<u8>) -> Self {
        Self {
            v: VERSION,
            wrapped,
        }
    }

    pub fn encode(&self) -> Result<Vec<u8>> {
        self.validate()
            .map_err(|e| invalid("key slot", e.to_string()))?;
        Ok(cbor::encode(self)?)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let s: Slot = cbor::decode(bytes)?;
        s.validate()?;
        Ok(s)
    }

    fn validate(&self) -> Result<()> {
        check_version("key slot", self.v)?;
        if self.wrapped.len() != SLOT_WRAPPED_LEN {
            return Err(corrupt(
                "key slot",
                format!(
                    "wrapped is {} bytes, not {SLOT_WRAPPED_LEN}",
                    self.wrapped.len()
                ),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::v4::Error;

    fn sample() -> Config {
        Config::new(
            RepoId::from_bytes([0xab; 16]),
            1_788_523_200_000_000_000,
            Chunker::default(),
            Kdf::recommended(KdfSalt::from_bytes([0x11; 16])),
        )
    }

    #[test]
    fn config_round_trips_and_keeps_the_field_order() {
        let c = sample();
        let bytes = c.encode().unwrap();
        assert_eq!(Config::decode(&bytes).unwrap(), c);
        // 欄位依 §4 的順序：v, repo_id, created, chunker, kdf。
        let text = String::from_utf8_lossy(&bytes);
        let pos: Vec<usize> = ["repo_id", "created", "chunker", "kdf"]
            .iter()
            .map(|k| text.find(k).unwrap())
            .collect();
        assert!(pos.windows(2).all(|w| w[0] < w[1]), "{pos:?}");
    }

    #[test]
    fn a_version_other_than_4_is_rejected() {
        let mut c = sample();
        c.v = 3;
        let bytes = cbor::encode(&c).unwrap();
        assert!(matches!(
            Config::decode(&bytes),
            Err(Error::Version { version: 3, .. })
        ));
        assert!(matches!(c.encode(), Err(Error::Invalid { .. })));
    }

    #[test]
    fn absurd_parameters_are_rejected_before_use() {
        let mut huge = sample();
        huge.kdf.m = 4 * 1024 * 1024;
        let mut bad_alg = sample();
        bad_alg.kdf.alg = "scrypt".into();
        let mut tiny_m = sample();
        tiny_m.kdf.m = 8 * tiny_m.kdf.p - 1;
        let mut chunk = sample();
        chunk.chunker.avg = chunk.chunker.max + 1;
        for c in [huge, bad_alg, tiny_m, chunk] {
            let bytes = cbor::encode(&c).unwrap();
            assert!(
                matches!(Config::decode(&bytes), Err(Error::Corrupt { .. })),
                "{c:?}"
            );
        }
    }

    #[test]
    fn slots_must_have_the_exact_wrapped_length() {
        let s = Slot::new(vec![7; SLOT_WRAPPED_LEN]);
        assert_eq!(Slot::decode(&s.encode().unwrap()).unwrap(), s);
        for len in [0, SLOT_WRAPPED_LEN - 1, SLOT_WRAPPED_LEN + 1] {
            let bytes = cbor::encode(&Slot::new(vec![7; len])).unwrap();
            assert!(Slot::decode(&bytes).is_err(), "{len}");
        }
    }

    #[test]
    fn unknown_fields_are_ignored_and_missing_fields_rejected() {
        #[derive(Serialize)]
        struct Extra<'a> {
            v: u32,
            #[serde(with = "serde_bytes")]
            wrapped: &'a [u8],
            future: u8,
        }
        let bytes = cbor::encode(&Extra {
            v: 4,
            wrapped: &[7; SLOT_WRAPPED_LEN],
            future: 1,
        })
        .unwrap();
        assert!(Slot::decode(&bytes).is_ok());
        #[derive(Serialize)]
        struct Missing {
            v: u32,
        }
        assert!(Slot::decode(&cbor::encode(&Missing { v: 4 }).unwrap()).is_err());
    }
}
