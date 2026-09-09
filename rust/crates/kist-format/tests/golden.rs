//! Golden file 測試：每種結構各有一份固定範例，序列化結果必須與 `tests/golden/*` 完全相同。
//!
//! 目的：格式一旦凍結，任何會改變 on-disk bytes 的修改（改欄位名、順序、型別）都會在這裡爆掉，
//! 逼修改者有意識地更新 golden file 與 `docs/format.md`。
//!
//! 更新方式：`UPDATE_GOLDEN=1 cargo test -p kist-format --test golden`，然後 review diff。

use std::path::PathBuf;

use kist_format::config::{ChunkerParams, KdfParams, KeySlot, RepoConfig};
use kist_format::index::{IndexBlob, IndexPack};
use kist_format::pack::{self, PackEntry, PackTrailer};
use kist_format::snapshot::{Root, Snapshot, SnapshotStats};
use kist_format::tree::{content_type, meta_kind, node_type, ChunkList, Entry, Tree};
use kist_format::{cbor, ChunkId, TreeId, FORMAT_VERSION};

/// `2026-09-04T12:00:00Z` 的 Unix 奈秒。
const CREATED_NS: i64 = 1_788_523_200_000_000_000;

fn golden_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden")
}

/// 比對 bytes；設 UPDATE_GOLDEN=1 時改為寫入。
fn check(name: &str, actual: &[u8]) {
    let path = golden_dir().join(name);
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        std::fs::write(&path, actual).unwrap();
        return;
    }
    let expected = std::fs::read(&path)
        .unwrap_or_else(|e| panic!("cannot read golden file {}: {e}", path.display()));
    assert!(
        expected == actual,
        "{name} differs from golden file\n expected: {}\n   actual: {}",
        hex::encode(&expected),
        hex::encode(actual)
    );
}

fn chunk_id(b: u8) -> ChunkId {
    ChunkId::from_bytes([b; 32])
}

fn tree_id(b: u8) -> TreeId {
    TreeId::from_bytes([b; 32])
}

fn sample_config() -> RepoConfig {
    let wrapped = {
        let mut v = vec![0xBB; 24]; // nonce
                                    // 密文(master 32 + Invariants CBOR + tag 16)：長度是示意，真正內容由 kist-crypto 測試釘
        v.extend_from_slice(&[0xCC; 32 + 16]);
        v
    };
    RepoConfig {
        version: FORMAT_VERSION,
        repo_id: (0u8..16).collect(),
        created_ns: CREATED_NS,
        chunker: ChunkerParams::default(),
        pack_target_size: 64 * 1024 * 1024,
        min_reader: FORMAT_VERSION.try_into().unwrap(),
        replicas: 0,
        key: KeySlot {
            version: FORMAT_VERSION,
            name: "default".to_owned(),
            created_ns: CREATED_NS,
            kdf: KdfParams {
                algorithm: "argon2id".to_owned(),
                m_cost_kib: 19 * 1024,
                t_cost: 2,
                p_cost: 1,
                salt: vec![0xAA; 16],
            },
            wrapped,
        },
    }
}

/// 填好預設值的 Entry（posix 檔案），測試再覆寫有興趣的欄位。
fn entry(name: &[u8]) -> Entry {
    Entry {
        name: name.to_vec(),
        kind: node_type::FILE,
        meta_kind: meta_kind::POSIX,
        size: 0,
        target: Vec::new(),
        content: content_type::DIRECT,
        chunks: Vec::new(),
        subtree: TreeId::ZERO,
        mode: Some(0),
        uid: Some(0),
        gid: Some(0),
        mtime_ns: Some(0),
        ctime_ns: None,
        dev: None,
        inode: None,
        nlink: None,
        xattrs: None,
        etag: None,
        vern: None,
    }
}

