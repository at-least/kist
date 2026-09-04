//! Golden file 測試：每種結構各有一份固定範例，序列化結果必須與 `tests/golden/*` 完全相同。
//!
//! 目的：格式一旦凍結，任何會改變 on-disk bytes 的修改（改欄位名、順序、型別）都會在這裡爆掉，
//! 逼修改者有意識地更新 golden file 與 `docs/format.md`。
//!
//! 更新方式：`UPDATE_GOLDEN=1 cargo test -p kist-format --test golden`，然後 review diff。

use std::path::PathBuf;

use kist_format::config::{ChunkerParams, KdfParams, KeySlot, RepoConfig, WrappedKey};
use kist_format::envelope::{Compression, Envelope, ObjectKind};
use kist_format::index::{IndexBlob, IndexPack};
use kist_format::pack::{self, PackEntry, PackTrailer};
use kist_format::snapshot::{Snapshot, SnapshotStats};
use kist_format::tree::{ChunkList, Content, Node, NodeKind, NodeMeta, Tree};
use kist_format::{cbor, ChunkId, ObjectId};

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

fn object_id(b: u8) -> ObjectId {
    ObjectId::from_bytes([b; 32])
}

fn sample_config() -> RepoConfig {
    RepoConfig {
        version: 1,
        repo_id: (0u8..16).collect(),
        created: "2026-09-04T12:00:00Z".to_owned(),
        chunker: ChunkerParams::default(),
        pack_target_size: 64 * 1024 * 1024,
        key: KeySlot {
            version: 1,
            name: "default".to_owned(),
            created: "2026-09-04T12:00:00Z".to_owned(),
            kdf: KdfParams {
                algorithm: "argon2id".to_owned(),
                m_cost_kib: 19 * 1024,
                t_cost: 2,
                p_cost: 1,
                salt: vec![0xAA; 16],
            },
            wrapped_master_key: WrappedKey {
                nonce: vec![0xBB; 24],
                ciphertext: vec![0xCC; 48],
            },
        },
    }
}

fn sample_tree() -> Tree {
    Tree::new(
        vec![
            Node {
                name: b"a.txt".to_vec(),
                meta: NodeMeta {
                    mode: 0o100644,
                    uid: 1000,
                    gid: 1000,
                    mtime_secs: 1_700_000_000,
                    mtime_nanos: 123_456_789,
                    ctime_secs: 1_700_000_001,
                    ctime_nanos: 5,
                    inode: 424_242,
                },
                kind: NodeKind::File {
                    size: 3_000_000,
                    content: Content::Direct {
                        chunks: vec![chunk_id(1), chunk_id(2)],
                    },
                },
            },
            Node {
                name: b"big.bin".to_vec(),
                meta: NodeMeta::default(),
                kind: NodeKind::File {
                    size: 10_000_000_000,
                    content: Content::Indirect {
                        chunks: vec![chunk_id(3)],
                    },
                },
            },
            Node {
                name: b"link".to_vec(),
                meta: NodeMeta {
                    mode: 0o120777,
                    ..NodeMeta::default()
                },
                kind: NodeKind::Symlink {
                    target: b"a.txt".to_vec(),
                },
            },
            Node {
                name: b"sub".to_vec(),
                meta: NodeMeta {
                    mode: 0o040755,
                    ..NodeMeta::default()
                },
                kind: NodeKind::Dir {
                    subtree: object_id(4),
                },
            },
        ],
        Some(object_id(5)),
    )
}

fn sample_trailer() -> PackTrailer {
    PackTrailer::new(vec![
        PackEntry {
            id: chunk_id(1),
            offset: 8,
            length: 1000 + 40,
            raw_len: 2000,
            flags: pack::FLAG_ZSTD,
        },
        PackEntry {
            id: chunk_id(2),
            offset: 1048,
            length: 500 + 40,
            raw_len: 500,
            flags: 0,
        },
    ])
}

fn sample_snapshot() -> Snapshot {
    Snapshot {
        version: 1,
        client_id: (0u8..16).collect(),
        hostname: "host".to_owned(),
        username: "user".to_owned(),
        time: "2026-09-04T12:00:00Z".to_owned(),
        paths: vec![serde_bytes::ByteBuf::from(b"/home/user".to_vec())],
        root: object_id(9),
        parent: Some(
            "snapshots/000102030405060708090a0b0c0d0e0f/20260903T120000000000000Z".to_owned(),
        ),
        stats: SnapshotStats {
            files: 2,
            dirs: 1,
            symlinks: 1,
            bytes_total: 10_003_000_000,
            bytes_new: 3_000_000,
            chunks_total: 3,
            chunks_new: 2,
            packs_new: 1,
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
        pack: object_id(7),
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
fn envelope_golden() {
    let env = Envelope {
        kind: ObjectKind::Tree,
        compression: Compression::Zstd,
        nonce: [0xEE; 24],
        ciphertext: vec![0xDD; 20],
    };
    let bytes = env.encode();
    check("envelope.bin", &bytes);
    assert_eq!(Envelope::parse(&bytes).unwrap(), env);
    // header 就是 AAD
    assert_eq!(&bytes[..32], &env.header_bytes());
}

#[test]
fn pack_layout_golden() {
    let mut buf = pack::begin();
    buf.extend_from_slice(&[0x11; 40]); // 假的 chunk entry
    let trailer_env = [0x22u8; 50];
    let bytes = pack::finish(buf, &trailer_env);
    check("pack_layout.bin", &bytes);
    assert_eq!(pack::trailer_bytes(&bytes).unwrap(), &trailer_env);
}

/// 加 ctime / inode 之前寫出的 tree 必須還能讀：缺少的欄位視為 0（`#[serde(default)]`）。
#[test]
fn tree_without_ctime_fields_still_decodes() {
    let bytes = std::fs::read(golden_dir().join("tree-before-ctime.cbor")).unwrap();
    let tree: Tree = cbor::decode(&bytes).unwrap();
    assert_eq!(tree.nodes.len(), 4);
    let meta = tree.nodes[0].meta;
    assert_eq!(meta.mtime_secs, 1_700_000_000);
    assert_eq!((meta.ctime_secs, meta.ctime_nanos, meta.inode), (0, 0, 0));
}
