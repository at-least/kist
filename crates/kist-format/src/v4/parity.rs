//! §13：parity。排版與 v3 相同（[`crate::parity`]：明文 CBOR `{v, k, m, pack_size, shard_len, hashes,
//! parity}`，k=16 個資料片、m 1..=8 片 Reed-Solomon 同位，對整個寫進 repo 的 pack bytes 計算），`v`
//! 是 4。key 是 `parity/<c>/<hex>`，與它對應的 pack 或 mpack 同名（[`super::keys::Key::Parity`]）。
//!
//! 明文的理由與 v3 相同：同位是密文的線性組合，偽造或損壞的 parity 最多讓修復失敗，修復結果以重算
//! BLAKE3 等於 pack 的名稱證明（[`Object::repair`]）。

pub use crate::parity::{Object, DATA_SHARDS, MAX_PARITY_SHARDS};

use super::ids::ObjectId;
use super::{Result, VERSION};

/// 對一個完整的 pack 或 mpack（BLAKE3 必須等於 `pack_id`）算出 m 片同位的 bytes。確定性。
pub fn encode(pack_id: &ObjectId, pack: &[u8], m: usize) -> Result<Vec<u8>> {
    Ok(crate::parity::encode_version(pack_id, pack, m, VERSION)?)
}

/// 讀取端：`v` 不是 4、header 自相矛盾或超過上限都拒絕（在依 header 配置記憶體之前）。
pub fn parse(data: &[u8]) -> Result<Object> {
    Ok(crate::parity::parse_version(data, VERSION)?)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn v3_parity_is_refused_and_v4_round_trips() {
        let pack = vec![7u8; 1000];
        let id = ObjectId::of(&pack);
        let v4 = encode(&id, &pack, 2).unwrap();
        let obj = parse(&v4).unwrap();
        assert_eq!(obj.v, 4);
        assert_eq!(obj.repair(&id, &pack).unwrap(), pack);
        let v3 = crate::parity::encode_version(&id, &pack, 2, 3).unwrap();
        assert!(
            parse(&v3).is_err(),
            "a v3 parity object must not pass as v4"
        );
        assert!(crate::parity::parse_version(&v4, 3).is_err());
    }
}
