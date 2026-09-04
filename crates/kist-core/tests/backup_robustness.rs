//! backup 在「不乖」的輸入下的行為：讀不到的檔、備份中變動的檔、index 遺失後的大檔快速路徑。

mod common;

use common::*;
use kist_core::{CheckOptions, RestoreOptions};

/// 讀不到的檔案：跳過、計數、snapshot 照寫（restic 的做法），其他檔案都在。
#[cfg(unix)]
#[tokio::test]
async fn unreadable_file_is_skipped_and_counted() {
    use std::os::unix::fs::PermissionsExt;
    if unsafe_is_root() {
        eprintln!("running as root, permission test is meaningless; skipped");
        return;
    }
    let t = TestRepo::new().await;
    let src = t.dir.path().join("src");
    make_source(&src);
    let secret = src.join("secret.txt");
    std::fs::write(&secret, b"top secret").unwrap();
    std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o000)).unwrap();
    let locked_dir = src.join("locked");
    std::fs::create_dir(&locked_dir).unwrap();
    std::fs::write(locked_dir.join("inside"), b"x").unwrap();
    std::fs::set_permissions(&locked_dir, std::fs::Permissions::from_mode(0o000)).unwrap();

    let repo = t.open().await;
    let s = repo
        .backup(std::slice::from_ref(&src), backup_options())
        .await
        .unwrap();
    // 收尾：把權限還回來，tempdir 才刪得掉
    std::fs::set_permissions(&locked_dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o644)).unwrap();

    assert_eq!(s.stats.errors, 2, "{:?}", s.stats);
    assert!(
        t.repo_path().join(&s.snapshot_key).is_file(),
        "snapshot 仍然要寫出"
    );
    let target = t.dir.path().join("out");
    repo.restore(&s.snapshot_key, &target, RestoreOptions::default())
        .await
        .unwrap();
    let restored = target.join(src.strip_prefix("/").unwrap_or(&src));
    assert!(restored.join("small.txt").is_file());
    assert!(!restored.join("secret.txt").exists());
    assert!(restored.join("locked").is_dir(), "讀不到的目錄以空目錄記錄");
    let report = repo.check(CheckOptions { read_data: false }).await.unwrap();
    assert!(report.errors.is_empty(), "{:?}", report.errors);
}

#[cfg(unix)]
fn unsafe_is_root() -> bool {
    std::fs::metadata("/proc/self")
        .map(|m| {
            use std::os::unix::fs::MetadataExt;
            m.uid() == 0
        })
        .unwrap_or(false)
}

/// metadata 說 0 bytes、實際讀出 100+ bytes 的檔（procfs），代表「備份中被改變大小的檔」。
/// size 必須用實際讀到的長度，restore 才不會把它當成損毀。
#[cfg(target_os = "linux")]
#[tokio::test]
async fn file_whose_size_changes_while_reading_restores_correctly() {
    let t = TestRepo::new().await;
    let repo = t.open().await;
    let path = std::path::PathBuf::from("/proc/version");
    let s = repo
        .backup(std::slice::from_ref(&path), backup_options())
        .await
        .unwrap();
    assert!(s.stats.bytes_total > 0, "{:?}", s.stats);
    let target = t.dir.path().join("out");
    repo.restore(&s.snapshot_key, &target, RestoreOptions::default())
        .await
        .unwrap();
    let restored = std::fs::read(target.join("proc/version")).unwrap();
    assert_eq!(restored.len() as u64, s.stats.bytes_total);
    let report = repo.check(CheckOptions { read_data: false }).await.unwrap();
    assert!(report.errors.is_empty(), "{:?}", report.errors);
}

/// 大檔（Indirect）：資料 chunk 從 index 消失、清單 chunk 還在時，快速路徑不能沿用。
#[tokio::test]
async fn indirect_fast_path_verifies_data_chunks_not_just_the_list() {
    let t = TestRepo::new().await;
    let src = t.dir.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    let big = src.join("big.bin");
    let mut data = random_bytes(21, 8 * 1024 * 1024);
    std::fs::write(&big, &data).unwrap();
    let repo = t.open().await;
    let s1 = repo
        .backup(std::slice::from_ref(&src), backup_options())
        .await
        .unwrap();
    assert!(s1.stats.chunks_total > 256, "要是 Indirect：{:?}", s1.stats);

    // append 1 byte：資料 chunk 幾乎全部重用（在 index blob 1），新清單 chunk 在 blob 2
    data.push(7);
    std::fs::write(&big, &data).unwrap();
    repo.backup(std::slice::from_ref(&src), backup_options())
        .await
        .unwrap();
    let mut blobs = walk_files(&t.repo_path().join("indexes"));
    blobs.sort_by_key(|p| std::fs::metadata(p).unwrap().modified().unwrap());
    assert_eq!(blobs.len(), 2);
    std::fs::remove_file(&blobs[0]).unwrap(); // 舊 blob（含資料 chunk）遺失

    let s3 = repo
        .backup(std::slice::from_ref(&src), backup_options())
        .await
        .unwrap();
    assert!(
        s3.stats.chunks_new > 100,
        "資料 chunk 不在 index 裡，必須重讀重傳：{:?}",
        s3.stats
    );
    let target = t.dir.path().join("out");
    repo.restore(&s3.snapshot_key, &target, RestoreOptions::default())
        .await
        .unwrap();
    let restored = target.join(src.strip_prefix("/").unwrap_or(&src));
    assert_eq!(std::fs::read(restored.join("big.bin")).unwrap(), data);
}

/// 大檔的 chunks_total 要算資料 chunk，不是清單 chunk。
#[tokio::test]
async fn indirect_fast_path_counts_data_chunks() {
    let t = TestRepo::new().await;
    let src = t.dir.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("big.bin"), random_bytes(22, 8 * 1024 * 1024)).unwrap();
    let repo = t.open().await;
    let s1 = repo
        .backup(std::slice::from_ref(&src), backup_options())
        .await
        .unwrap();
    let s2 = repo
        .backup(std::slice::from_ref(&src), backup_options())
        .await
        .unwrap();
    assert_eq!(
        s2.stats.chunks_total, s1.stats.chunks_total,
        "{:?} vs {:?}",
        s1.stats, s2.stats
    );
}

/// 「racily clean」：ctime 不早於上一次 backup 開始時間的檔案不能走快速路徑。
#[test]
fn fast_path_rejects_files_changed_at_or_after_parent_start() {
    use kist_core::fsmeta::unchanged;
    use kist_format::tree::NodeMeta;
    let meta = NodeMeta {
        mtime_secs: 1000,
        mtime_nanos: 0,
        ctime_secs: 1000,
        ctime_nanos: 0,
        inode: 5,
        ..NodeMeta::default()
    };
    assert!(
        unchanged(&meta, &meta, (2000, 0)),
        "早於 parent 開始時間：可沿用"
    );
    assert!(!unchanged(&meta, &meta, (1000, 0)), "同一秒：不可沿用");
    assert!(
        !unchanged(&meta, &meta, (500, 0)),
        "晚於 parent 開始：不可沿用"
    );
    let no_ctime = NodeMeta {
        ctime_secs: 0,
        ctime_nanos: 0,
        ..meta
    };
    assert!(
        !unchanged(&no_ctime, &no_ctime, (500, 0)),
        "沒有 ctime 就看 mtime，同樣不可沿用"
    );
}