fn sample_tree() -> Tree {
    let mut a = entry(b"a.txt");
    a.mode = Some(0o100644);
    a.uid = Some(1000);
    a.gid = Some(1000);
    a.mtime_ns = Some(1_700_000_000_123_456_789);
    a.ctime_ns = Some(1_700_000_001_000_000_005);
    a.size = 3_000_000;
    a.chunks = vec![chunk_id(1), chunk_id(2)];
    a.nlink = Some(2); // 硬連結：dev/ino/nlink 只有 nlink > 1 才記
    a.dev = Some(7);
    a.inode = Some(424_242);
    a.xattrs = Some(
        vec![(
            serde_bytes::ByteBuf::from(b"user.comment".to_vec()),
            serde_bytes::ByteBuf::from(b"hi".to_vec()),
        )]
        .into_iter()
        .collect(),
    );

    let mut big = entry(b"big.bin");
    big.size = 10_000_000_000;
    big.chunks = vec![chunk_id(3)];
    big.content = content_type::INDIRECT;

    let mut link = entry(b"link");
    link.kind = node_type::SYMLINK;
    link.mode = Some(0o120777);
    link.target = b"a.txt".to_vec();

    let mut sub = entry(b"sub");
    sub.kind = node_type::DIR;
    sub.mode = Some(0o040755);
    sub.subtree = tree_id(4);

    Tree::new(vec![a, big, link, sub], Some(tree_id(5)))
}

fn sample_trailer() -> PackTrailer {
    PackTrailer::new(vec![
        PackEntry {
            id: chunk_id(1),
            offset: 8,
            length: 1000 + 24 + 16,
            raw_len: 2000,
        },
        PackEntry {
            id: chunk_id(2),
            offset: 1048,
            length: 500 + 24 + 16,
            raw_len: 500,
        },
    ])
}

fn sample_snapshot() -> Snapshot {
    Snapshot {
        version: Snapshot::VERSION,
        client_id: (0u8..16).collect(),
        host: "host".to_owned(),
        user: "user".to_owned(),
        time_ns: CREATED_NS,
        roots: vec![Root {
            path: serde_bytes::ByteBuf::from(b"/home/user".to_vec()),
            tree: tree_id(9),
        }],
        parent: Some(
            "snapshots/000102030405060708090a0b0c0d0e0f/20260903T120000000000000Z".to_owned(),
        ),
        stats: SnapshotStats {
            files: 2,
            dirs: 1,
            symlinks: 1,
            bytes: 10_003_000_000,
        },
    }
}

#[test]
fn config_golden() {
    let v = sample_config();
    let bytes = cbor::encode(&v).unwrap();
    check("config.cbor", &bytes);
    assert_eq!(cbor::decode::<RepoConfig>(&bytes).unwrap(), v);
}

#[test]
fn tree_golden() {
    let v = sample_tree();
    let bytes = cbor::encode(&v).unwrap();
    check("tree.cbor", &bytes);
    assert_eq!(cbor::decode::<Tree>(&bytes).unwrap(), v);
}

#[test]
fn chunk_list_golden() {
    let v = ChunkList::new(vec![chunk_id(1), chunk_id(2), chunk_id(3)]);
    let bytes = cbor::encode(&v).unwrap();
    check("chunk_list.cbor", &bytes);
    assert_eq!(cbor::decode::<ChunkList>(&bytes).unwrap(), v);
}

#[test]
fn pack_trailer_golden() {
    let v = sample_trailer();
    let bytes = cbor::encode(&v).unwrap();
    check("pack_trailer.cbor", &bytes);
    assert_eq!(cbor::decode::<PackTrailer>(&bytes).unwrap(), v);
}

#[test]
fn index_golden() {
    let v = IndexBlob::new(vec![IndexPack {
        pack: kist_format::ObjectId::from_bytes([7; 32]),
        size: 1604,
        entries: sample_trailer().entries,
    }]);
    let bytes = cbor::encode(&v).unwrap();
    check("index.cbor", &bytes);
    assert_eq!(cbor::decode::<IndexBlob>(&bytes).unwrap(), v);
}

