//! parity 的測試（`docs/format-v4.md` §13）：編碼的確定性與輸入驗證、各種尺寸的修復、m 片的上限、偽造的
//! parity 修不出「成功」、header 自相矛盾的拒絕，與 proptest。凍結的向量（v3 錄的 `parity-golden.txt`
//! 只差 `v` 一個 byte，與 `golden-v4/parity.cbor`）在 `golden_v4.rs` 比對。

use kist_format::v4::parity::{self, Object, DATA_SHARDS, MAX_PARITY_SHARDS};
use kist_format::v4::{Error as V4Error, VERSION};
use kist_format::{FormatError, ObjectId};
use proptest::prelude::*;

/// 測試用的確定性位元組流（輸入 "kist/test/<seed>" 的 BLAKE3 XOF）；
/// golden 向量的 pack 由它產生，改了就對不上向量。
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
fn encode_is_deterministic_and_validates_inputs() {
    let pack = fake_pack("pack", 1000);
    let (id, raw) = encode(&pack, 2);
    assert_eq!(encode(&pack, 2).1, raw, "encoding is not deterministic");

    let obj = parity::parse(&raw).unwrap();
    assert_eq!(obj.v, VERSION);
    assert_eq!(obj.parity_shards(), 2);
    assert_eq!(obj.pack_size, 1000);
    assert_eq!(obj.shard_len, 63);

    let wrong = ObjectId::from_hex(&"1".repeat(64)).unwrap();
    assert!(matches!(
        parity::encode(&wrong, &pack, 2),
        Err(V4Error::Format(FormatError::ParityCorrupt(_)))
    ));
    for m in [0, MAX_PARITY_SHARDS + 1] {
        assert!(parity::encode(&id, &pack, m).is_err(), "accepted m={m}");
    }
    assert!(parity::encode(&id, &[], 2).is_err(), "accepted empty pack");
}

/// shard 邊界附近的尺寸。178 是曾經出錯的那個（下界寫成
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
    let shard_len = obj.shard_len as usize;

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
    let shard_len = obj.shard_len as usize;
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
    let shard_len = lying.shard_len as usize;
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
    assert_eq!(obj.pack_size, 3000);

    let mutate = |f: &dyn Fn(&mut Wire)| {
        let shard_len = obj.shard_len;
        let mut w = Wire {
            v: VERSION,
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
        ("version", mutate(&|w| w.v = VERSION + 1)),
        ("v3", mutate(&|w| w.v = 3)),
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
            matches!(
                parity::parse(&raw),
                Err(V4Error::Format(FormatError::ParityCorrupt(_)))
            ),
            "{name}: parse accepted an inconsistent header"
        );
    }
    for garbage in [&b"not cbor"[..], &[], &[0xa1, 0x61, 0x7a, 0x01][..]] {
        assert!(parity::parse(garbage).is_err());
    }
    let _ = id;
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
        let shard_len = obj.shard_len as usize;

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
