//! v4 的識別碼（§2）。
//!
//! - [`ChunkId`]：keyed BLAKE3(`K_chunk_id`, chunk 明文)。與 v3 同一個型別。
//! - [`MetaId`]：keyed BLAKE3(`K_meta_id`, metadata item 的規範 CBOR，壓縮之前)。兩把 key 不同，一個 ID
//!   不會同時是資料與 metadata。
//! - [`ObjectId`]：pack、mpack、delta、map 分片、parity 的名稱＝BLAKE3(寫進 repo 的 bytes)。與 v3 同一個
//!   型別。
//! - [`ClientId`]：16 bytes；全零是維護角色（prune 的 repack 輸出）。
//!
//! CBOR 裡一律是 byte string，路徑裡一律是小寫 hex。

pub use crate::ids::{ChunkId, ObjectId};

use super::{corrupt, Result};

/// 長度固定為 `N` bytes 的小寫 hex。大寫、長度不對或不是 hex 都拒絕：路徑裡的名稱只有一種寫法。
pub fn parse_lower_hex<const N: usize>(what: &'static str, s: &str) -> Result<[u8; N]> {
    let ok = s.len() == 2 * N
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    if !ok {
        return Err(corrupt(
            what,
            format!("{s:?} is not {} lowercase hex digits", 2 * N),
        ));
    }
    let mut out = [0u8; N];
    hex::decode_to_slice(s, &mut out).map_err(|e| corrupt(what, e.to_string()))?;
    Ok(out)
}

macro_rules! id_type {
    ($name:ident, $len:literal, $doc:literal) => {
        #[doc = $doc]
        #[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
        pub struct $name([u8; $len]);

        impl $name {
            pub const fn from_bytes(bytes: [u8; $len]) -> Self {
                Self(bytes)
            }

            pub fn as_bytes(&self) -> &[u8; $len] {
                &self.0
            }

            pub fn to_hex(&self) -> String {
                hex::encode(self.0)
            }

            /// 只接受小寫 hex（[`parse_lower_hex`]）。
            pub fn from_hex(s: &str) -> $crate::v4::Result<Self> {
                $crate::v4::ids::parse_lower_hex::<$len>(stringify!($name), s).map(Self)
            }
        }

        impl ::std::fmt::Debug for $name {
            fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                write!(f, "{}({})", stringify!($name), self.to_hex())
            }
        }

        impl ::std::fmt::Display for $name {
            fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                f.write_str(&self.to_hex())
            }
        }

        /// CBOR（repo 格式）是 byte string；人類可讀的格式（`--json`）是 hex 字串。
        impl ::serde::Serialize for $name {
            fn serialize<S: ::serde::Serializer>(
                &self,
                s: S,
            ) -> ::std::result::Result<S::Ok, S::Error> {
                if s.is_human_readable() {
                    s.serialize_str(&self.to_hex())
                } else {
                    s.serialize_bytes(&self.0)
                }
            }
        }

        impl<'de> ::serde::Deserialize<'de> for $name {
            fn deserialize<D: ::serde::Deserializer<'de>>(
                d: D,
            ) -> ::std::result::Result<Self, D::Error> {
                if d.is_human_readable() {
                    let text = <String as ::serde::Deserialize>::deserialize(d)?;
                    return Self::from_hex(&text).map_err(serde::de::Error::custom);
                }
                let buf = <::serde_bytes::ByteBuf as ::serde::Deserialize>::deserialize(d)?;
                let arr: [u8; $len] = buf.into_vec().try_into().map_err(|v: Vec<u8>| {
                    serde::de::Error::invalid_length(v.len(), &concat!($len, " bytes"))
                })?;
                Ok(Self(arr))
            }
        }
    };
}

pub(crate) use id_type;

id_type!(
    MetaId,
    32,
    "metadata item 的 ID：keyed BLAKE3(`K_meta_id`, item 的規範 CBOR，壓縮之前)。"
);
id_type!(
    ClientId,
    16,
    "client id：16 bytes，路徑裡是 32 位小寫 hex。全零是維護角色。"
);

impl ClientId {
    /// 維護角色（prune 的 repack 輸出）的 client id。
    pub const MAINT: Self = Self([0u8; 16]);
}

id_type!(RepoId, 16, "repo id：init 時以 OS 亂數產生的 16 bytes。");

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::cbor;

    #[test]
    fn hex_must_be_lowercase_and_exact_length() {
        let id = MetaId::from_bytes([0xab; 32]);
        assert_eq!(MetaId::from_hex(&id.to_hex()).unwrap(), id);
        assert!(MetaId::from_hex(&id.to_hex().to_uppercase()).is_err());
        assert!(MetaId::from_hex(&id.to_hex()[..62]).is_err());
        assert!(MetaId::from_hex(&format!("{}00", id.to_hex())).is_err());
        assert!(ClientId::from_hex("0123456789abcdef0123456789abcdeg").is_err());
        assert_eq!(
            ClientId::from_hex(&"0".repeat(32)).unwrap(),
            ClientId::MAINT
        );
    }

    #[test]
    fn ids_are_byte_strings_in_cbor() {
        let c = ClientId::from_bytes([7; 16]);
        let bytes = cbor::encode(&c).unwrap();
        assert_eq!(bytes[0], 0x50); // major 2, length 16
        assert_eq!(cbor::decode::<ClientId>(&bytes).unwrap(), c);
        let short = cbor::encode(&serde_bytes::Bytes::new(&[7; 15])).unwrap();
        assert!(cbor::decode::<ClientId>(&short).is_err());
    }
}
