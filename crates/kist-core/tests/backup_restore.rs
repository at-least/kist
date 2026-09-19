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
        summary.report.packs_new >= 2,
        "小 pack 設定下應該有多個 pack：{:?}",
        summary.report
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
    assert_eq!(second.report.chunks_new, 0);
    assert_eq!(second.report.packs_new, 0);
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
        second.report.chunks_new >= 2 && second.report.chunks_new <= 4,
        "{:?}",
        second.report
    );

    let target = t.dir.path().join("out");
    repo.restore(&second.snapshot_key, &target, RestoreOptions::default())
        .await
        .unwrap();
    let restored = target.join(src.strip_prefix("/").unwrap_or(&src));
    assert_same_tree(&src, &restored);
}

/// FIFO 這類非正規檔案必須跳過並記一筆略過（與 Go 實作同款）；
/// 不能把 open（FIFO 無寫端會無限阻塞）帶進 backup——那會讓整個備份掛死。
#[cfg(unix)]
#[tokio::test]
async fn fifo_in_source_tree_is_skipped_not_read() {
    let t = TestRepo::new().await;
    let src = t.dir.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("f.txt"), b"hello").unwrap();
    let fifo = src.join("pipe");
    let ok = std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .expect("mkfifo")
        .success();
    assert!(ok, "mkfifo failed");

    let repo = t.open().await;
    let summary = repo
        .backup(std::slice::from_ref(&src), backup_options())
        .await
        .unwrap();
    assert_eq!(summary.report.errors, 1, "{:?}", summary.report);
    assert_eq!(summary.stats.files, 1, "{:?}", summary.stats);

    let target = t.dir.path().join("out");
    repo.restore(&summary.snapshot_key, &target, RestoreOptions::default())
        .await
        .unwrap();
    let restored = target.join(src.strip_prefix("/").unwrap_or(&src));
    assert_eq!(std::fs::read(restored.join("f.txt")).unwrap(), b"hello");
    assert!(!restored.join("pipe").exists(), "FIFO 不該進 snapshot");
}

/// 檔案 root 明確指到非正規檔：直接失敗並指名原因，不掛死。
#[cfg(unix)]
#[tokio::test]
async fn fifo_as_file_root_fails_loudly() {
    let t = TestRepo::new().await;
    let fifo = t.dir.path().join("pipe");
    let ok = std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .expect("mkfifo")
        .success();
    assert!(ok, "mkfifo failed");

    let repo = t.open().await;
    let err = repo
        .backup(std::slice::from_ref(&fifo), backup_options())
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("not a regular file"),
        "錯誤要指名非正規檔：{err}"
    );
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
    assert_eq!(second.report.chunks_new, 0);
    assert_eq!(second.report.packs_new, 0);
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
    assert!(s.report.chunks_new > 256, "{:?}", s.report);

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
    // v3：root tree = 來源目錄內容本身（大目錄 2 段）——沒有 v2 的合成根，
    // 少一顆樹。
    assert_eq!(t.count("trees"), 2);

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
        second.report.chunks_new > 0,
        "內容變了卻沒有新 chunk：快速路徑誤判 {:?}",
        second.report
    );
    let target = t.dir.path().join("out");
    repo.restore(&second.snapshot_key, &target, RestoreOptions::default())
        .await
        .unwrap();
    let restored = target.join(src.strip_prefix("/").unwrap_or(&src));
    assert_same_tree(&src, &restored);
}

/// 超過 MAX_NODES_PER_TREE（10,000）的目錄會切成多段 tree（`prev` 串接）。
/// 第二次 backup 的 parent 快速路徑必須**跨段**還能用：沒改的檔案直接沿用、
/// 改過的重讀、restore 仍逐 byte 相同。
#[tokio::test]
async fn parent_reuse_across_tree_segments_in_one_dir() {
    let t = TestRepo::new().await;
    let src = t.dir.path().join("big");
    std::fs::create_dir_all(&src).unwrap();
    // 10,050 個檔案 → 至少兩段
    for i in 0..10_050u32 {
        std::fs::write(src.join(format!("f{i:06}.dat")), format!("content {i}\n")).unwrap();
    }
    let repo = t.open().await;
    let first = repo
        .backup(std::slice::from_ref(&src), backup_options())
        .await
        .unwrap();
    let _packs_after_first = t.count("packs");

    // 改掉一批跨段界的檔案（每段的頭、尾、中間都有）；fs::write 會更新
    // mtime，parent 快速路徑因此失效，必須重讀。
    let mut changed = 0u64;
    for i in (0..10_050u32).step_by(97) {
        std::fs::write(src.join(format!("f{i:06}.dat")), format!("CHANGED {i}\n")).unwrap();
        changed += 1;
    }

    let second = repo
        .backup(std::slice::from_ref(&src), backup_options())
        .await
        .unwrap();
    assert_ne!(first.snapshot_key, second.snapshot_key);
    assert_eq!(second.parent.as_deref(), Some(first.snapshot_key.as_str()));
    assert_eq!(
        second.report.files_reused,
        10_050 - changed,
        "跨段界之外沒改的檔案都該走 parent 快速路徑"
    );
    assert_eq!(
        second.report.chunks_new, changed,
        "改過的檔案各有一個新內容 chunk"
    );

    let target = t.dir.path().join("out2");
    repo.restore(&second.snapshot_key, &target, RestoreOptions::default())
        .await
        .unwrap();
    let restored = target.join(src.strip_prefix("/").unwrap_or(&src));
    assert_same_tree(&src, &restored);
}

