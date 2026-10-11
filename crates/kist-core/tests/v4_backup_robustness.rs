//! v4 backup 在「不乖」的輸入下的行為：讀不到的檔、備份中變動的檔、快速路徑的時間與大小判斷、parity 的範圍。
//! 搬自 v3 的 `backup_robustness.rs`（v4 的走訪是 v3 的複製）；快速路徑的兩個純函式測試原樣搬過來。v3 的兩個
//! 大檔快速路徑測試不搬：v4 沿用 chunk 清單時把它當成沿用的子樹（B4 檢查子樹的 parent，見 v4_backup_gc），
//! 沒有 v3 的 index blob 與 `chunks_read`。

#[path = "v4_common/mod.rs"]
mod v4_common;

#[cfg(unix)]
use kist_core::v4::check::CheckOptions;
#[cfg(unix)]
use kist_core::RestoreOptions;
use v4_common::*;

#[cfg(unix)]
const PLAIN: CheckOptions = CheckOptions {
    read_data: false,
    repair: false,
};

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
        t.repo_path().join(&s.snapshot).is_file(),
        "snapshot 仍然要寫出"
    );
    let target = t.dir.path().join("out");
    repo.restore(&skey(&s.snapshot), &target, RestoreOptions::default())
        .await
        .unwrap();
    let restored = landed(&target, &src);
    assert!(restored.join("small.txt").is_file());
    assert!(!restored.join("secret.txt").exists());
    assert!(restored.join("locked").is_dir(), "讀不到的目錄以空目錄記錄");
    let report = repo.check(PLAIN).await.unwrap();
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
    repo.restore(&skey(&s.snapshot), &target, RestoreOptions::default())
        .await
        .unwrap();
    let restored = std::fs::read(target.join("proc/version")).unwrap();
    assert_eq!(restored.len() as u64, s.stats.bytes);
    let report = repo.check(PLAIN).await.unwrap();
    assert!(report.errors.is_empty(), "{:?}", report.errors);
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

