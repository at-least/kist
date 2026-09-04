//! 排版解析的邊界測試：截斷、錯 magic、不合法欄位都要回錯，不能 panic。

use kist_format::envelope::{Compression, Envelope, ObjectKind};
use kist_format::snapshot::{format_key_timestamp, parse_key_timestamp};
use kist_format::{cbor, keys, pack, ChunkId, FormatError, ObjectId};
use proptest::prelude::*;

#[test]
fn envelope_rejects_bad_input() {
    let good = Envelope {
        kind: ObjectKind::Index,
        compression: Compression::None,
        nonce: [1; 24],
        ciphertext: vec![2; 16],
    }
    .encode();

    assert!(matches!(
        Envelope::parse(&good[..31]),
        Err(FormatError::Truncated { .. })
    ));
    let mut bad_magic = good.clone();
    bad_magic[0] = b'X';
    assert!(matches!(
        Envelope::parse(&bad_magic),
        Err(FormatError::BadMagic { .. })
    ));
    let mut bad_version = good.clone();
    bad_version[4] = 2;
    assert!(matches!(
        Envelope::parse(&bad_version),
        Err(FormatError::UnsupportedVersion { .. })
    ));
    let mut bad_kind = good.clone();
    bad_kind[5] = 99;
    assert!(matches!(
        Envelope::parse(&bad_kind),
        Err(FormatError::UnknownKind(99))
    ));
    let mut bad_comp = good.clone();
    bad_comp[6] = 7;
    assert!(matches!(
        Envelope::parse(&bad_comp),
        Err(FormatError::UnknownCompression(7))
    ));
    let mut bad_reserved = good;
    bad_reserved[7] = 1;
    assert!(Envelope::parse(&bad_reserved).is_err());
}

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
    fn envelope_parse_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..80)) {
        let _ = Envelope::parse(&bytes);
    }

    #[test]
    fn pack_trailer_bytes_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..80)) {
        let _ = pack::trailer_bytes(&bytes);
    }

    #[test]
    fn envelope_round_trip(ct in proptest::collection::vec(any::<u8>(), 0..200), nonce in any::<[u8; 24]>()) {
        let env = Envelope { kind: ObjectKind::Snapshot, compression: Compression::None, nonce, ciphertext: ct };
        prop_assert_eq!(Envelope::parse(&env.encode()).unwrap(), env);
    }
}
