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

/// DeriveKey("kist/v4/cut", master 0x42×32)，由 scripts/v4-vectors 的 Go 程式算出。
const CUT_KEY: &str = "54ea4d4e979f0304865575bf18fd56070b27c3411cc1017b2f4cb207485d4a8b";

fn is_cut(name: &[u8]) -> bool {
    let mut key = [0u8; 32];
    hex::decode_to_slice(CUT_KEY, &mut key).unwrap();
    kist_format::v4::meta::is_cut_hash(blake3::keyed_hash(&key, name).as_bytes())
}

/// meta ID 不在這個向量裡比對，用無 key 的 BLAKE3 代替。
fn plain_id(bytes: &[u8]) -> kist_format::v4::ids::MetaId {
    kist_format::v4::ids::MetaId::from_bytes(*blake3::hash(bytes).as_bytes())
}

/// §7 的切段規則：目錄 n0..n19999（依 bytes 排序），正式的上限。每段的第一個名稱與項數由 Go 程式獨立算出。
#[test]
fn directory_segmentation_matches_the_independent_vector() {
    use kist_format::v4::meta::{
        build_dir, decode_item, meta_kind, node_type, Entry, Item, Limits,
    };
    let mut names: Vec<Vec<u8>> = (0..20000).map(|i| format!("n{i}").into_bytes()).collect();
    names.sort();
    let entries: Vec<Entry> = names
        .into_iter()
        .map(|n| {
            let mut e = Entry::new(n, node_type::FILE, meta_kind::GENERIC);
            e.mtime = Some(1);
            e
        })
        .collect();
    let built = build_dir(entries, Limits::V4, &is_cut, &plain_id).unwrap();
    let items: std::collections::HashMap<_, _> = built
        .items
        .iter()
        .map(|(id, b)| (*id, decode_item(b).unwrap()))
        .collect();
    let Item::Dir(root) = &items[&built.root] else {
        panic!("the root is not a Dir");
    };
    assert_eq!(root.level, 1);
    let got: Vec<(String, usize)> = root
        .segs
        .iter()
        .map(|s| match &items[&s.id] {
            Item::Seg(seg) => (
                String::from_utf8(s.first.clone()).unwrap(),
                seg.entries.len(),
            ),
            other => panic!("{other:?}"),
        })
        .collect();
    let expected = [
        ("n0", 1960),
        ("n11761", 7116),
        ("n18166", 4667),
        ("n4367", 4234),
        ("n8178", 1164),
        ("n9225", 859),
    ];
    let expected: Vec<(String, usize)> =
        expected.iter().map(|(n, c)| (n.to_string(), *c)).collect();
    assert_eq!(got, expected);
}

