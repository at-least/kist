//! v4 的 golden 向量（`tests/golden-v4/`）：每種結構一份固定範例，序列化結果必須逐 byte 相同。
//!
//! config 與槽的向量不是這份程式碼錄的：它們由一支獨立的 Go 程式（手寫的最短 CBOR 編碼、
//! lukechampine.com/blake3、x/crypto 的 argon2 與 chacha20poly1305）產生，見 ADR 026 實作第二段。
//! 只有之後新增、由這份程式碼錄的向量才用 `UPDATE_GOLDEN=1` 更新，而且要 review diff。

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;

use kist_format::v4::config::{Chunker, Config, Kdf, KdfSalt, Slot};
use kist_format::v4::ids::RepoId;

/// `2026-09-04T12:00:00Z` 的 Unix 奈秒。
const CREATED_NS: i64 = 1_788_523_200_000_000_000;

fn golden(name: &str) -> Vec<u8> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden-v4")
        .join(name);
    std::fs::read(&path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()))
}

fn check(name: &str, actual: &[u8]) {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden-v4")
        .join(name);
    if std::env::var_os("UPDATE_GOLDEN").as_deref() == Some(std::ffi::OsStr::new("1")) {
        std::fs::write(&path, actual).unwrap();
        return;
    }
    let expected = golden(name);
    assert!(
        expected == actual,
        "{name} differs from the golden file\n expected: {}\n   actual: {}",
        hex::encode(&expected),
        hex::encode(actual)
    );
}

/// 輸入：repo_id 0xab×16、created 2026-09-04T12:00:00Z、預設 chunker（512 KiB／2 MiB／8 MiB）、
/// Argon2id t=3 m=64 MiB p=4、salt 0x11×16。
pub fn sample_config() -> Config {
    Config::new(
        RepoId::from_bytes([0xab; 16]),
        CREATED_NS,
        Chunker::default(),
        Kdf::recommended(KdfSalt::from_bytes([0x11; 16])),
    )
}

#[test]
fn config_matches_the_independent_vector() {
    let c = sample_config();
    check("config.cbor", &c.encode().unwrap());
    assert_eq!(Config::decode(&golden("config.cbor")).unwrap(), c);
}

#[test]
fn slot_matches_the_independent_vector() {
    let bytes = golden("slot.cbor");
    let slot = Slot::decode(&bytes).unwrap();
    check(
        "slot.cbor",
        &Slot::new(slot.wrapped.clone()).encode().unwrap(),
    );
}

/// 兩項的 mpack trailer：{v:4, kind:1, entries:[{i:0x01×32, o:8, l:41, r:10}, {i:0x02×32, o:49, l:50, r:100}]}。
fn two_entry_trailer() -> kist_format::v4::pack::PackTrailer {
    use kist_format::v4::ids::ItemId;
    use kist_format::v4::pack::{Kind, PackEntry, PackTrailer};
    PackTrailer::new(
        Kind::Meta,
        vec![
            PackEntry {
                i: ItemId::from_bytes([1; 32]),
                o: 8,
                l: 41,
                r: 10,
            },
            PackEntry {
                i: ItemId::from_bytes([2; 32]),
                o: 49,
                l: 50,
                r: 100,
            },
        ],
    )
}

#[test]
fn pack_trailer_matches_the_golden_file() {
    let t = two_entry_trailer();
    check(
        "pack_trailer.cbor",
        &t.encode(99, kist_format::v4::pack::MAX_META_ITEM).unwrap(),
    );
}

/// 排版向量：magic、兩項（0x01×41、0x02×50）、trailer（這裡放未密封的 trailer CBOR，只為了排版）、長度、
/// magic。
#[test]
fn pack_layout_matches_the_golden_file() {
    use kist_format::v4::pack;
    let trailer = two_entry_trailer().encode(99, pack::MAX_META_ITEM).unwrap();
    let mut p = pack::begin();
    p.extend_from_slice(&[0x01; 41]);
    p.extend_from_slice(&[0x02; 50]);
    let p = pack::finish(p, &trailer);
    check("pack_layout.bin", &p);
    let (start, sealed) = pack::split_trailer(&p).unwrap();
    assert_eq!((start, sealed), (99, &trailer[..]));
}
