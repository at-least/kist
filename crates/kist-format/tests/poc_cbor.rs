//! CBOR 編碼行為測試（v2 修訂後：欄位表順序，不做排序）。
//!
//! 歷史：v2 草案曾採「map keys 排序」的 Core Deterministic（P1 驗證過
//! ciborium Value 層排序可與 Go fxamacker CoreDet 逐 byte 相同），但那是
//! 「Go 免費、Rust 付 20 倍 encode 成本」的選擇。2026-09-05 修訂為規格釘
//! 死欄位順序；跨語言一致性由 `tests/interop.rs` 的 tree 向量承擔。

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
struct Sample {
    // 宣告順序故意不是排序順序：ids < x < m < n < v 才是排序序。
    #[serde(rename = "n")]
    n: String,
    #[serde(rename = "v")]
    v: u64,
    #[serde(rename = "m")]
    m: Meta,
    #[serde(rename = "ids", skip_serializing_if = "Option::is_none")]
    ids: Option<Vec<serde_bytes::ByteBuf>>,
    #[serde(rename = "x", skip_serializing_if = "Option::is_none")]
    x: Option<std::collections::BTreeMap<String, serde_bytes::ByteBuf>>,
}

#[derive(Serialize, Deserialize)]
struct Meta {
    #[serde(rename = "mode")]
    mode: u32,
    #[serde(rename = "mtime")]
    mtime: i64,
}

#[test]
fn encode_emits_declaration_order_without_sorting() {
    let s = Sample {
        n: "a.txt".to_owned(),
        v: 2,
        m: Meta {
            mode: 0o100644,
            mtime: 1788605504101452995,
        },
        ids: None,
        x: None,
    };
    let enc = kist_format::cbor::encode(&s).unwrap();
    let hex: String = enc.iter().map(|b| format!("{b:02x}")).collect();
    // n, v, m —— 宣告順序，不是排序序（m < n < v）。
    assert_eq!(
        hex,
        "a3616e65612e74787461760261 6da2646d6f64651981a4656d74696d651b18d2673ac3468cc3"
            .replace(' ', "")
    );
}

#[test]
fn decode_rejects_duplicate_struct_fields() {
    // {a: 1, a: 2} 解進 struct —— 重複欄位是偽造或損壞，serde visitor 拒絕。
    #[derive(serde::Deserialize, Debug)]
    struct Dup {
        // 欄位值不被讀——存在只是為了觸發 serde 的重複欄位偵測。
        #[expect(dead_code)]
        a: u64,
    }
    let dup = [0xa2u8, 0x61, 0x61, 0x01, 0x61, 0x61, 0x02];
    let v: Result<Dup, _> = kist_format::cbor::decode(&dup);
    assert!(v.is_err(), "duplicate field must be rejected");
}

#[test]
fn decode_rejects_trailing_bytes() {
    let trailing = [0x01u8, 0x02];
    let v: Result<u64, _> = kist_format::cbor::decode(&trailing);
    assert!(v.is_err(), "trailing bytes must be rejected");
}

#[test]
fn decode_ignores_unknown_fields() {
    // {"known": 1, "future_field": "x"} → 解進只認得 known 的 struct。
    #[derive(Debug, PartialEq, serde::Deserialize)]
    struct Known {
        #[serde(rename = "known")]
        known: u64,
    }
    let bytes = [
        0xa2u8, 0x45, 0x6b, 0x6e, 0x6f, 0x77, 0x6e, 0x01, 0x4c, 0x66, 0x75, 0x74, 0x75, 0x72, 0x65,
        0x5f, 0x66, 0x69, 0x65, 0x6c, 0x64, 0x41, 0x78,
    ];
    let v: Known = kist_format::cbor::decode(&bytes).unwrap();
    assert_eq!(v.known, 1);
}
