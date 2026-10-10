//! v4 backup 在真的後端上：init、backup、讀回、沒變的第二次 backup 不寫新東西。MinIO、SFTP、rclone 有環境
//! 變數才跑（tests/README.md）；本機與記憶體後端的規則測試在 `v4_backup*.rs`。

#[path = "v4_common/mod.rs"]
mod v4_common;

use kist_backend::Backend;
use kist_core::v4::Repo;
use v4_common::*;

async fn round_trip(backend: Backend) {
    let dir = tempfile::tempdir().unwrap();
    Repo::init(backend.clone(), PASSWORD, init_options(0))
        .await
        .unwrap();
    let cache = dir.path().join("cache");
    let repo = Repo::open(backend.clone(), PASSWORD, Some(&cache))
        .await
        .unwrap();
    let src = dir.path().join("src");
    sample_source(&src);
    let first = repo
        .backup(std::slice::from_ref(&src), opts(1))
        .await
        .unwrap();
    let mut expected = std::collections::BTreeMap::new();
    let segs: Vec<String> = kist_format::v4::snapshot::split_locator(
        src.canonicalize().unwrap().as_os_str().as_encoded_bytes(),
    )
    .unwrap()
    .into_iter()
    .map(|s| String::from_utf8(s).unwrap())
    .collect();
    for (p, n) in source_tree(&src) {
        expected.insert(format!("{}/{p}", segs.join("/")), n);
    }
    assert_eq!(read_tree(&repo, &first.snapshot).await, expected);
    let second = repo.backup(&[src], opts(1)).await.unwrap();
    assert_eq!(second.parent.as_deref(), Some(first.snapshot.as_str()));
    assert_eq!(
        second.report.packs_new + second.report.mpacks_new + second.report.deltas_new,
        0,
        "{:?}",
        second.report
    );
    // 清掉（只有測試用的前綴）
    for prefix in ["keys", "packs", "mpacks", "deltas", "snapshots", "maint"] {
        for o in backend.list(prefix).await.unwrap() {
            backend.delete(&o.key).await.unwrap();
        }
    }
    backend.delete("config").await.unwrap();
}

#[tokio::test]
async fn s3_v4_backup_round_trips() {
    let (Some(endpoint), Some(bucket)) = (
        std::env::var("KIST_TEST_S3_ENDPOINT").ok(),
        std::env::var("KIST_TEST_S3_BUCKET").ok(),
    ) else {
        eprintln!("S3 v4 backup test skipped");
        return;
    };
    std::env::set_var("AWS_ENDPOINT", endpoint);
    std::env::set_var("AWS_ALLOW_HTTP", "true");
    if std::env::var("AWS_DEFAULT_REGION").is_err() {
        std::env::set_var("AWS_DEFAULT_REGION", "us-east-1");
    }
    let b = Backend::from_url(&format!("s3://{bucket}/v4-backup-{}", std::process::id()))
        .await
        .unwrap();
    round_trip(b).await;
}

#[tokio::test]
async fn sftp_v4_backup_round_trips() {
    let (Some(url), Some(password), Some(known_hosts)) = (
        std::env::var("KIST_TEST_SFTP_URL").ok(),
        std::env::var("KIST_TEST_SFTP_PASSWORD").ok(),
        std::env::var("KIST_TEST_SFTP_KNOWN_HOSTS").ok(),
    ) else {
        eprintln!("SFTP v4 backup test skipped (tests/sftp-setup.sh 未跑)");
        return;
    };
    let mut cfg = kist_backend::sftp::parse_sftp_url(&url).unwrap();
    cfg.path = format!("{}/v4-backup-{}", cfg.path, std::process::id());
    let auth = kist_backend::sftp::SftpAuth {
        known_hosts: Some(known_hosts.into()),
        key: None,
        password: Some(password),
    };
    round_trip(Backend::sftp(&cfg, auth).await.unwrap()).await;
}

/// 寬鬆後端：backup 可以用（§12），prune 與維護命令不行。
#[tokio::test]
async fn rclone_v4_backup_round_trips() {
    if std::env::var("KIST_TEST_RCLONE").ok().as_deref() != Some("1") {
        eprintln!("rclone v4 backup test skipped (tests/rclone-setup.sh 未跑)");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let b = Backend::from_url(&format!("rclone://{}", dir.path().display()))
        .await
        .unwrap();
    round_trip(b).await;
}
