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

    assert_eq!(s.report.errors, 2, "{:?}", s.report);
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
    let report = repo
        .check(CheckOptions {
            read_data: false,
            repair: false,
        })
        .await
        .unwrap();
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
    assert!(s.stats.bytes > 0, "{:?}", s.stats);
    let target = t.dir.path().join("out");
    repo.restore(&s.snapshot_key, &target, RestoreOptions::default())
        .await
        .unwrap();
    let restored = std::fs::read(target.join("proc/version")).unwrap();
    assert_eq!(restored.len() as u64, s.stats.bytes);
    let report = repo
        .check(CheckOptions {
            read_data: false,
            repair: false,
        })
        .await
        .unwrap();
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
    assert!(s1.report.chunks_new > 256, "要是 Indirect：{:?}", s1.report);

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
        s3.report.chunks_new > 100,
        "資料 chunk 不在 index 裡，必須重讀重傳：{:?}",
        s3.report
    );
    let target = t.dir.path().join("out");
    repo.restore(&s3.snapshot_key, &target, RestoreOptions::default())
        .await
        .unwrap();
    let restored = target.join(src.strip_prefix("/").unwrap_or(&src));
    assert_eq!(std::fs::read(restored.join("big.bin")).unwrap(), data);
}

/// 大檔的快速路徑要算**資料** chunk 的數量（`chunks_read`），不是只算清單 chunk。
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
    assert!(s1.report.chunks_new > 256, "要是 Indirect：{:?}", s1.report);
    let s2 = repo
        .backup(std::slice::from_ref(&src), backup_options())
        .await
        .unwrap();
    // 沿用時 `chunks_read` 數的是解開清單後的資料 chunk（> 256 個），
    // 不是樹裡那 1-2 個清單 chunk
    assert!(
        s2.report.chunks_read > 256,
        "{:?} vs {:?}",
        s1.report,
        s2.report
    );
    assert_eq!(s2.report.chunks_new, 0, "{:?}", s2.report);
}

/// 「racily clean」：ctime 不早於上一次 backup 開始時間的檔案不能走快速路徑。
#[test]
fn fast_path_rejects_files_changed_at_or_after_parent_start() {
    use kist_core::fsmeta::{unchanged, FsMeta};
    let meta = FsMeta {
        mode: 0o100644,
        uid: 0,
        gid: 0,
        mtime_ns: 1_000_000_000_000,
        ctime_ns: 1_000_000_000_000,
        inode: 5,
        dev: 0,
        nlink: 0,
    };
    assert!(
        unchanged(&meta, &meta, 2_000_000_000_000),
        "早於 parent 開始時間：可沿用"
    );
    assert!(
        !unchanged(&meta, &meta, 1_000_000_000_000),
        "同一瞬間：不可沿用"
    );
    assert!(
        !unchanged(&meta, &meta, 500_000_000_000),
        "晚於 parent 開始：不可沿用"
    );
    let no_ctime = FsMeta {
        ctime_ns: 0,
        ..meta
    };
    assert!(
        !unchanged(&no_ctime, &no_ctime, 500_000_000_000),
        "沒有 ctime 就看 mtime，同樣不可沿用"
    );
}

/// snapshot 的時間必須是 backup 的**開始**時間：快速路徑用它判斷「檔案早於上次備份就沒再動過」。
/// 把一個檔案的 mtime 設成剛好等於上一個 snapshot 的時間 → 不能沿用。
#[tokio::test]
async fn fast_path_uses_parent_start_time_and_reports_reused_files() {
    let t = TestRepo::new().await;
    let src = t.dir.path().join("src");
    make_source(&src);
    let repo = t.open().await;
    let s1 = repo
        .backup(std::slice::from_ref(&src), backup_options())
        .await
        .unwrap();
    assert_eq!(s1.report.files_reused, 0);
    let s2 = repo
        .backup(std::slice::from_ref(&src), backup_options())
        .await
        .unwrap();
    assert_eq!(s2.report.files_reused, s2.stats.files, "{:?}", s2.report);

    let snap2 = repo.read_snapshot_by_key(&s2.snapshot_key).await.unwrap();
    let start = time::OffsetDateTime::from_unix_timestamp_nanos(snap2.time_ns as i128).unwrap();
    let at_start = filetime::FileTime::from_unix_time(start.unix_timestamp(), start.nanosecond());
    filetime::set_file_mtime(src.join("random.bin"), at_start).unwrap();

    let s3 = repo
        .backup(std::slice::from_ref(&src), backup_options())
        .await
        .unwrap();
    assert_eq!(
        s3.report.files_reused,
        s3.stats.files - 1,
        "mtime == parent 開始時間的檔案必須重讀：{:?}",
        s3.report
    );
    assert_eq!(s3.report.chunks_new, 0, "內容沒變，重讀也不寫新 chunk");
}