/// chunk 清單的切段與 ChunkIndex 的 size：ID = BLAKE3(i 的 u32 little-endian)、明文長度 1000+i，i 從 0 到
/// 19999。每段的長度與明文長度合計由 Go 程式獨立算出。
#[test]
fn chunk_list_segmentation_and_sizes_match_the_independent_vector() {
    use kist_format::v4::ids::ChunkId;
    use kist_format::v4::meta::{build_chunks, content_type, decode_item, Item, Limits};
    let chunks: Vec<(ChunkId, u64)> = (0..20000u32)
        .map(|i| {
            (
                ChunkId::from_bytes(*blake3::hash(&i.to_le_bytes()).as_bytes()),
                1000 + u64::from(i),
            )
        })
        .collect();
    let built = build_chunks(chunks, Limits::V4, &is_cut, &plain_id).unwrap();
    assert_eq!((built.ct, built.chunks.len()), (content_type::INDIRECT, 1));
    let items: std::collections::HashMap<_, _> = built
        .items
        .iter()
        .map(|(id, b)| (*id, decode_item(b).unwrap()))
        .collect();
    let Item::ChunkIndex(root) = &items[&built.chunks[0].to_meta()] else {
        panic!("the root is not a ChunkIndex");
    };
    assert_eq!(root.level, 1);
    let got: Vec<(usize, u64)> = root
        .parts
        .iter()
        .map(|p| match &items[&p.id] {
            Item::ChunkList(l) => (l.chunks.len(), p.size),
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(
        got,
        [
            (3865, 11332180),
            (8020, 71173490),
            (2770, 39526515),
            (5345, 97957815)
        ]
    );
}

/// snapshot：roots 0x02×32、2026-09-04T12:00:00.123456789Z、host "h"、client 0x1f×16、parent 是同一個 client
/// 的 `snapshots/<c>/20260904T120000000000001Z`、stats {files 3, dirs 1, bytes 10}。
#[test]
fn snapshot_matches_the_golden_file() {
    use kist_format::v4::ids::{ClientId, MetaId};
    use kist_format::v4::keys::{Key, SnapTs};
    use kist_format::v4::snapshot::{Snapshot, SnapshotStats};
    let client = ClientId::from_bytes([0x1f; 16]);
    let parent = Key::Snapshot {
        client,
        ts: SnapTs::from_nanos(CREATED_NS + 1).unwrap(),
        replica: false,
    }
    .path();
    let s = Snapshot {
        v: 4,
        roots: MetaId::from_bytes([2; 32]),
        time: CREATED_NS + 123_456_789,
        host: "h".into(),
        user: String::new(),
        client,
        parent: Some(parent),
        stats: SnapshotStats {
            files: 3,
            dirs: 1,
            symlinks: 0,
            bytes: 10,
        },
    };
    let bytes = s.encode().unwrap();
    check("snapshot.cbor", &bytes);
    assert_eq!(Snapshot::decode(&golden("snapshot.cbor")).unwrap(), s);
}

/// RootSeg：一個目錄 root（/srv/data）與一個檔案 root（/etc/hosts，posix）。
#[test]
fn root_seg_matches_the_golden_file() {
    use kist_format::v4::ids::MetaId;
    use kist_format::v4::meta::{decode_item, meta_kind, node_type, Entry, Item};
    use kist_format::v4::snapshot::{Root, RootSeg};
    let mut hosts = Entry::new(b"hosts".to_vec(), node_type::FILE, meta_kind::POSIX);
    hosts.size = 220;
    hosts.mode = Some(0o100644);
    hosts.uid = Some(0);
    hosts.gid = Some(0);
    hosts.mtime = Some(CREATED_NS);
    let roots = vec![
        Root {
            path: b"/etc/hosts".to_vec(),
            t: node_type::FILE,
            tree: None,
            entry: Some(hosts),
        },
        Root {
            path: b"/srv/data".to_vec(),
            t: node_type::DIR,
            tree: Some(MetaId::from_bytes([3; 32])),
            entry: None,
        },
    ];
    let item = Item::RootSeg(RootSeg::new(roots));
    let bytes = item.encode().unwrap();
    check("root_seg.cbor", &bytes);
    assert_eq!(decode_item(&golden("root_seg.cbor")).unwrap(), item);
}

/// head：g=2、w=1000、parent 0x11×32、分片 0 是 0x22×32（其餘 null）、grace 2 小時、drain {from 2, old_grace
/// 72 小時}、一個 bad 的 snapshot（first 1）、hold {1..=1, 72 小時}。
#[test]
fn head_matches_the_golden_file() {
    use kist_format::v4::ids::{ClientId, ObjectId};
    use kist_format::v4::index::{Bad, Drain, Head, Hold, Settings};
    use kist_format::v4::keys::{Key, SnapTs};
    let mut h = Head::initial(1000, Settings::defaults(1));
    h.g = 2;
    h.parent = Some(ObjectId::from_bytes([0x11; 32]));
    h.shards[0] = Some(ObjectId::from_bytes([0x22; 32]));
    h.settings.grace = 2 * 3600;
    h.drain = Some(Drain {
        from: 2,
        old_grace: 72 * 3600,
    });
    h.bad = vec![Bad {
        key: Key::Snapshot {
            client: ClientId::from_bytes([0x1f; 16]),
            ts: SnapTs::from_nanos(CREATED_NS).unwrap(),
            replica: false,
        }
        .path(),
        first: 1,
    }];
    h.hold = Some(Hold {
        from: 1,
        lost: 1,
        grace: 72 * 3600,
    });
    check("head.cbor", &h.encode().unwrap());
    assert_eq!(Head::decode(&golden("head.cbor"), 2).unwrap(), h);
}

/// map 分片 0xaa：一筆 client 0x1f 的 live 資料 pack（since 1），一筆維護角色 repack 出來、held 的 condemned
/// mpack（since 2、at 3、age 72 小時）。
#[test]
fn map_shard_matches_the_golden_file() {
    use kist_format::v4::ids::{ClientId, ItemId, ObjectId};
    use kist_format::v4::index::{status, Record, Shard, Status};
    use kist_format::v4::pack::{Kind, PackEntry};
    let name = |rest: u8| {
        let mut b = [rest; 32];
        b[0] = 0xaa;
        ObjectId::from_bytes(b)
    };
    let entry = |b: u8, o: u64| PackEntry {
        i: ItemId::from_bytes([b; 32]),
        o,
        l: 41,
        r: 10,
    };
    let live = Record {
        id: name(1),
        client: ClientId::from_bytes([0x1f; 16]),
        kind: Kind::Data,
        size: 106,
        entries: vec![entry(1, 8), entry(2, 49)],
        by_prune: false,
        since: 1,
        status: Status::LIVE,
        held: false,
    };
    let condemned = Record {
        id: name(2),
        client: ClientId::MAINT,
        kind: Kind::Meta,
        size: 65,
        entries: vec![entry(3, 8)],
        by_prune: true,
        since: 2,
        status: Status {
            s: status::CONDEMNED,
            at: 3,
            age: 72 * 3600,
        },
        held: true,
    };
    let shard = Shard::new(0xaa, vec![condemned, live]);
    check("map_shard.cbor", &shard.encode().unwrap());
    assert_eq!(
        Shard::decode(&golden("map_shard.cbor"), 0xaa).unwrap(),
        shard
    );
}

/// delta：client 0x1f 寫的一個資料 pack（兩項）與一個 mpack（一項）。
#[test]
fn delta_matches_the_golden_file() {
    use kist_format::v4::ids::{ClientId, ItemId, ObjectId};
    use kist_format::v4::index::{Delta, DeltaPack};
    use kist_format::v4::pack::{Kind, PackEntry};
    let entry = |b: u8, o: u64| PackEntry {
        i: ItemId::from_bytes([b; 32]),
        o,
        l: 41,
        r: 10,
    };
    let client = ClientId::from_bytes([0x1f; 16]);
    let d = Delta::new(
        client,
        vec![
            DeltaPack {
                id: ObjectId::from_bytes([0x31; 32]),
                kind: Kind::Data,
                size: 106,
                entries: vec![entry(1, 8), entry(2, 49)],
            },
            DeltaPack {
                id: ObjectId::from_bytes([0x32; 32]),
                kind: Kind::Meta,
                size: 65,
                entries: vec![entry(3, 8)],
            },
        ],
    );
    check("delta.cbor", &d.encode().unwrap());
    assert_eq!(Delta::decode(&golden("delta.cbor"), &client).unwrap(), d);
}

/// 測試用的確定性位元組流（"kist/test/<seed>" 的 BLAKE3 XOF），與 tests/parity.rs 相同。
fn fake_pack(seed: &str, n: usize) -> Vec<u8> {
    let mut hasher = blake3::Hasher::new();
    hasher.update(format!("kist/test/{seed}").as_bytes());
    let mut out = vec![0u8; n];
    hasher.finalize_xof().fill(&mut out);
    out
}

/// §13：v4 的 parity 是 v3 的排版、`v` 是 4。獨立核對：v3 凍結的 golden（`testdata/parity-golden.txt`，
/// 原本與已移除的 Go 實作共用）只差 `v` 那一個 byte（`a7 61 76 03` → `04`）。
#[test]
fn parity_is_the_v3_layout_with_version_4() {
    use kist_format::v4::ids::ObjectId;
    let text = std::fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/testdata/parity-golden.txt"),
    )
    .unwrap();
    let mut lines = text.lines();
    let id = ObjectId::from_hex(lines.next().unwrap().strip_prefix("pack ").unwrap()).unwrap();
    let mut expected = hex::decode(lines.next().unwrap().strip_prefix("parity ").unwrap()).unwrap();
    assert_eq!(expected[..4], [0xa7, 0x61, b'v', 0x03], "v3 golden header");
    expected[3] = 0x04;

    let pack = fake_pack("golden", 4321);
    assert_eq!(ObjectId::of(&pack), id);
    let ours = kist_format::v4::parity::encode(&id, &pack, 2).unwrap();
    assert_eq!(hex::encode(&ours), hex::encode(&expected));
    check("parity.cbor", &ours);

    let obj = kist_format::v4::parity::parse(&golden("parity.cbor")).unwrap();
    let mut bad = pack.clone();
    bad[100] ^= 0x5a;
    bad[4000] ^= 0x5a;
    assert_eq!(obj.repair(&id, &bad).unwrap(), pack);
}
