//! 排版解析的邊界測試：截斷、錯 magic、不合法欄位都要回錯，不能 panic。

use kist_format::snapshot::{format_key_timestamp, parse_key_timestamp};
use kist_format::{cbor, keys, pack, ChunkId, FormatError, ObjectId};
use proptest::prelude::*;

#[test]
fn pack_rejects_bad_input() {
    let bytes = pack::finish(pack::begin(), &[9; 10]);
    assert_eq!(pack::trailer_bytes(&bytes).unwrap(), &[9; 10]);

    assert!(matches!(
        pack::trailer_bytes(&bytes[..10]),
        Err(FormatError::Truncated { .. })
    ));
    let mut bad_head = bytes.clone();
    bad_head[0] = b'x';
    assert!(matches!(
        pack::trailer_bytes(&bad_head),
        Err(FormatError::BadMagic { .. })
    ));
    let mut bad_tail = bytes.clone();
    let last = bad_tail.len() - 1;
    bad_tail[last] = b'x';
    assert!(matches!(
        pack::trailer_bytes(&bad_tail),
        Err(FormatError::BadMagic { .. })
    ));
    // trailer 長度宣稱得比檔案還大
    let mut bad_len = bytes.clone();
    let len_pos = bytes.len() - 16;
    bad_len[len_pos..len_pos + 8].copy_from_slice(&u64::MAX.to_le_bytes());
    assert!(matches!(
        pack::trailer_bytes(&bad_len),
        Err(FormatError::Truncated { .. })
    ));
    // trailer 長度宣稱到 header 裡面去
    let mut overlap = bytes;
    overlap[len_pos..len_pos + 8].copy_from_slice(&11u64.to_le_bytes());
    assert!(pack::trailer_bytes(&overlap).is_err());
}

/// v2 沒有 envelope header：magic（含版號）在 pack 的檔頭檔尾各一份。
/// 錯的 magic 前綴、不支援的版號、被改過的檔尾都必須被拒絕。
#[test]
fn pack_rejects_bad_magic_and_unsupported_version() {
    let good = pack::finish(pack::begin(), &[9; 10]);
    assert_eq!(pack::trailer_bytes(&good).unwrap(), &[9; 10]);

    // 檔頭 magic 前綴（"kistpk"）被改
    let mut bad_prefix = good.clone();
    bad_prefix[0] = b'X';
    assert!(matches!(
        pack::trailer_bytes(&bad_prefix),
        Err(FormatError::BadMagic { what: "pack header" })
    ));
    // 檔頭版號不支援（v3）：magic 含版號，整段不符就是 BadMagic
    let mut bad_version = good.clone();
    bad_version[6..8].copy_from_slice(&3u16.to_be_bytes());
    assert!(matches!(
        pack::trailer_bytes(&bad_version),
        Err(FormatError::BadMagic { what: "pack header" })
    ));
    // 檔尾 magic 被改（前綴與版號各試一次）
    let mut bad_footer = good.clone();
    let n = bad_footer.len();
    bad_footer[n - 1] = b'X';
    assert!(matches!(
        pack::trailer_bytes(&bad_footer),
        Err(FormatError::BadMagic { what: "pack footer" })
    ));
    let mut bad_footer_version = good;
    let n = bad_footer_version.len();
    bad_footer_version[n - 2..].copy_from_slice(&1u16.to_be_bytes());
    assert!(matches!(
        pack::trailer_bytes(&bad_footer_version),
        Err(FormatError::BadMagic { what: "pack footer" })
    ));
    // 檔頭檔尾版號不一致：只有一邊對 → 另一邊擋下
    let mut mismatch = pack::begin();
    mismatch.extend_from_slice(&[9; 10]);
    let mut forged = pack::finish(mismatch, &[9; 10]);
    let n = forged.len();
    forged[n - 2..].copy_from_slice(&(kist_format::FORMAT_VERSION as u16).to_be_bytes());
    forged[6..8].copy_from_slice(&(kist_format::FORMAT_VERSION as u16 + 1).to_be_bytes());
    assert!(matches!(
        pack::trailer_bytes(&forged),
        Err(FormatError::BadMagic { .. })
    ));
}

