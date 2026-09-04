//! 對 S3 相容服務（MinIO）跑完整流程。啟用方式見 `crates/kist-backend/tests/s3.rs`。

mod common;

use common::*;
use kist_backend::Backend;
use kist_core::{
    BackupOptions, CheckOptions, ForgetOptions, PruneOptions, Repository, RestoreOptions,
};

fn s3_backend(test: &str) -> Option<Backend> {
    let endpoint = std::env::var("KIST_TEST_S3_ENDPOINT").ok();
    let bucket = std::env::var("KIST_TEST_S3_BUCKET").ok();
    let (Some(endpoint), Some(bucket)) = (endpoint, bucket) else {
        eprintln!("KIST_TEST_S3_ENDPOINT / KIST_TEST_S3_BUCKET not set; S3 test skipped");
        return None;
    };
    std::env::set_var("AWS_ENDPOINT", endpoint);
    std::env::set_var("AWS_ALLOW_HTTP", "true");
    if std::env::var("AWS_DEFAULT_REGION").is_err() {
        std::env::set_var("AWS_DEFAULT_REGION", "us-east-1");
    }
    let prefix = format!("core-{test}-{}", std::process::id());
    Some(Backend::from_url(&format!("s3://{bucket}/{prefix}")).unwrap())
}

#[tokio::test]
async fn backup_restore_check_on_s3() {
    let Some(backend) = s3_backend("e2e") else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    make_source(&src);
    Repository::init(backend.clone(), PASSWORD.as_bytes(), init_options())
        .await
        .unwrap();
    let repo = Repository::open(backend.clone(), PASSWORD.as_bytes())
        .await
        .unwrap();
    let s1 = repo
        .backup(std::slice::from_ref(&src), backup_options())
        .await
        .unwrap();
    assert!(s1.stats.packs_new >= 2);
    let s2 = repo
        .backup(std::slice::from_ref(&src), backup_options())
        .await
        .unwrap();
    assert_eq!(s2.stats.packs_new, 0);
    assert_eq!(s2.stats.files_reused, s2.stats.files);

    let target = dir.path().join("out");
    let summary = repo
        .restore(&s2.snapshot_key, &target, RestoreOptions::default())
        .await
        .unwrap();
    assert!(summary.errors.is_empty(), "{summary:?}");
    assert_same_tree(&src, &target.join(src.strip_prefix("/").unwrap_or(&src)));

    let report = repo.check(CheckOptions { read_data: true }).await.unwrap();
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    assert_eq!(report.snapshots, 2);

    // 收尾：刪掉這次的物件（測試 prefix 是隨機的，留著也不影響別的測試）
    for prefix in ["packs", "trees", "indexes", "snapshots"] {
        for o in backend.list(prefix).await.unwrap() {
            backend.delete(&o.key).await.unwrap();
        }
    }
    backend.delete("config").await.unwrap();
}

/// M2 驗收：只有 Put/Get/List 權限的使用者能完成整個 backup（含 snapshot 的 conditional put）。
#[tokio::test]
async fn put_only_user_completes_a_backup() {
    let Some(root_backend) = s3_backend("putonly") else {
        return;
    };
    let (Some(key), Some(secret)) = (
        std::env::var("KIST_TEST_S3_PUTONLY_KEY").ok(),
        std::env::var("KIST_TEST_S3_PUTONLY_SECRET").ok(),
    ) else {
        eprintln!("KIST_TEST_S3_PUTONLY_KEY / _SECRET not set; skipped");
        return;
    };
    // 管理者建 repo
    Repository::init(root_backend.clone(), PASSWORD.as_bytes(), init_options())
        .await
        .unwrap();
    // 備份機器只有受限帳號
    let kist_backend::RepoLocation::S3 { bucket, prefix } = root_backend.location().clone() else {
        panic!("not s3");
    };
    let limited = Backend::s3_with_credentials(&bucket, &prefix, &key, &secret).unwrap();
    let repo = Repository::open(limited.clone(), PASSWORD.as_bytes())
        .await
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    make_source(&src);
    let s = repo
        .backup(std::slice::from_ref(&src), backup_options())
        .await
        .unwrap();
    assert!(s.stats.packs_new > 0);
    let s2 = repo
        .backup(std::slice::from_ref(&src), backup_options())
        .await
        .unwrap();
    assert_eq!(s2.stats.chunks_new, 0);
    // 受限帳號也能 check（只需 Get / List）
    let report = repo.check(CheckOptions { read_data: true }).await.unwrap();
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    // 但刪不掉任何東西
    assert!(limited.delete("config").await.is_err());
    assert!(root_backend.exists("config").await.unwrap());
}

/// M3 對 S3：標記（conditional put）、LastModified 當時間、刪除；用 grace 0 讓兩階段在幾秒內走完。
#[tokio::test]
async fn forget_and_prune_on_s3() {
    let Some(backend) = s3_backend("prune") else {
        return;
    };
    Repository::init(backend.clone(), PASSWORD.as_bytes(), init_options())
        .await
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    make_source(&src);
    std::fs::write(src.join("zz-big.bin"), random_bytes(78, 900 * 1024)).unwrap();
    let repo = Repository::open(backend.clone(), PASSWORD.as_bytes())
        .await
        .unwrap();
    // backup 的 gc_grace 維持預設（0 會讓每個 backup 都被當成「跑超過 grace」）；prune 用 grace 0
    let opts: BackupOptions = backup_options();
    let b1 = repo
        .backup(std::slice::from_ref(&src), opts.clone())
        .await
        .unwrap();
    std::fs::remove_file(src.join("zz-big.bin")).unwrap();
    repo.backup(std::slice::from_ref(&src), opts.clone())
        .await
        .unwrap();
    let packs_before = backend.list("packs").await.unwrap().len();
    repo.forget(ForgetOptions {
        snapshots: vec![b1.snapshot_key],
        policy: Default::default(),
        dry_run: false,
        now: None,
    })
    .await
    .unwrap();
    let zero = PruneOptions {
        grace: std::time::Duration::ZERO,
        ..PruneOptions::default()
    };
    let p1 = repo.prune(zero.clone()).await.unwrap();
    assert!(p1.marked > 0 && p1.deleted == 0, "{p1:?}");
    assert_eq!(backend.list("gc").await.unwrap().len() as u64, p1.marked);
    // 活躍 client 在標記後要有新 snapshot（S3 的 LastModified 是秒級：等一秒再備份）
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    repo.backup(std::slice::from_ref(&src), opts).await.unwrap();
    let p2 = repo.prune(zero).await.unwrap();
    assert!(p2.deleted > 0, "{p2:?}");
    assert!(backend.list("packs").await.unwrap().len() < packs_before + p1.new_packs as usize + 1);
    let fresh = Repository::open(backend.clone(), PASSWORD.as_bytes())
        .await
        .unwrap();
    let report = fresh.check(CheckOptions { read_data: true }).await.unwrap();
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    let target = dir.path().join("out");
    let r = fresh
        .restore("latest", &target, RestoreOptions::default())
        .await;
    assert!(r.is_err() || r.as_ref().unwrap().errors.is_empty());
    for prefix in ["packs", "trees", "indexes", "snapshots", "gc"] {
        for o in backend.list(prefix).await.unwrap() {
            backend.delete(&o.key).await.unwrap();
        }
    }
    backend.delete("config").await.unwrap();
}
