//! parity sidecar 的測試。golden 向量（`parity-golden.txt`）與 Go
//! `internal/parity` 的 testdata 是同一份：pack 由與 Go
//! `crypto.DeterministicReader` 相同的 BLAKE3 XOF 流（"kist/test/<seed>"）
//! 產生，所以這個檔案同時是「Go 寫、Rust 讀/修」的跨語言向量。

use kist_format::parity::{self, Object, DATA_SHARDS, MAX_PARITY_SHARDS};
use kist_format::{FormatError, ObjectId};
use proptest::prelude::*;

/// 與 Go crypto.DeterministicReader 相同的測試用位元組流。
fn fake_pack(seed: &str, n: usize) -> Vec<u8> {
    let mut hasher = blake3::Hasher::new();
    hasher.update(format!("kist/test/{seed}").as_bytes());
    let mut xof = hasher.finalize_xof();
    let mut out = vec![0u8; n];
    xof.fill(&mut out);
    out
}

fn encode(pack: &[u8], m: usize) -> (ObjectId, Vec<u8>) {
    let id = ObjectId::of(pack);
    let raw = parity::encode(&id, pack, m).unwrap();
    (id, raw)
}

#[test]
fn golden_go_parity_repairs_rust_side() {
    let text = std::fs::read_to_string("tests/testdata/parity-golden.txt").unwrap();
    let mut lines = text.lines();
    let id = ObjectId::from_hex(lines.next().unwrap().strip_prefix("pack ").unwrap()).unwrap();
    let raw = hex::decode(lines.next().unwrap().strip_prefix("parity ").unwrap()).unwrap();

    let pack = fake_pack("golden", 4321);
    assert_eq!(ObjectId::of(&pack), id, "pack stream diverged from Go");

    let obj = parity::parse(&raw).unwrap();
    assert_eq!(obj.version(), 2);
    assert_eq!(obj.parity_shards(), 2);
    assert_eq!(obj.pack_size(), 4321);

    // 同一個 pack、同一個 m，Rust 必須編出與 Go 相同的 bytes（矩陣相容 +
    // CBOR 欄位順序一致的共同證明）。
    let ours = parity::encode(&id, &pack, 2).unwrap();
    assert_eq!(ours, raw, "Rust encode differs from Go golden");

    let mut bad = pack.clone();
    bad[100] ^= 0x5a;
    bad[4000] ^= 0x5a;
    assert_eq!(obj.repair(&id, &bad).unwrap(), pack);
    assert_eq!(obj.repair(&id, &pack).unwrap(), pack);
}

#[test]
fn encode_is_deterministic_and_validates_inputs() {
    let pack = fake_pack("pack", 1000);
    let (id, raw) = encode(&pack, 2);
    assert_eq!(encode(&pack, 2).1, raw, "encoding is not deterministic");

    let obj = parity::parse(&raw).unwrap();
    assert_eq!(obj.parity_shards(), 2);
    assert_eq!(obj.pack_size(), 1000);
    assert_eq!(obj.shard_len(), 63);

    let wrong = ObjectId::from_hex(&"1".repeat(64)).unwrap();
    assert!(matches!(
        parity::encode(&wrong, &pack, 2),
        Err(FormatError::ParityCorrupt(_))
    ));
    for m in [0, MAX_PARITY_SHARDS + 1] {
        assert!(parity::encode(&id, &pack, m).is_err(), "accepted m={m}");
    }
    assert!(parity::encode(&id, &[], 2).is_err(), "accepted empty pack");
}

/// shard 邊界附近的尺寸。178 是 Go 端曾經出錯的那個（下界寫成
/// shard_len*15 而不是 ceil(size/16)）。
#[test]
fn roundtrip_at_awkward_sizes() {
    for n in [
        1, 15, 16, 17, 178, 179, 180, 191, 192, 193, 255, 256, 4095, 4097,
    ] {
        let pack = fake_pack(&format!("size{n}"), n);
        let (id, raw) = encode(&pack, 2);
        let obj = parity::parse(&raw).unwrap();
        let mut bad = pack.clone();
        bad[n / 2] ^= 1;
        assert_eq!(obj.repair(&id, &bad).unwrap(), pack, "{n} bytes: repair");
    }
}

#[test]
fn repairs_up_to_m_shards() {
    let pack = fake_pack("repair", 100_000);
    let (id, raw) = encode(&pack, 2);
    let obj = parity::parse(&raw).unwrap();
    let shard_len = obj.shard_len() as usize;

    let damage = |offsets: &[usize]| {
        let mut out = pack.clone();
        for o in offsets {
            out[*o] ^= 0x5a;
        }
        out
    };
    let cases: Vec<(&str, Vec<usize>)> = vec![
        ("one byte in the body", vec![shard_len * 3]),
        (
            "two flips in one shard",
            vec![shard_len * 5 + 1, shard_len * 5 + 40],
        ),
        (
            "two shards, one in the last (trailer)",
            vec![shard_len * 2, pack.len() - 3],
        ),
        ("the very first and last bytes", vec![0, pack.len() - 1]),
    ];
    for (name, offsets) in cases {
        assert_eq!(obj.repair(&id, &damage(&offsets)).unwrap(), pack, "{name}");
    }
    // 過短/過長的 pack：差異視同損壞 shard。
    assert_eq!(
        obj.repair(&id, &pack[..pack.len() - shard_len / 2])
            .unwrap(),
        pack
    );
    let mut overlong = pack.clone();
    overlong.extend_from_slice(&[1, 2, 3]);
    assert_eq!(obj.repair(&id, &overlong).unwrap(), pack);
    assert_eq!(obj.repair(&id, &pack).unwrap(), pack);
}

