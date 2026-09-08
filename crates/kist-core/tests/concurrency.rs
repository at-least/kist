//! 兩台 client 同時 backup 到同一個 repo：不能互相破壞，而且事後彼此的資料要能去重。

mod common;

use std::path::Path;

use common::*;
use kist_backend::Backend;
use kist_core::{BackupOptions, CheckOptions, Repository, RestoreOptions};

/// 兩份有重疊的來源：shared/ 一樣，各自再有自己的檔案。
fn make_overlapping_sources(root: &Path) -> (std::path::PathBuf, std::path::PathBuf) {
    let a = root.join("a");
    let b = root.join("b");
    for src in [&a, &b] {
        std::fs::create_dir_all(src.join("shared")).unwrap();
        std::fs::write(src.join("shared/common.bin"), random_bytes(51, 400 * 1024)).unwrap();
        std::fs::write(src.join("shared/text.txt"), "the same text\n".repeat(2000)).unwrap();
    }
    std::fs::write(a.join("only-a.bin"), random_bytes(52, 300 * 1024)).unwrap();
    std::fs::write(b.join("only-b.bin"), random_bytes(53, 300 * 1024)).unwrap();
    (a, b)
}

fn client(id: u8) -> BackupOptions {
    BackupOptions {
        client_id: [id; 16],
        hostname: format!("host{id}"),
        username: "tester".to_owned(),
        now: None,
        gc_grace: kist_core::DEFAULT_GC_GRACE,
        parity: 0,
        progress: None,
    }
}

/// 對任何後端跑同一套劇本。
async fn concurrent_scenario(backend: Backend, work: &Path) {
    Repository::init(backend.clone(), PASSWORD.as_bytes(), init_options())
        .await
        .unwrap();
    let (src_a, src_b) = make_overlapping_sources(work);
    let repo_a = Repository::open(backend.clone(), PASSWORD.as_bytes())
        .await
        .unwrap();
    let repo_b = Repository::open(backend.clone(), PASSWORD.as_bytes())
        .await
        .unwrap();

    // 同時跑
    let (ra, rb) = tokio::join!(
        repo_a.backup(std::slice::from_ref(&src_a), client(0xA)),
        repo_b.backup(std::slice::from_ref(&src_b), client(0xB)),
    );
    let (sa, sb) = (ra.unwrap(), rb.unwrap());
    assert_ne!(sa.snapshot_key, sb.snapshot_key);
    assert!(sa.report.chunks_new > 0 && sb.report.chunks_new > 0);

    // repo 一致
    let fresh = Repository::open(backend.clone(), PASSWORD.as_bytes())
        .await
        .unwrap();
    let report = fresh
        .check(CheckOptions {
            read_data: true,
            repair: false,
        })
        .await
        .unwrap();
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    assert_eq!(report.snapshots, 2);

    // 兩邊都能 byte-for-byte 還原
    for (key, src, name) in [
        (&sa.snapshot_key, &src_a, "out-a"),
        (&sb.snapshot_key, &src_b, "out-b"),
    ] {
        let target = work.join(name);
        let summary = fresh
            .restore(key, &target, RestoreOptions::default())
            .await
            .unwrap();
        assert!(summary.errors.is_empty(), "{summary:?}");
        assert_same_tree(src, &target.join(src.strip_prefix("/").unwrap_or(src)));
    }

    // A 之後備份 B 的資料：B 上傳過的 chunk 都在（跨 client 的 index 合併有效）
    let sa2 = fresh
        .backup(std::slice::from_ref(&src_b), client(0xA))
        .await
        .unwrap();
    assert_eq!(sa2.report.chunks_new, 0, "{:?}", sa2.report);
    assert_eq!(sa2.report.packs_new, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_clients_backup_concurrently_local() {
    let dir = tempfile::tempdir().unwrap();
    let backend = Backend::local(&dir.path().join("repo")).unwrap();
    concurrent_scenario(backend, dir.path()).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_clients_backup_concurrently_s3() {
    let (Some(endpoint), Some(bucket)) = (
        std::env::var("KIST_TEST_S3_ENDPOINT").ok(),
        std::env::var("KIST_TEST_S3_BUCKET").ok(),
    ) else {
        eprintln!("S3 env not set; skipped");
        return;
    };
    std::env::set_var("AWS_ENDPOINT", endpoint);
    std::env::set_var("AWS_ALLOW_HTTP", "true");
    if std::env::var("AWS_DEFAULT_REGION").is_err() {
        std::env::set_var("AWS_DEFAULT_REGION", "us-east-1");
    }
    let dir = tempfile::tempdir().unwrap();
    let backend = Backend::from_url(&format!("s3://{bucket}/concurrent-{}", std::process::id()))
        .await
        .unwrap();
    concurrent_scenario(backend, dir.path()).await;
}

/// 多次重跑同一劇本，抓偶發的競態。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_clients_concurrently_repeated() {
    for round in 0..5 {
        let dir = tempfile::tempdir().unwrap();
        let backend = Backend::local(&dir.path().join(format!("repo{round}"))).unwrap();
        concurrent_scenario(backend, dir.path()).await;
    }
}
