//! 跨語言 conformance（tree 向量）：與 Go 端 internal/interop 同一棵樹，
//! 兩邊必須編出相同的規範 CBOR。

use kist_format::tree::{content_type, node_type, Entry, Tree};
use kist_format::TreeId;

#[test]
fn interop_tree_canonical_cbor() {
    // 與 Go 端同一棵樹（每種欄位都有）：兩邊必須編出相同的規範 CBOR
    // （tree ID 依賴它）。
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
                mode: 0o100644,
                uid: 1000,
                gid: 100,
                mtime_ns: 1788605504101452995,
                ctime_ns: 1788605504000000001,
                size: 4096,
                target: Vec::new(),
                chunks: vec![kist_format::ChunkId::from_bytes(id1)],
                content: content_type::DIRECT,
                subtree: TreeId::ZERO,
                dev: 0,
                inode: 0,
                nlink: 0,
                xattrs: None,
            },
            Entry {
                name: b"big.bin".to_vec(),
                kind: node_type::FILE,
                mode: 0o100600,
                uid: 0,
                gid: 0,
                mtime_ns: 1788605500000000000,
                ctime_ns: 0,
                size: 0,
                target: Vec::new(),
                chunks: vec![
                    kist_format::ChunkId::from_bytes(id1),
                    kist_format::ChunkId::from_bytes(sub),
                ],
                content: content_type::INDIRECT,
                subtree: TreeId::ZERO,
                dev: 8,
                inode: 999,
                nlink: 3,
                xattrs: Some(xattrs),
            },            Entry {
                name: b"dir".to_vec(),
                kind: node_type::DIR,
                mode: 0o040755,
                uid: 0,
                gid: 0,
                mtime_ns: 1788605500000000000,
                ctime_ns: 0,
                size: 0,
                target: Vec::new(),
                chunks: Vec::new(),
                content: content_type::DIRECT,
                subtree: TreeId::from_bytes(sub),
                dev: 0,
                inode: 0,
                nlink: 0,
                xattrs: None,
            },
            Entry {
                name: b"link".to_vec(),
                kind: node_type::SYMLINK,
                mode: 0o120777,
                uid: 0,
                gid: 0,
                mtime_ns: 1788605500000000000,
                ctime_ns: 0,
                size: 0,
                target: b"../a.txt".to_vec(),
                chunks: Vec::new(),
                content: content_type::DIRECT,
                subtree: TreeId::ZERO,
                dev: 0,
                inode: 0,
                nlink: 0,
                xattrs: None,
            },

        ],
        None,
    );
    let encoded = kist_format::cbor::encode(&tree).unwrap();
    let want = std::fs::read_to_string("tests/testdata/tree-canonical.hex")
        .expect("testdata/tree-canonical.hex")
        .trim()
        .to_owned();
    assert_eq!(
        to_hex(&encoded),
        want,
        "tree encoding diverged from the Go-recorded vector"
    );
}

fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
