//! TEMPORARY PoC for v2 format unification. Delete after the experiment.
//!
//! Proves two things:
//! 1. Plain ciborium output (declaration order) differs from Go's Core
//!    Deterministic encoding of the same logical struct.
//! 2. Canonicalising at the Value level (recursively sort map entries by
//!    encoded key bytes) makes the bytes identical to Go's output.

use ciborium::value::Value;
use serde::{Deserialize, Serialize};

const GO_HEX: &str = "a5616da3636269671b0000010000000000646d6f64651981a4656d74696d651b18d2673ac3468cc3616e65612e7478746176026178a3617a41216c757365722e636f6d6d656e744268697073656375726974792e73656c696e757842780063696473825820000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f5820fffefdfcfbfaf9f8f7f6f5f4f3f2f1f0efeeedecebeae9e8e7e6e5e4e3e2e1e0";

#[derive(Serialize, Deserialize)]
struct Sample {
    // Declaration order deliberately NOT the canonical sorted order.
    #[serde(rename = "note", skip_serializing_if = "Option::is_none")]
    note: Option<String>,
    #[serde(rename = "ids", skip_serializing_if = "Option::is_none")]
    ids: Option<Vec<serde_bytes::ByteBuf>>,
    #[serde(rename = "x", skip_serializing_if = "Option::is_none")]
    x: Option<std::collections::BTreeMap<String, serde_bytes::ByteBuf>>,
    #[serde(rename = "m")]
    m: Meta,
    #[serde(rename = "n")]
    n: String,
    #[serde(rename = "v")]
    v: u64,
}

#[derive(Serialize, Deserialize)]
struct Meta {
    #[serde(rename = "mode")]
    mode: u32,
    #[serde(rename = "mtime")]
    mtime: i64,
    #[serde(rename = "big")]
    big: u64,
}

fn sample() -> Sample {
    let mut id1 = vec![0u8; 32];
    for (i, b) in id1.iter_mut().enumerate() {
        *b = i as u8;
    }
    let mut id2 = vec![0u8; 32];
    for (i, b) in id2.iter_mut().enumerate() {
        *b = 255 - i as u8;
    }
    Sample {
        note: None,
        ids: Some(vec![id1, id2].into_iter().map(serde_bytes::ByteBuf::from).collect()),
        // Order deliberately different from canonical to exercise sorting.
        x: Some(
            [
                ("user.comment", &b"hi"[..]),
                ("z", &b"!"[..]),
                ("security.selinux", &b"x\x00"[..]),
            ]
            .into_iter()
            .map(|(k, v)| (k.to_string(), serde_bytes::ByteBuf::from(v.to_vec())))
            .collect(),
        ),
        m: Meta { mode: 0o100644, mtime: 1788605504101452995, big: 1 << 40 },
        n: "a.txt".to_string(),
        v: 2,
    }
}

/// RFC 8949 §4.2.1 canonicalisation: recursively sort every map's entries
/// by the bytewise order of the encoded key.
fn canonical(v: Value) -> Value {
    match v {
        Value::Map(entries) => {
            let mut keyed: Vec<(Vec<u8>, Value, Value)> = entries
                .into_iter()
                .map(|(k, val)| {
                    let mut enc = Vec::new();
                    ciborium::into_writer(&k, &mut enc).unwrap();
                    (enc, k, canonical(val))
                })
                .collect();
            keyed.sort_by(|a, b| a.0.cmp(&b.0));
            Value::Map(keyed.into_iter().map(|(_, k, val)| (k, val)).collect())
        }
        Value::Array(items) => Value::Array(items.into_iter().map(canonical).collect()),
        other => other,
    }
}

#[test]
fn poc_cbor_matches_go_core_deterministic() {
    let s = sample();

    let mut plain = Vec::new();
    ciborium::into_writer(&s, &mut plain).unwrap();
    let plain_hex = hex::encode(&plain);
    println!("POC_RUST_PLAIN   {plain_hex}");
    println!("POC_GO_CANONICAL {GO_HEX}");
    assert_ne!(plain_hex, GO_HEX, "plain ciborium output already matches Go?!");

    let mut value_buf = Vec::new();
    ciborium::into_writer(&s, &mut value_buf).unwrap();
    let value: Value = ciborium::from_reader(std::io::Cursor::new(&value_buf)).unwrap();
    let canon = canonical(value);
    let mut canon_bytes = Vec::new();
    ciborium::into_writer(&canon, &mut canon_bytes).unwrap();
    let canon_hex = hex::encode(&canon_bytes);
    println!("POC_RUST_CANON   {canon_hex}");

    // Round-trip back into the struct to prove decode still works.
    let back: Sample = ciborium::from_reader(std::io::Cursor::new(&canon_bytes)).unwrap();
    let mut back_bytes = Vec::new();
    ciborium::into_writer(&back, &mut back_bytes).unwrap();
    assert_eq!(hex::encode(&back_bytes), plain_hex, "round-trip not stable");

    assert_eq!(canon_hex, GO_HEX, "canonicalised encoding differs from Go");
}
