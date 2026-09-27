//! ADR 019 A4：restore 以目錄 handle 為錨逐層走，同時開著的 handle 數以 tree
//! 深度為上限（每層一個），與 entry 數無關。硬連結表記的是相對路徑、不是
//! handle，到時再從目標逐層重新開。
//!
//! 單獨一個測試檔：這裡把整個 process 的 RLIMIT_NOFILE 調低，不能和其他
//! 測試共用 process（`cargo test` 同一個檔的測試在同一個 process 的多條
//! 執行緒上跑）。

// RLIMIT_NOFILE 與目錄 fd 都是 unix 的事；非 unix 的 restore 不開目錄 handle。
#![cfg(unix)]

mod common;

use common::*;
use kist_core::RestoreOptions;

/// 300 個目錄、每個目錄一對硬連結（硬連結表 300 筆）；還原期間 fd 上限只有
/// 64。handle 要是跟著目錄或硬連結表的筆數累積（例如表裡存目錄 handle），
/// 還原到一半就會 EMFILE。
#[tokio::test]
async fn restore_fd_usage_does_not_grow_with_entry_count() {
    use rustix::process::{getrlimit, setrlimit, Resource, Rlimit};
    use std::os::unix::fs::MetadataExt;

    const DIRS: usize = 300;
    const FD_LIMIT: u64 = 64;
    let t = TestRepo::new().await;
    let src = t.dir.path().join("src");
    for i in 0..DIRS {
        let d = src.join(format!("d{i:03}"));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("a"), format!("file {i}")).unwrap();
        std::fs::hard_link(d.join("a"), d.join("b")).unwrap();
    }
    let repo = t.open().await;
    let s = repo
        .backup(std::slice::from_ref(&src), backup_options())
        .await
        .unwrap();
    let target = t.dir.path().join("out");

    let original = getrlimit(Resource::Nofile);
    let lowered = Rlimit {
        current: Some(FD_LIMIT),
        maximum: original.maximum,
    };
    setrlimit(Resource::Nofile, lowered).unwrap();
    let outcome = repo
        .restore(&s.snapshot_key, &target, RestoreOptions::default())
        .await;
    setrlimit(Resource::Nofile, original).unwrap();
    let summary = outcome.unwrap();

    let errors = &summary.errors[..summary.errors.len().min(3)];
    assert!(
        summary.errors.is_empty(),
        "fd 上限 {FD_LIMIT} 下還原出錯 {} 筆，例如 {errors:?}",
        summary.errors.len()
    );
    assert_eq!(summary.files, 2 * DIRS as u64);
    let restored = target.join(src.strip_prefix("/").unwrap_or(&src));
    for i in [0, DIRS - 1] {
        let d = restored.join(format!("d{i:03}"));
        let a = std::fs::symlink_metadata(d.join("a")).unwrap();
        let b = std::fs::symlink_metadata(d.join("b")).unwrap();
        assert_eq!((a.ino(), a.nlink()), (b.ino(), 2), "d{i:03} 的硬連結斷了");
    }
}
