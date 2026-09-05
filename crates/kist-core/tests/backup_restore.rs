//! backup → restore 的端到端行為。

mod common;

use common::*;
use kist_core::{CoreError, RestoreOptions};

#[tokio::test]
async fn backup_then_restore_is_byte_identical() {
    let t = TestRepo::new().await;
    let src = t.dir.path().join("src");
    make_source(&src);

    let repo = t.open().await;
    let summary = repo
        .backup(std::slice::from_ref(&src), backup_options())
        .await
        .unwrap();
    assert!(summary.stats.files >= 7, "{:?}", summary.stats);
    assert!(
        summary.stats.packs_new >= 2,
        "小 pack 設定下應該有多個 pack：{:?}",
        summary.stats
    );
    assert!(t.repo_path().join(&summary.snapshot_key).is_file());

    let target = t.dir.path().join("out");
    repo.restore(&summary.snapshot_key, &target, RestoreOptions::default())
        .await
        .unwrap();
    // restore 會在 target 底下重建完整的絕對路徑
    let restored = target.join(src.strip_prefix("/").unwrap_or(&src));
    assert_same_tree(&src, &restored);
}

#[tokio::test]
async fn second_backup_writes_no_new_packs() {
    let t = TestRepo::new().await;
    let src = t.dir.path().join("src");
    make_source(&src);
    let repo = t.open().await;

    let first = repo
        .backup(std::slice::from_ref(&src), backup_options())
        .await
        .unwrap();
    let packs_after_first = t.count("packs");
    let trees_after_first = t.count("trees");
    assert!(packs_after_first > 0);

    let second = repo
        .backup(std::slice::from_ref(&src), backup_options())
        .await
        .unwrap();
    assert_ne!(first.snapshot_key, second.snapshot_key);
    assert_eq!(
        t.count("packs"),
        packs_after_first,
        "第二次 backup 不該寫新 pack"
    );
    assert_eq!(
        t.count("trees"),
        trees_after_first,
        "目錄沒變，tree 應該全部重用"
    );
    assert_eq!(second.stats.chunks_new, 0);
    assert_eq!(second.stats.packs_new, 0);
    assert_eq!(second.stats.bytes, first.stats.bytes);
    assert_eq!(second.parent.as_deref(), Some(first.snapshot_key.as_str()));
    assert_eq!(t.count("snapshots"), 2);
}

#[tokio::test]
async fn modified_file_is_picked_up_and_only_it_is_new() {
    let t = TestRepo::new().await;
    let src = t.dir.path().join("src");
    make_source(&src);
    let repo = t.open().await;
    repo.backup(std::slice::from_ref(&src), backup_options())
        .await
        .unwrap();

    // 改一個檔（內容 + mtime）、新增一個檔、刪一個檔
    std::fs::write(src.join("small.txt"), b"changed\n").unwrap();
    std::fs::write(src.join("sub/new.txt"), b"new").unwrap();
    std::fs::remove_file(src.join("zeros.bin")).unwrap();

    let second = repo
        .backup(std::slice::from_ref(&src), backup_options())
        .await
        .unwrap();
    assert!(
        second.stats.chunks_new >= 2 && second.stats.chunks_new <= 4,
        "{:?}",
        second.stats
    );

    let target = t.dir.path().join("out");
    repo.restore(&second.snapshot_key, &target, RestoreOptions::default())
        .await
        .unwrap();
    let restored = target.join(src.strip_prefix("/").unwrap_or(&src));
    assert_same_tree(&src, &restored);
}

#[tokio::test]
async fn touching_mtime_without_changing_content_does_not_write_chunks() {
    // parent 快速路徑失效（mtime 變了）也只是重讀檔案，chunk 去重仍然不寫新東西。
    let t = TestRepo::new().await;
    let src = t.dir.path().join("src");
    make_source(&src);
    let repo = t.open().await;
    repo.backup(std::slice::from_ref(&src), backup_options())
        .await
        .unwrap();

    let later = filetime::FileTime::from_unix_time(2_000_000_000, 0);
    filetime::set_file_mtime(src.join("random.bin"), later).unwrap();
    let second = repo
        .backup(std::slice::from_ref(&src), backup_options())
        .await
        .unwrap();
    assert_eq!(second.stats.chunks_new, 0);
    assert_eq!(second.stats.packs_new, 0);
}

#[tokio::test]
async fn large_file_uses_indirect_content_and_restores() {
    let t = TestRepo::new().await;
    let src = t.dir.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    // 16 KiB 平均 chunk × 256 = 4 MiB；給 8 MiB 保證超過 inline 上限
    std::fs::write(src.join("big.bin"), random_bytes(7, 8 * 1024 * 1024)).unwrap();
    let repo = t.open().await;
    let s = repo
        .backup(std::slice::from_ref(&src), backup_options())
        .await
        .unwrap();
    assert!(s.stats.chunks_new > 256, "{:?}", s.stats);

    let target = t.dir.path().join("out");
    repo.restore(&s.snapshot_key, &target, RestoreOptions::default())
        .await
        .unwrap();
    let restored = target.join(src.strip_prefix("/").unwrap_or(&src));
    assert_same_tree(&src, &restored);
}

