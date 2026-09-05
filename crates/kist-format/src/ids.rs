//! 兩種 32-byte 識別碼。
//!
//! - [`ChunkId`]：chunk 的身分 = keyed BLAKE3(hash key, 明文)。只出現在加密過的
//!   trailer / index / tree 裡，**絕不**當作 repo 裡的物件名稱。
//! - [`ObjectId`]：獨立物件（pack / tree / index）的名稱 = 一般 BLAKE3(整個物件的密文 bytes)。
//!   下載端不需要金鑰就能驗證拿到的 bytes 沒被改過。
//!
//! 兩者在 CBOR 裡都以 32-byte 的 byte string 存放（不是 32 個整數的陣列），
//! 在 repo 路徑裡則用小寫 hex。

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::{FormatError, Result};

macro_rules! id_type {
    ($name:ident, $doc:literal) => {
        #[doc = $doc]
        #[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name([u8; 32]);

        impl $name {
            pub const LEN: usize = 32;

            pub const fn from_bytes(bytes: [u8; 32]) -> Self {
                Self(bytes)
            }

            pub fn as_bytes(&self) -> &[u8; 32] {
                &self.0
            }

            pub fn to_hex(&self) -> String {
                hex::encode(self.0)
            }

            pub fn from_hex(s: &str) -> Result<Self> {
                let bytes = hex::decode(s).map_err(|_| FormatError::BadName(s.to_owned()))?;
                let arr: [u8; 32] = bytes
                    .try_into()
                    .map_err(|_| FormatError::BadName(s.to_owned()))?;
                Ok(Self(arr))
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}({})", stringify!($name), self.to_hex())
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.to_hex())
            }
        }

        /// CBOR（repo 格式）是 32 bytes；人類可讀的格式（`--json` 輸出）是 hex 字串。
        /// ciborium 的 `is_human_readable()` 是 false，所以 repo 裡的 bytes 不受影響（golden 測試守著）。
        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
                if s.is_human_readable() {
                    s.serialize_str(&self.to_hex())
                } else {
                    s.serialize_bytes(&self.0)
                }
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
                if d.is_human_readable() {
                    let text = String::deserialize(d)?;
                    return Self::from_hex(&text).map_err(serde::de::Error::custom);
                }
                let buf = serde_bytes::ByteBuf::deserialize(d)?;
                let arr: [u8; 32] = buf
                    .into_vec()
                    .try_into()
                    .map_err(|v: Vec<u8>| serde::de::Error::invalid_length(v.len(), &"32 bytes"))?;
                Ok(Self(arr))
            }
        }
    };
}

id_type!(ChunkId, "chunk 的身分：keyed BLAKE3(hash key, 明文)。");
id_type!(ObjectId, "獨立物件的名稱：BLAKE3(物件的完整密文 bytes)。");

impl ObjectId {
    /// 依格式規則計算一個物件的名稱：對「要寫進 repo 的完整 bytes」做一般 BLAKE3。
    pub fn of(object_bytes: &[u8]) -> Self {
        Self(*blake3::hash(object_bytes).as_bytes())
    }
}