#[test]
fn refuses_more_than_m_erasures() {
    let pack = fake_pack("toomuch", 50_000);
    let (id, raw) = encode(&pack, 2);
    let obj = parity::parse(&raw).unwrap();
    let shard_len = obj.shard_len() as usize;
    let mut bad = pack.clone();
    for o in [0, shard_len, 2 * shard_len] {
        bad[o] ^= 0x5a;
    }
    assert!(matches!(
        obj.repair(&id, &bad),
        Err(FormatError::Unrepairable(_))
    ));
}

/// parity 是合法的但不屬於這個 pack——偽造或拿錯——不能產生「成功」的修復。
#[test]
fn forged_parity_cannot_repair_wrongly() {
    let pack = fake_pack("real", 20_000);
    let id = ObjectId::of(&pack);
    let other = fake_pack("other", 20_000);
    let (_, forged_raw) = encode(&other, 2);
    let forged = parity::parse(&forged_raw).unwrap();

    let mut bad = pack.clone();
    bad[5] ^= 0x5a;
    assert!(matches!(
        forged.repair(&id, &bad),
        Err(FormatError::Unrepairable(_))
    ));

    // hash 恰好都符合受損 shard 的偽造 parity：每片都「驗過」，只剩名字檢查。
    let (_, real_raw) = encode(&pack, 2);
    let mut lying: Object = parity::parse(&real_raw).unwrap();
    let shard_len = lying.shard_len() as usize;
    let first = bad[..shard_len].to_vec();
    lying.hashes[0] = ObjectId::of(&first);
    assert!(matches!(
        lying.repair(&id, &bad),
        Err(FormatError::Unrepairable(_))
    ));
}

#[test]
fn parse_rejects_inconsistent_headers() {
    use serde::Serialize;

    #[derive(Serialize)]
    struct Wire {
        v: u32,
        k: u8,
        m: u8,
        pack_size: u64,
        shard_len: u32,
        hashes: Vec<ObjectId>,
        parity: Vec<serde_bytes::ByteBuf>,
    }

    let pack = fake_pack("hdr", 3000);
    let (id, raw) = encode(&pack, 1);
    let obj = parity::parse(&raw).unwrap();
    assert_eq!(obj.pack_size(), 3000);

    let mutate = |f: &dyn Fn(&mut Wire)| {
        let shard_len = obj.shard_len();
        let mut w = Wire {
            v: 2,
            k: DATA_SHARDS as u8,
            m: 1,
            pack_size: pack.len() as u64,
            shard_len,
            hashes: (0..DATA_SHARDS + 1)
                .map(|i| ObjectId::of(&pack[i % pack.len()..]))
                .collect(),
            parity: vec![serde_bytes::ByteBuf::from(vec![0u8; shard_len as usize])],
        };
        f(&mut w);
        kist_format::cbor::encode(&w).unwrap()
    };
    let cases: Vec<(&str, Vec<u8>)> = vec![
        ("version", mutate(&|w| w.v = 3)),
        ("k", mutate(&|w| w.k = 8)),
        ("m zero", mutate(&|w| w.m = 0)),
        ("m too big", mutate(&|w| w.m = 9)),
        ("shard len zero", mutate(&|w| w.shard_len = 0)),
        ("shard len huge", mutate(&|w| w.shard_len = 1 << 30)),
        ("pack size small", mutate(&|w| w.pack_size = 1)),
        ("pack size large", mutate(&|w| w.pack_size = 1 << 40)),
        ("hash count", mutate(&|w| w.hashes.truncate(3))),
        ("parity count", mutate(&|w| w.parity.clear())),
        ("parity shard len", mutate(&|w| w.parity[0].truncate(5))),
    ];
    for (name, raw) in cases {
        assert!(
            matches!(parity::parse(&raw), Err(FormatError::ParityCorrupt(_))),
            "{name}: parse accepted an inconsistent header"
        );
    }
    for garbage in [&b"not cbor"[..], &[], &[0xa1, 0x61, 0x7a, 0x01][..]] {
        assert!(parity::parse(garbage).is_err());
    }
    assert_eq!(parity::key(&id), format!("parity/{id}"));
}

proptest! {
    /// 任意尺寸與 m 的組合：encode → parse → 修 ≤m 片 → 原封不動；
    /// 超過 m 片一定安全失敗。
    #[test]
    fn roundtrip_and_bound(n in 1usize..5000, m in 1usize..=MAX_PARITY_SHARDS, damage_count in 0usize..=MAX_PARITY_SHARDS + 2) {
        prop_assume!(n >= 1);
        let pack = fake_pack("prop", n);
        let (id, raw) = encode(&pack, m);
        let obj = parity::parse(&raw).unwrap();
        let shard_len = obj.shard_len() as usize;

        let mut bad = pack.clone();
        // 每個受損 shard 翻一個 byte（同一 shard 翻兩次仍是 1 片 erasure）。
        for i in 0..damage_count {
            let pos = i * shard_len;
            if pos >= n {
                break;
            }
            bad[pos] ^= 0x5a;
        }
        let damaged_shards = damage_count.min(n.div_ceil(shard_len));
        if damaged_shards <= m {
            prop_assert_eq!(obj.repair(&id, &bad).unwrap(), pack);
        } else {
            prop_assert!(matches!(obj.repair(&id, &bad), Err(FormatError::Unrepairable(_))));
        }
    }
}