/// 快速路徑的「racily clean」守門用 parent 的**開始**時間（snapshot 的 time）：記錄的 ctime 不早於它的檔案，
/// stat 一樣也要重讀。第一個 backup 的時鐘撥早一小時，每個檔案記錄的 ctime 都不早於它的開始時間 → 第二個
/// backup 一個都不能沿用；第三個（parent 的開始時間晚於 ctime）全部沿用。接著把一個檔案的 mtime 設成剛好
/// 等於上一個 snapshot 的時間 → 那一個不能沿用（v3 的原測試；它單獨抓不到守門用錯時間：設 mtime 本身就讓
/// stat 不同，所以加上前半）。
#[tokio::test]
async fn fast_path_uses_parent_start_time_and_reports_reused_files() {
    let t = TestRepo::new().await;
    let src = t.dir.path().join("src");
    make_source(&src);
    let repo = t.open().await;
    let base = std::time::Instant::now();
    let mut early = backup_options();
    early.clock = kist_core::v4::Clock::new(move || kist_core::v4::Reading {
        wall: time::OffsetDateTime::now_utc() - time::Duration::hours(1),
        boot: base.elapsed(),
    });
    let s1 = repo
        .backup(std::slice::from_ref(&src), early)
        .await
        .unwrap();
    assert_eq!(s1.report.files_reused, 0);
    let s2 = repo
        .backup(std::slice::from_ref(&src), backup_options())
        .await
        .unwrap();
    assert_eq!(s2.parent.as_deref(), Some(s1.snapshot.as_str()));
    assert_eq!(
        s2.report.files_reused, 0,
        "ctime 不早於 parent 的開始時間：stat 一樣也要重讀：{:?}",
        s2.report
    );
    assert_eq!(s2.report.chunks_new, 0, "內容沒變，重讀也不寫新 chunk");
    let s3 = repo
        .backup(std::slice::from_ref(&src), backup_options())
        .await
        .unwrap();
    assert_eq!(s3.report.files_reused, s3.stats.files, "{:?}", s3.report);

    let snap3 = repo.read_snapshot(&skey(&s3.snapshot)).await.unwrap();
    let start = time::OffsetDateTime::from_unix_timestamp_nanos(i128::from(snap3.time)).unwrap();
    let at_start = filetime::FileTime::from_unix_time(start.unix_timestamp(), start.nanosecond());
    filetime::set_file_mtime(src.join("random.bin"), at_start).unwrap();

    let s4 = repo
        .backup(std::slice::from_ref(&src), backup_options())
        .await
        .unwrap();
    assert_eq!(
        s4.report.files_reused,
        s4.stats.files - 1,
        "mtime == parent 開始時間的檔案必須重讀：{:?}",
        s4.report
    );
    assert_eq!(s4.report.chunks_new, 0, "內容沒變，重讀也不寫新 chunk");
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
    let snap = repo.read_snapshot(&skey(&s.snapshot)).await.unwrap();
    let time = time::OffsetDateTime::from_unix_timestamp_nanos(i128::from(snap.time)).unwrap();
    assert!(before <= time && time <= after);
    let ts = s.snapshot.rsplit('/').next().unwrap();
    assert_eq!(
        kist_format::v4::keys::SnapTs::from_nanos(snap.time)
            .unwrap()
            .as_str(),
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

/// report.bytes_stored 必須涵蓋**全部**新寫的 chunk。對帳基準：新 repo 的第一個 backup，資料 pack 的
/// trailer 裡每一項的明文長度（`r`）總和 == bytes_stored（無去重、無殘留）。v4 的 chunk 清單是 metadata
/// item（在 mpack 裡），不算 chunk（v3 的這個測試抓的是清單 chunk 漏算）。
#[tokio::test]
async fn bytes_stored_counts_every_new_chunk() {
    let t = TestRepo::new().await;
    let src = t.dir.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    // 多到要 chunk 清單
    std::fs::write(src.join("big.bin"), random_bytes(77, 10 * 1024 * 1024)).unwrap();
    let repo = t.open().await;
    let s = repo
        .backup(std::slice::from_ref(&src), backup_options())
        .await
        .unwrap();
    assert!(s.report.chunks_new > 256, "{:?}", s.report);

    let chunk_max = repo.config().chunker.max;
    let mut trailer_raw = 0u64;
    for key in t.keys("packs").await {
        let bytes = repo.backend().get(&key).await.unwrap();
        let trailer =
            kist_core::v4::pack::trailer_of(repo.keys(), &skey(&key), &bytes, chunk_max).unwrap();
        trailer_raw += trailer.entries.iter().map(|e| e.r).sum::<u64>();
    }
    assert_eq!(
        s.report.bytes_stored, trailer_raw,
        "bytes_stored 要等於資料 pack 裡實存 chunk 的明文 bytes"
    );
}

/// ADR 019 A12：parity 份數超出 0..=MAX_PARITY_SHARDS 時，backup 在入口就回 Usage，repo 裡一個物件都不多。
/// 對照組：上限 8 仍然寫出 parity。
#[tokio::test]
async fn parity_out_of_range_is_rejected_before_writing_anything() {
    let t = TestRepo::new().await;
    let src = t.dir.path().join("src");
    make_source(&src);
    let repo = t.open().await;

    for m in [9u8, 255] {
        let before = walk_files(&t.repo_path());
        let mut opts = backup_options();
        opts.parity = m;
        let got = repo.backup(std::slice::from_ref(&src), opts).await;
        assert!(
            matches!(got, Err(kist_core::CoreError::Usage(_))),
            "parity {m} 應該回 Usage，實際：{got:?}"
        );
        assert_eq!(
            walk_files(&t.repo_path()),
            before,
            "parity {m} 被拒時不得寫任何物件"
        );
    }

    let mut opts = backup_options();
    opts.parity = 8;
    repo.backup(std::slice::from_ref(&src), opts).await.unwrap();
    assert!(t.count("parity") > 0, "parity 8 應該寫出 parity");
}
