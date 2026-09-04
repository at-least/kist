//! 對 S3 相容服務（MinIO）跑完整流程。啟用方式見 `crates/kist-backend/tests/s3.rs`。

mod common;

use common::*;
use kist_backend::Backend;
use kist_core::{CheckOptions, Repository, RestoreOptions};

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
        for (key, _) in backend.list(prefix).await.unwrap() {
            backend.delete(&key).await.unwrap();
        }
    }
    backend.delete("config").await.unwrap();
}
