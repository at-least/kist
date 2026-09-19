//! tree 上傳策略與 index blob 的取代規則。

mod common;

use common::*;
use kist_core::CheckOptions;
use kist_format::index::IndexBlob;
use kist_format::ObjectId;

/// tree 是 content-addressed、put 冪等：壞掉的 tree 在下一次（來源未變的）backup 要被重寫回去，
/// 而不是因為「名稱已存在」而永遠跳過。
#[tokio::test]
async fn corrupt_tree_is_healed_by_the_next_backup() {
    let t = TestRepo::new().await;
    let src = t.dir.path().join("src");
    make_source(&src);
    let repo = t.open().await;
    repo.backup(std::slice::from_ref(&src), backup_options())
        .await
        .unwrap();
    for tree in walk_files(&t.repo_path().join("trees")) {
        if tree.is_file() {
            let mut bytes = std::fs::read(&tree).unwrap();
            bytes[40] ^= 0xff;
            std::fs::write(&tree, bytes).unwrap();
        }
    }
    let before = repo
        .check(CheckOptions {
            read_data: false,
            repair: false,
        })
        .await
        .unwrap();
    assert!(!before.errors.is_empty(), "破壞要先被看見");

    let s = repo
        .backup(std::slice::from_ref(&src), backup_options())
        .await
        .unwrap();
    assert_eq!(
        s.report.chunks_new, 0,
        "資料沒變，不該有新 chunk：{:?}",
        s.report
    );
    let after = repo
        .check(CheckOptions {
            read_data: false,
            repair: false,
        })
        .await
        .unwrap();
    // 舊 snapshot 仍指向壞掉的…不，tree 名稱相同，重寫後兩個 snapshot 都好了
    assert!(after.errors.is_empty(), "{:?}", after.errors);
}

/// 新 index blob 的 `supersedes` 列出的舊 blob 必須被忽略（M3 repack 依賴這點）。
#[tokio::test]
async fn superseded_index_blobs_are_ignored() {
    let t = TestRepo::new().await;
    let src = t.dir.path().join("src");
    make_source(&src);
    let repo = t.open().await;
    repo.backup(std::slice::from_ref(&src), backup_options())
        .await
        .unwrap();
    let old_blob = walk_files(&t.repo_path().join("indexes"))
        .into_iter()
        .find(|p| p.is_file())
        .unwrap();
    let old_id = ObjectId::from_hex(old_blob.file_name().unwrap().to_str().unwrap()).unwrap();
    assert!(repo.load_index().await.unwrap().pack_count() > 0);

    let mut blob = IndexBlob::new(Vec::new());
    blob.supersedes = vec![old_id];
    repo.write_index(blob).await.unwrap();

    let index = repo.load_index().await.unwrap();
    assert_eq!(index.pack_count(), 0, "被取代的 blob 裡的 pack 不該出現");
    assert!(index.is_empty());
}

/// 寫入端必須擋住讀取端會拒絕的 tree（讀取路徑有 `Tree::validate`，seal 沒有）：
/// 來源端冒出 `..`、重複或未排序的名稱時，backup 要當場失敗，而不是把
/// 「兩種實作的讀取端都拒讀」的 tree 寫進 repo（Go 的 Encode 會擋）。
#[tokio::test]
async fn seal_tree_rejects_trees_readers_would_reject() {
    use kist_format::tree::{content_type, meta_kind, node_type, Entry, Tree};

    fn file_entry(name: &[u8]) -> Entry {
        Entry {
            name: name.to_vec(),
            kind: node_type::FILE,
            meta_kind: meta_kind::GENERIC,
            size: 0,
            target: Vec::new(),
            content: content_type::DIRECT,
            chunks: Vec::new(),
            subtree: kist_format::TreeId::ZERO,
            mode: None,
            uid: None,
            gid: None,
            mtime_ns: None,
            ctime_ns: None,
            dev: None,
            inode: None,
            nlink: None,
            xattrs: None,
            etag: None,
            vern: None,
        }
    }

    let t = TestRepo::new().await;
    let repo = t.open().await;
    let bad_trees: Vec<(&str, Tree)> = vec![
        ("dotdot name", Tree::new(vec![file_entry(b"..")], None)),
        ("empty name", Tree::new(vec![file_entry(b"")], None)),
        ("slash in name", Tree::new(vec![file_entry(b"a/b")], None)),
        ("duplicate names", Tree::new(vec![file_entry(b"a"), file_entry(b"a")], None)),
        ("unsorted names", Tree::new(vec![file_entry(b"b"), file_entry(b"a")], None)),
    ];
    for (what, tree) in bad_trees {
        let result = repo.seal_tree(tree).await;
        assert!(result.is_err(), "{what}: seal_tree 必須回錯，卻成功寫出");
    }
}
