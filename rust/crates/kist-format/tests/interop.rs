//! 跨語言 conformance（tree 向量）：與 Go 端 internal/interop 同一棵樹，
//! 兩邊必須編出相同的規範 CBOR。
//!
//! v3 起向量由 **Rust（產品／規格管理者）錄製**：`UPDATE_VECTOR=1 cargo test
//! -p kist-format --test interop` 重寫 `tests/testdata/tree-canonical.hex`，
//! Go 端移植時必須對同一棵樹編出相同 bytes（V3-TREE-1..3）。

use kist_format::tree::{content_type, meta_kind, node_type, Entry, Tree};
use kist_format::TreeId;

#[test]
fn interop_tree_canonical_cbor() {
    // 與 Go 端同一棵樹（每種欄位、每種 metadata kind 都有）：兩邊必須編出
    // 相同的規範 CBOR（tree ID 依賴它）。
    let mut id1 = [0u8; 32];
    for (i, b) in id1.iter_mut().enumerate() {
        *b = i as u8;
    }
    let mut sub = [0u8; 32];
    for (i, b) in sub.iter_mut().enumerate() {
        *b = 0xA0 + i as u8;
    }
    let mut xattrs = std::collections::BTreeMap::new();
    xattrs.insert(
        serde_bytes::ByteBuf::from(&b"user.k"[..]),
        serde_bytes::ByteBuf::from(&b"v"[..]),
    );
    let tree = Tree::new(
        vec![
            Entry {
                name: b"a.txt".to_vec(),
                kind: node_type::FILE,
                meta_kind: meta_kind::POSIX,
                size: 4096,
                target: Vec::new(),
                content: content_type::DIRECT,
                chunks: vec![kist_format::ChunkId::from_bytes(id1)],
                subtree: TreeId::ZERO,
                mode: Some(0o100644),
                uid: Some(1000),
                gid: Some(100),
                mtime_ns: Some(1788605504101452995),
                ctime_ns: Some(1788605504000000001),
                dev: None,
                inode: None,
                nlink: None,
                xattrs: None,
                etag: None,
                vern: None,
            },
            Entry {
                name: b"big.bin".to_vec(),
                kind: node_type::FILE,
                meta_kind: meta_kind::POSIX,
                size: 0,
                target: Vec::new(),
                content: content_type::INDIRECT,
                chunks: vec![
                    kist_format::ChunkId::from_bytes(id1),
                    kist_format::ChunkId::from_bytes(sub),
                ],
                subtree: TreeId::ZERO,
                mode: Some(0o100600),
                uid: Some(0),
                gid: Some(0),
                mtime_ns: Some(1788605500000000000),
                ctime_ns: None,
                dev: Some(8),
                inode: Some(999),
                nlink: Some(3),
                xattrs: Some(xattrs),
                etag: None,
                vern: None,
            },
            Entry {
                name: b"dir".to_vec(),
                kind: node_type::DIR,
                meta_kind: meta_kind::POSIX,
                size: 0,
                target: Vec::new(),
                content: content_type::DIRECT,
                chunks: Vec::new(),
                subtree: TreeId::from_bytes(sub),
                mode: Some(0o040755),
                uid: Some(0),
                gid: Some(0),
                mtime_ns: Some(1788605500000000000),
                ctime_ns: None,
                dev: None,
                inode: None,
                nlink: None,
                xattrs: None,
                etag: None,
                vern: None,
            },
            Entry {
                name: b"link".to_vec(),
                kind: node_type::SYMLINK,
                meta_kind: meta_kind::POSIX,
                size: 0,
                target: b"../a.txt".to_vec(),
                content: content_type::DIRECT,
                chunks: Vec::new(),
                subtree: TreeId::ZERO,
                mode: Some(0o120777),
                uid: Some(0),
                gid: Some(0),
                mtime_ns: Some(1788605500000000000),
                ctime_ns: None,
                dev: None,
                inode: None,
                nlink: None,
                xattrs: None,
                etag: None,
                vern: None,
            },
            Entry {
                name: b"z-s3.bin".to_vec(),
                kind: node_type::FILE,
                meta_kind: meta_kind::S3,
                size: 777,
                target: Vec::new(),
                content: content_type::DIRECT,
                chunks: vec![kist_format::ChunkId::from_bytes(sub)],
                subtree: TreeId::ZERO,
                mode: None,
                uid: None,
                gid: None,
                mtime_ns: Some(1788605500000000000),
                ctime_ns: None,
                dev: None,
                inode: None,
                nlink: None,
                xattrs: None,
                etag: Some(serde_bytes::ByteBuf::from(
                    &b"\"5e6f80a1c9de4c2b95e6f81a03cf8f4d\""[..],
                )),
                vern: Some(serde_bytes::ByteBuf::from(
                    &b"3sL4k1JtX9qZ7wR2.noS3vId8UuM5pQ0"[..],
                )),
            },
        ],
        None,
    );
    let encoded = kist_format::cbor::encode(&tree).unwrap();
    let path = "tests/testdata/tree-canonical.hex";
    if std::env::var_os("UPDATE_VECTOR").is_some() {
        std::fs::write(path, format!("{}\n", to_hex(&encoded))).unwrap();
        return;
    }
    let want = std::fs::read_to_string(path)
        .expect("testdata/tree-canonical.hex")
        .trim()
        .to_owned();
    assert_eq!(
        to_hex(&encoded),
        want,
        "tree encoding diverged from the recorded vector"
    );
}

fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