/// xattr 的 backup → restore 往返（unix）。三個案例：
/// 1. 檔案與目錄的 user.* xattr 原樣回來；
/// 2. 唯讀 mode（0o444）的檔案也帶 xattr：xattr 必須在 chmod **之前**套用
///    （套完 0444 之後 user.* 會設不進去——EACCES）；
/// 3. symlink 條目帶 xattr：不套用（Linux 不能對 symlink 設 user.*，而
///    xattr::set 會跟隨連結寫到目標去）→ 只警告，不算錯、目標不受影響。
#[cfg(unix)]
#[tokio::test]
async fn xattrs_survive_backup_restore() {
    use std::ffi::OsStr;

    let t = TestRepo::new().await;
    let src = t.dir.path().join("src");
    std::fs::create_dir_all(src.join("sub")).unwrap();
    std::fs::write(src.join("plain.txt"), b"plain").unwrap();

    // 1. 一般檔案與目錄
    xattr::set(src.join("plain.txt"), OsStr::new("user.tag"), b"file-value").unwrap();
    xattr::set(src.join("sub"), OsStr::new("user.dir-tag"), b"dir-value").unwrap();

    // 2. 唯讀檔 + xattr：先設 xattr 再轉唯讀（模擬使用者機器上的實況）
    std::fs::write(src.join("ro.txt"), b"read only").unwrap();
    xattr::set(src.join("ro.txt"), OsStr::new("user.locked"), b"yes").unwrap();
    let mut perms = std::fs::metadata(src.join("ro.txt")).unwrap().permissions();
    use std::os::unix::fs::PermissionsExt;
    perms.set_mode(0o444);
    std::fs::set_permissions(src.join("ro.txt"), perms).unwrap();

    // 3. symlink 指向帶 xattr 的檔案：backup 的 read_xattrs 會跟隨連結，
    //    把目標的 xattr 記到 symlink 條目上
    std::os::unix::fs::symlink("plain.txt", src.join("link")).unwrap();

    let repo = t.open().await;
    let summary = repo
        .backup(std::slice::from_ref(&src), backup_options())
        .await
        .unwrap();

    let target = t.dir.path().join("out");
    let restored = repo
        .restore(&summary.snapshot_key, &target, RestoreOptions::default())
        .await
        .unwrap();
    let root = target.join(src.strip_prefix("/").unwrap_or(&src));
    let get = |p: &std::path::Path, name: &str| {
        xattr::get(p, OsStr::new(name))
            .unwrap()
            .unwrap_or_else(|| panic!("xattr {name} missing on {}", p.display()))
    };
    // 1. 檔案與目錄的 xattr 原樣回來
    assert_eq!(get(&root.join("plain.txt"), "user.tag"), b"file-value");
    assert_eq!(get(&root.join("sub"), "user.dir-tag"), b"dir-value");
    // 2. 唯讀檔：xattr 有回來、mode 也是唯讀（順序正確的證據）
    let ro = root.join("ro.txt");
    assert_eq!(get(&ro, "user.locked"), b"yes");
    let mode = std::fs::metadata(&ro).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o444);
    // 3. symlink：restore 沒把它算錯誤，xattr 也不會寫到目標上
    assert!(
        restored.errors.iter().all(|e| !e.contains("link")),
        "symlink 的 xattr 警告不該變成錯誤：{:?}",
        restored.errors
    );
    assert_eq!(get(&root.join("plain.txt"), "user.tag"), b"file-value");
}

/// docs/format.md §9 釘的 stats 口徑：`dirs` 含來源目錄本身、不含合成根
/// tree；`files` 依名字計（hard link 各算一個）；`bytes` 同一份 hard link
/// 內容只算一次。Go 參考實作以同一組數字斷言（internal/repo/backup_test.go）。
#[cfg(unix)]
#[tokio::test]
async fn stats_count_dirs_without_root_tree_and_hard_link_bytes_once() {
    let t = TestRepo::new().await;
    let src = t.dir.path().join("src");
    std::fs::create_dir_all(src.join("sub")).unwrap();
    std::fs::create_dir_all(src.join("empty")).unwrap();
    let payload = random_bytes(7, 300 * 1024);
    std::fs::write(src.join("a.bin"), &payload).unwrap();
    std::fs::hard_link(src.join("a.bin"), src.join("b.bin")).unwrap();
    std::fs::write(src.join("sub").join("c.txt"), b"c").unwrap();
    std::os::unix::fs::symlink("a.bin", src.join("link")).unwrap();

    let repo = t.open().await;
    let s = repo
        .backup(std::slice::from_ref(&src), backup_options())
        .await
        .unwrap();
    // v3 口徑（§9.1）：dirs 是樹裡的目錄 entry 數，root 是 path 不是 entry
    // ——只有 sub、empty 兩個。
    assert_eq!(
        s.stats.dirs, 2,
        "sub, empty（root 本身不算）：{:?}",
        s.stats
    );
    assert_eq!(
        s.stats.files, 3,
        "a.bin、b.bin（hard link）、sub/c.txt：{:?}",
        s.stats
    );
    assert_eq!(s.stats.symlinks, 1, "{:?}", s.stats);
    assert_eq!(
        s.stats.bytes,
        payload.len() as u64 + 1,
        "hard link 內容只算一次：{:?}",
        s.stats
    );
}