#[test]
fn snapshot_golden() {
    let v = sample_snapshot();
    let bytes = cbor::encode(&v).unwrap();
    check("snapshot.cbor", &bytes);
    assert_eq!(cbor::decode::<Snapshot>(&bytes).unwrap(), v);
}

#[test]
fn pack_layout_golden() {
    let mut buf = pack::begin();
    buf.extend_from_slice(&[0x11; 40]); // 假的 chunk entry
    let trailer_sealed = [0x22u8; 50];
    let bytes = pack::finish(buf, &trailer_sealed);
    check("pack_layout.bin", &bytes);
    assert_eq!(pack::trailer_bytes(&bytes).unwrap(), &trailer_sealed);
}

/// 舊版（或少寫欄位的實作）寫出的 tree 少了 `ctime` 等 `#[serde(default)]` 欄位時必須還能讀：
/// 缺少的欄位視為 0。v1 的 `tree-before-ctime.cbor`（nodes/NodeMeta 形狀）已隨 envelope
/// 一起淘汰，這裡改以「v2 bytes 拿掉 `ctime`」重建同樣的容忍場景。
#[test]
fn tree_without_ctime_fields_still_decodes() {
    use ciborium::value::Value;

    let bytes = cbor::encode(&sample_tree()).unwrap();
    let mut value: Value = ciborium::from_reader(std::io::Cursor::new(&bytes)).unwrap();
    // 從每個 entry 的 map 裡移除 "ctime"（與其他可選欄位），模擬沒寫它的實作
    let Value::Map(fields) = &mut value else {
        panic!("tree must encode as a map");
    };
    let mut stripped: Vec<(Value, Value)> = Vec::new();
    for (k, v) in std::mem::take(fields) {
        let v = match (&k, v) {
            (Value::Text(key), Value::Array(entries)) if key == "entries" => Value::Array(
                entries
                    .into_iter()
                    .map(|e| match e {
                        Value::Map(map) => Value::Map(
                            map.into_iter()
                                .filter(|(fk, _)| !matches!(fk, Value::Text(f) if f == "ctime"))
                                .collect(),
                        ),
                        other => other,
                    })
                    .collect(),
            ),
            (_, v) => v,
        };
        stripped.push((k, v));
    }
    *fields = stripped;
    let older = cbor::encode(&value).unwrap();
    assert_ne!(older, bytes, "前置條件：移除欄位要有作用");

    let tree: Tree = cbor::decode(&older).unwrap();
    assert_eq!(tree.entries.len(), 4);
    assert_eq!(tree.entries[0].mtime_ns, Some(1_700_000_000_123_456_789));
    assert_eq!(tree.entries[0].ctime_ns, None);
    let mut expected = sample_tree();
    expected.entries[0].ctime_ns = None;
    assert_eq!(tree, expected);
}

/// 向前相容：未來版本**多寫**的欄位要被忽略（而非報錯），舊讀取端才能讀新 repo。
#[test]
fn tree_with_unknown_fields_still_decodes() {
    use ciborium::value::Value;

    let bytes = cbor::encode(&sample_tree()).unwrap();
    let mut value: Value = ciborium::from_reader(std::io::Cursor::new(&bytes)).unwrap();
    let Value::Map(fields) = &mut value else {
        panic!("tree must encode as a map");
    };
    if let Some((_, Value::Array(entries))) = fields
        .iter_mut()
        .find(|(k, _)| matches!(k, Value::Text(f) if f == "entries"))
    {
        for e in entries.iter_mut() {
            if let Value::Map(map) = e {
                map.push((Value::Text("future_field".to_owned()), Value::from(42u32)));
            }
        }
    }
    let newer = cbor::encode(&value).unwrap();
    assert_ne!(newer, bytes, "前置條件：加欄位要有作用");
    assert_eq!(cbor::decode::<Tree>(&newer).unwrap(), sample_tree());
}