#[tokio::test]
async fn huge_directory_is_split_into_tree_parts_and_restores() {
    let t = TestRepo::new().await;
    let src = t.dir.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    for i in 0..(kist_format::tree::MAX_NODES_PER_TREE + 5) {
        std::fs::write(src.join(format!("f{i:06}")), format!("{i}")).unwrap();
    }
    let repo = t.open().await;
    let s = repo
        .backup(std::slice::from_ref(&src), backup_options())
        .await
        .unwrap();
    assert_eq!(
        s.stats.files as usize,
        kist_format::tree::MAX_NODES_PER_TREE + 5
    );
    // 根 tree（1 段）+ 大目錄（2 段）
    assert_eq!(t.count("trees"), 3);

    let target = t.dir.path().join("out");
    repo.restore(&s.snapshot_key, &target, RestoreOptions::default())
        .await
        .unwrap();
    let restored = target.join(src.strip_prefix("/").unwrap_or(&src));
    assert_same_tree(&src, &restored);
}

#[tokio::test]
async fn backup_of_multiple_paths_and_single_file() {
    let t = TestRepo::new().await;
    let a = t.dir.path().join("a");
    let b = t.dir.path().join("b");
    std::fs::create_dir_all(&a).unwrap();
    std::fs::create_dir_all(&b).unwrap();
    std::fs::write(a.join("x"), b"x").unwrap();
    std::fs::write(b.join("y"), b"y").unwrap();
    let file = t.dir.path().join("single.txt");
    std::fs::write(&file, b"single").unwrap();

    let repo = t.open().await;
    let s = repo
        .backup(&[a.clone(), b.clone(), file.clone()], backup_options())
        .await
        .unwrap();
    assert_eq!(s.stats.files, 3);

    let target = t.dir.path().join("out");
    repo.restore(&s.snapshot_key, &target, RestoreOptions::default())
        .await
        .unwrap();
    let rel = |p: &std::path::Path| target.join(p.strip_prefix("/").unwrap_or(p));
    assert_same_tree(&a, &rel(&a));
    assert_same_tree(&b, &rel(&b));
    assert_eq!(std::fs::read(rel(&file)).unwrap(), b"single");
}

#[tokio::test]
async fn restore_unknown_snapshot_fails() {
    let t = TestRepo::new().await;
    let repo = t.open().await;
    let err = repo
        .restore(
            "snapshots/00/nope",
            &t.dir.path().join("out"),
            RestoreOptions::default(),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, CoreError::SnapshotNotFound(_)), "{err}");
}

#[tokio::test]
async fn list_and_resolve_snapshots() {
    let t = TestRepo::new().await;
    let src = t.dir.path().join("src");
    make_source(&src);
    let repo = t.open().await;
    let s1 = repo
        .backup(std::slice::from_ref(&src), backup_options())
        .await
        .unwrap();
    let s2 = repo
        .backup(std::slice::from_ref(&src), backup_options())
        .await
        .unwrap();

    let list = repo.list_snapshots().await.unwrap();
    assert_eq!(list.len(), 2);
    assert_eq!(list[0].key, s1.snapshot_key, "依時間排序");
    assert_eq!(list[1].key, s2.snapshot_key);
    assert_eq!(list[0].snapshot.host, "testhost");
    assert_eq!(
        list[1].snapshot.parent.as_deref(),
        Some(s1.snapshot_key.as_str())
    );

    assert_eq!(
        repo.resolve_snapshot("latest").await.unwrap(),
        s2.snapshot_key
    );
    assert_eq!(
        repo.resolve_snapshot(&s1.snapshot_key).await.unwrap(),
        s1.snapshot_key
    );
    // 只給時間戳也行
    let ts = s1.snapshot_key.rsplit('/').next().unwrap();
    assert_eq!(repo.resolve_snapshot(ts).await.unwrap(), s1.snapshot_key);
    assert!(matches!(
        repo.resolve_snapshot("20000101T000000000000000Z").await,
        Err(CoreError::SnapshotNotFound(_))
    ));
}

/// `cp -p` / `rsync -a` 會保留 mtime；內容不同但大小相同時，只比 size + mtime 會漏掉。
/// ctime 是 kernel 在寫入時更新、使用者改不了的，所以能抓到。
#[cfg(unix)]
#[tokio::test]
async fn same_size_and_mtime_but_different_content_is_detected() {
    let t = TestRepo::new().await;
    let src = t.dir.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    let file = src.join("data.bin");
    std::fs::write(&file, random_bytes(11, 100 * 1024)).unwrap();
    let mtime = filetime::FileTime::from_unix_time(1_700_000_000, 0);
    filetime::set_file_mtime(&file, mtime).unwrap();

    let repo = t.open().await;
    repo.backup(std::slice::from_ref(&src), backup_options())
        .await
        .unwrap();

    // 換掉內容（同大小），再把 mtime 設回去
    std::fs::write(&file, random_bytes(12, 100 * 1024)).unwrap();
    filetime::set_file_mtime(&file, mtime).unwrap();

    let second = repo
        .backup(std::slice::from_ref(&src), backup_options())
        .await
        .unwrap();
    assert!(
        second.stats.chunks_new > 0,
        "內容變了卻沒有新 chunk：快速路徑誤判 {:?}",
        second.stats
    );
    let target = t.dir.path().join("out");
    repo.restore(&second.snapshot_key, &target, RestoreOptions::default())
        .await
        .unwrap();
    let restored = target.join(src.strip_prefix("/").unwrap_or(&src));
    assert_same_tree(&src, &restored);
}
