//! restore 在「不理想」情況下的行為：重複還原、互相包含的來源、壞掉的 chunk、惡意名稱。

mod common;

use common::*;
use kist_core::RestoreOptions;

/// 第二次 restore 到同一個目錄（裡面已有 symlink）必須成功，結果仍然正確。
#[cfg(unix)]
#[tokio::test]
async fn restore_twice_into_same_target_succeeds() {
    let t = TestRepo::new().await;
    let src = t.dir.path().join("src");
    make_source(&src); // 含 link -> small.txt
    let repo = t.open().await;
    let s = repo
        .backup(std::slice::from_ref(&src), backup_options())
        .await
        .unwrap();
    let target = t.dir.path().join("out");
    repo.restore(&s.snapshot_key, &target, RestoreOptions::default())
        .await
        .unwrap();
    repo.restore(&s.snapshot_key, &target, RestoreOptions::default())
        .await
        .unwrap();
    let restored = target.join(src.strip_prefix("/").unwrap_or(&src));
    assert_same_tree(&src, &restored);
}

/// 來源 `/a` 與 `/a/b` 互相包含：只保留外層，restore 不會因為重建同一條路徑而失敗。
#[tokio::test]
async fn nested_source_paths_are_deduplicated() {
    let t = TestRepo::new().await;
    let a = t.dir.path().join("a");
    let b = a.join("b");
    std::fs::create_dir_all(&b).unwrap();
    std::fs::write(a.join("x"), b"x").unwrap();
    std::fs::write(b.join("y"), b"y").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink("y", b.join("link")).unwrap();
    let repo = t.open().await;
    let s = repo
        .backup(&[b.clone(), a.clone()], backup_options())
        .await
        .unwrap();
    assert_eq!(s.stats.files, 2, "b 在 a 裡面，只該算一次：{:?}", s.stats);
    let target = t.dir.path().join("out");
    repo.restore(&s.snapshot_key, &target, RestoreOptions::default())
        .await
        .unwrap();
    assert_same_tree(&a, &target.join(a.strip_prefix("/").unwrap_or(&a)));
}

/// 一個 chunk 壞掉：那個檔案失敗、其餘照常還原，錯誤在 summary 裡而不是整個 restore 中止。
#[tokio::test]
async fn corrupt_chunk_fails_only_that_file() {
    let t = TestRepo::new().await;
    let src = t.dir.path().join("src");
    make_source(&src);
    let repo = t.open().await;
    let s = repo
        .backup(std::slice::from_ref(&src), backup_options())
        .await
        .unwrap();
    // 翻掉每個 pack 的一個 byte（entry 區域），保證有檔案受影響
    for pack in walk_files(&t.repo_path().join("packs")) {
        if pack.is_file() {
            let mut bytes = std::fs::read(&pack).unwrap();
            bytes[40] ^= 0xff;
            std::fs::write(&pack, bytes).unwrap();
        }
    }
    let target = t.dir.path().join("out");
    let summary = repo
        .restore(&s.snapshot_key, &target, RestoreOptions::default())
        .await
        .unwrap();
    assert!(!summary.errors.is_empty(), "{summary:?}");
    let restored = target.join(src.strip_prefix("/").unwrap_or(&src));
    // 目錄結構與空檔一定還原得了
    assert!(restored.join("sub/deeper").is_dir());
    assert!(restored.join("empty.txt").is_file());
    // 用 symlink_metadata：`is_file()` 會跟隨 symlink，把 link 也算成檔案
    let files_ok = walk_files(&restored)
        .iter()
        .filter(|p| std::fs::symlink_metadata(p).unwrap().is_file())
        .count() as u64;
    assert_eq!(
        files_ok + summary.errors.len() as u64,
        s.stats.files,
        "還原成功 + 失敗 = 全部檔案；{summary:?}"
    );
}

/// tree 裡的子節點名稱由 repo 內容決定；含分隔符或 `..` 的名稱不能讓 restore 逃出目標目錄。
#[test]
fn child_names_that_escape_are_rejected() {
    use kist_core::fsmeta::validate_child_name;
    for bad in [&b""[..], b".", b"..", b"a/b", b"a\0b", b"/etc/passwd"] {
        assert!(validate_child_name(bad).is_err(), "{bad:?} 應該被拒絕");
    }
    // 反斜線在 Unix 是合法檔名，在 Windows 是分隔符
    assert_eq!(validate_child_name(b"a\\b").is_err(), cfg!(windows));
    for good in [
        &b"a"[..],
        b"..a",
        b"a..",
        "中文.txt".as_bytes(),
        b"weird name with spaces",
    ] {
        assert!(validate_child_name(good).is_ok(), "{good:?} 應該可以");
    }
}