/// footer 本體（u64 BE 長度 + magic）的解析邊界。
#[test]
fn pack_footer_parsing() {
    let good = pack::finish(pack::begin(), &[9; 10]);
    assert_eq!(pack::parse_footer(&good).unwrap(), 10);
    assert!(matches!(
        pack::parse_footer(&good[..15]),
        Err(FormatError::Truncated { .. })
    ));
    // magic（footer 的後 8 bytes）任何一個 bit 被改都必須被拒絕
    let n = good.len();
    for pos in n - 8..n {
        let mut bad = good.clone();
        bad[pos] ^= 1;
        assert!(pack::parse_footer(&bad).is_err(), "pos {pos}");
    }
    // 長度欄位本身 parse_footer 不判斷（它無從知道對錯），但 trailer_bytes 會擋下
    let mut huge = good.clone();
    huge[n - 16] ^= 1; // 長度的最高 byte：瞬間變成天文數字
    assert!(pack::parse_footer(&huge).is_ok());
    assert!(matches!(
        pack::trailer_bytes(&huge),
        Err(FormatError::Truncated { .. })
    ));
}

#[test]
fn ids_hex_round_trip() {
    let id = ObjectId::of(b"hello");
    assert_eq!(ObjectId::from_hex(&id.to_hex()).unwrap(), id);
    assert_eq!(id.to_hex().len(), 64);
    assert!(ObjectId::from_hex("zz").is_err());
    assert!(ObjectId::from_hex("00").is_err());
    assert_eq!(
        keys::object_id_from_key(&keys::pack(&id)).unwrap(),
        id,
        "key → id 要能還原"
    );
    assert_eq!(keys::pack(&id), format!("packs/{}", id.to_hex()));
    assert_eq!(
        keys::snapshot(&[0xab, 0xcd], "20260904T000000000000000Z"),
        "snapshots/abcd/20260904T000000000000000Z"
    );
}

#[test]
fn ids_serialize_as_byte_string() {
    // 32 bytes 的 byte string 在 CBOR 是 2 bytes 的 head + 32 bytes，不是 33 個整數。
    let bytes = cbor::encode(&ChunkId::from_bytes([7; 32])).unwrap();
    assert_eq!(bytes.len(), 34);
    assert_eq!(bytes[0], 0x58);
    assert_eq!(bytes[1], 32);
}

#[test]
fn cbor_rejects_trailing_bytes() {
    let mut bytes = cbor::encode(&1u32).unwrap();
    bytes.push(0);
    assert!(matches!(
        cbor::decode::<u32>(&bytes),
        Err(FormatError::Decode(_))
    ));
}

#[test]
fn key_timestamp_round_trip_and_ordering() {
    let t = time::macros::datetime!(2026-09-04 14:39:44.946220116 UTC);
    let s = format_key_timestamp(t).unwrap();
    assert_eq!(s, "20260904T143944946220116Z");
    assert_eq!(parse_key_timestamp(&s).unwrap(), t);
    assert!(!s.contains(':'), "Windows 檔名不能有冒號");
    let later = format_key_timestamp(t + time::Duration::nanoseconds(1)).unwrap();
    assert!(later > s, "字典序必須等於時間序");
    assert!(parse_key_timestamp("2026-09-04T14:39:44Z").is_err());
}

proptest! {
    #[test]
    fn pack_trailer_bytes_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..80)) {
        let _ = pack::trailer_bytes(&bytes);
    }

    #[test]
    fn pack_parse_footer_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..80)) {
        let _ = pack::parse_footer(&bytes);
    }

    #[test]
    fn pack_round_trip(trailer in proptest::collection::vec(any::<u8>(), 0..200)) {
        let mut buf = pack::begin();
        buf.extend_from_slice(&[0xAB; 40]);
        let bytes = pack::finish(buf, &trailer);
        prop_assert_eq!(pack::trailer_bytes(&bytes).unwrap(), &trailer[..]);
    }
}

/// `--json` 用：人類可讀的序列化是 hex 字串；CBOR 仍是 bytes（golden 測試另外守）。
#[test]
fn ids_serialize_as_hex_in_json_and_bytes_in_cbor() {
    let id = kist_format::ObjectId::from_bytes([0xab; 32]);
    let json = serde_json::to_string(&id).unwrap();
    assert_eq!(json, format!("\"{}\"", "ab".repeat(32)));
    let back: kist_format::ObjectId = serde_json::from_str(&json).unwrap();
    assert_eq!(back, id);
    let cbor = kist_format::cbor::encode(&id).unwrap();
    assert_eq!(cbor[0], 0x58, "CBOR 應該是 bytes(32)，不是字串"); // 0x58 = bytes, 1-byte length
    assert_eq!(cbor[1], 32);
    let back: kist_format::ObjectId = kist_format::cbor::decode(&cbor).unwrap();
    assert_eq!(back, id);
}
