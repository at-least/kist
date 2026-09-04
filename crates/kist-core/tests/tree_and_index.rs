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
    let before = repo.check(CheckOptions { read_data: false }).await.unwrap();
    assert!(!before.errors.is_empty(), "破壞要先被看見");

    let s = repo
        .backup(std::slice::from_ref(&src), backup_options())
        .await
        .unwrap();
    assert_eq!(
        s.stats.chunks_new, 0,
        "資料沒變，不該有新 chunk：{:?}",
        s.stats
    );
    let after = repo.check(CheckOptions { read_data: false }).await.unwrap();
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