/// snapshot 內容的 `time` 與 key 裡的時間戳必須來自同一個瞬間（開始時間）。
#[tokio::test]
async fn snapshot_time_is_backup_start_and_matches_key() {
    let t = TestRepo::new().await;
    let src = t.dir.path().join("src");
    make_source(&src);
    let repo = t.open().await;
    let before = time::OffsetDateTime::now_utc();
    let s = repo
        .backup(std::slice::from_ref(&src), backup_options())
        .await
        .unwrap();
    let after = time::OffsetDateTime::now_utc();
    let snap = repo.read_snapshot_by_key(&s.snapshot_key).await.unwrap();
    let time = time::OffsetDateTime::from_unix_timestamp_nanos(snap.time_ns as i128).unwrap();
    assert!(before <= time && time <= after);
    let ts = s.snapshot_key.rsplit('/').next().unwrap();
    assert_eq!(
        kist_format::snapshot::format_key_timestamp(time).unwrap(),
        ts
    );
}

/// 沒有 ctime / inode 的平台（Windows）只剩 size + mtime：size 不同就不能沿用。
#[test]
fn fast_path_compares_size_even_without_ctime() {
    use kist_core::fsmeta::{file_unchanged, FsMeta};
    let meta = FsMeta {
        mode: 0,
        uid: 0,
        gid: 0,
        mtime_ns: 1_000_000_000_000,
        ctime_ns: 0,
        inode: 0,
        dev: 0,
        nlink: 0,
    };
    assert!(file_unchanged(&meta, 10, &meta, 10, 2_000_000_000_000));
    assert!(
        !file_unchanged(&meta, 10, &meta, 11, 2_000_000_000_000),
        "size 變了不能沿用"
    );
}

/// report.bytes_stored 必須涵蓋**全部**新寫的 chunk——間接內容的清單
/// chunk 也是這次存的 bytes。對帳基準：fresh repo 的第一個 backup，
/// 所有 pack trailer 的 entry 長度總和 == bytes_stored（無去重、無殘留）。
#[tokio::test]
async fn bytes_stored_counts_the_indirect_chunk_list_too() {
    let t = TestRepo::new().await;
    let src = t.dir.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    // >256 chunks（avg 16 KiB）：清單必須走間接。
    std::fs::write(src.join("big.bin"), random_bytes(77, 10 * 1024 * 1024)).unwrap();
    let repo = t.open().await;
    let s = repo
        .backup(std::slice::from_ref(&src), backup_options())
        .await
        .unwrap();

    let keys = repo.keys().clone();
    // bytes_stored 的語義是**明文** bytes 新存（chunk.len() 的累加），
    // 對帳基準用 trailer 的 raw_len。
    let mut trailer_raw = 0u64;
    for pack in walk_files(&t.repo_path().join("packs")) {
        let bytes = std::fs::read(&pack).unwrap();
        let trailer = kist_core::pack::read_trailer(&keys, &bytes).unwrap();
        trailer_raw += trailer.entries.iter().map(|e| e.raw_len).sum::<u64>();
    }
    assert_eq!(
        s.report.bytes_stored, trailer_raw,
        "bytes_stored 要等於 pack 裡實存 chunk 的明文 bytes（含間接清單 chunk）"
    );
}
