//! `rebuild-index`：從 pack trailer 重建 index。

mod common;

use common::*;
use kist_core::{CheckOptions, RestoreOptions};

#[tokio::test]
async fn rebuild_after_all_index_blobs_are_lost() {
    let t = TestRepo::new().await;
    let src = t.dir.path().join("src");
    make_source(&src);
    let repo = t.open().await;
    let s = repo
        .backup(std::slice::from_ref(&src), backup_options())
        .await
        .unwrap();
    let before = repo.load_index().await.unwrap();
    for blob in walk_files(&t.repo_path().join("indexes")) {
        if blob.is_file() {
            std::fs::remove_file(blob).unwrap();
        }
    }
    assert!(!repo
        .check(CheckOptions { read_data: false, repair: false })
        .await
        .unwrap()
        .errors
        .is_empty());
    let broken = repo
        .restore(
            &s.snapshot_key,
            &t.dir.path().join("out0"),
            RestoreOptions::default(),
        )
        .await
        .unwrap();
    assert!(!broken.errors.is_empty(), "沒有 index 時檔案應該還原失敗");

    let summary = repo.rebuild_index().await.unwrap();
    assert_eq!(summary.packs as usize, before.pack_count());
    assert_eq!(summary.chunks as usize, before.len());
    assert_eq!(summary.superseded, 0);
    assert_eq!(t.count("indexes"), 1);

    let after = repo.load_index().await.unwrap();
    assert_eq!(after.len(), before.len());
    for (id, loc) in before.chunks() {
        assert_eq!(after.get(&id), Some(loc), "chunk {id}");
    }
    let report = repo.check(CheckOptions { read_data: true, repair: false }).await.unwrap();
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    let target = t.dir.path().join("out");
    repo.restore(&s.snapshot_key, &target, RestoreOptions::default())
        .await
        .unwrap();
    assert_same_tree(&src, &target.join(src.strip_prefix("/").unwrap_or(&src)));
}

#[tokio::test]
async fn rebuild_with_existing_blobs_supersedes_them() {
    let t = TestRepo::new().await;
    let src = t.dir.path().join("src");
    make_source(&src);
    let repo = t.open().await;
    repo.backup(std::slice::from_ref(&src), backup_options())
        .await
        .unwrap();
    std::fs::write(src.join("more.bin"), random_bytes(41, 100 * 1024)).unwrap();
    repo.backup(std::slice::from_ref(&src), backup_options())
        .await
        .unwrap();
    assert_eq!(t.count("indexes"), 2);
    let before = repo.load_index().await.unwrap();

    let summary = repo.rebuild_index().await.unwrap();
    assert_eq!(summary.superseded, 2);
    assert_eq!(
        t.count("indexes"),
        3,
        "舊 blob 不刪（M3 的 GC 才刪），新 blob 取代它們"
    );
    let after = repo.load_index().await.unwrap();
    assert_eq!(after.len(), before.len());
    assert_eq!(after.pack_count(), before.pack_count());
    let report = repo.check(CheckOptions { read_data: false, repair: false }).await.unwrap();
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    assert!(
        report.warnings.is_empty(),
        "重建後不該有未被 index 引用的 pack：{:?}",
        report.warnings
    );
}

#[tokio::test]
async fn rebuild_reports_corrupt_pack_trailer() {
    let t = TestRepo::new().await;
    let src = t.dir.path().join("src");
    make_source(&src);
    let repo = t.open().await;
    repo.backup(std::slice::from_ref(&src), backup_options())
        .await
        .unwrap();
    let mut packs = walk_files(&t.repo_path().join("packs"));
    packs.retain(|p| p.is_file());
    let victim = &packs[0];
    let mut bytes = std::fs::read(victim).unwrap();
    let n = bytes.len();
    bytes[n - 40] ^= 0xff; // trailer 內
    std::fs::write(victim, bytes).unwrap();

    let err = repo.rebuild_index().await.unwrap_err();
    let name = victim.file_name().unwrap().to_str().unwrap();
    assert!(err.to_string().contains(name), "{err}");
}
