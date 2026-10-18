//! 真的遠端來源（M12 第 7 段之三，remote-to-remote）：`sftp://` 與 `s3://` 來源的 backup、讀回、沒變的第二次
//! backup。tests/sftp-setup.sh 與 tests/minio-setup.sh 的環境變數沒設就略過。
//!
//! SFTP 來源的連線認證走 `KIST_SFTP_*`（`sftp::auth_from_env`），這裡從 `KIST_TEST_SFTP_*` 設進去；`set_var` 是
//! 行程全域的，所以這些測試自成一個 binary，不與別的 SFTP 測試搶。來源的檔案經 repo 模式的 `Backend` 寫進伺服器：
//! SFTP 的 chroot 根屬於 root，寫得進去的只有 `KIST_TEST_SFTP_URL` 的那個目錄底下。

#[path = "v4_common/mod.rs"]
mod v4_common;

use std::collections::BTreeMap;
use std::path::PathBuf;

use kist_backend::Backend;
use kist_core::v4::{BackupOptions, BackupSummary, Repo};
use kist_core::SourceSpec;
use kist_format::v4::snapshot::split_locator;
use v4_common::*;

/// 寫進來源的檔案：巢狀目錄、空檔、一個超過 8 MiB 的檔（遠端讀取要分好幾次）、幾個小檔。
fn source_files() -> Vec<(String, Vec<u8>)> {
    vec![
        ("top.txt".to_owned(), b"top\n".to_vec()),
        ("empty".to_owned(), Vec::new()),
        ("d1/a.bin".to_owned(), data("a", 20 * 1024)),
        ("d1/d2/b.bin".to_owned(), data("b", 3 * 1024)),
        (
            "d1/d2/big.bin".to_owned(),
            data("big", 9 * 1024 * 1024 + 123),
        ),
        ("d3/c.txt".to_owned(), b"c".to_vec()),
    ]
}

/// `read_tree` 的預期：root 的定位切段之後接相對路徑；每個上層目錄一個 `Dir`。
fn expected_tree(url: &str, files: &[(String, Vec<u8>)]) -> BTreeMap<String, Node> {
    let root: Vec<String> = split_locator(url.as_bytes())
        .unwrap()
        .into_iter()
        .map(|s| String::from_utf8(s).unwrap())
        .collect();
    let root = root.join("/");
    let mut out = BTreeMap::new();
    for (rel, content) in files {
        let parts: Vec<&str> = rel.split('/').collect();
        for i in 1..parts.len() {
            out.insert(format!("{root}/{}", parts[..i].join("/")), Node::Dir);
        }
        out.insert(format!("{root}/{rel}"), Node::File(content.clone()));
    }
    out
}

async fn backup_url(repo: &Repo, url: &str) -> BackupSummary {
    repo.backup(
        &[PathBuf::from(url)],
        BackupOptions {
            source: SourceSpec::Url(url.to_owned()),
            ..backup_options()
        },
    )
    .await
    .unwrap()
}

/// `tests/sftp-setup.sh` 的連線：repo 模式的 config 與認證；來源連線要的 `KIST_SFTP_*` 也在這裡設好。
fn sftp_env() -> Option<(kist_backend::sftp::SftpConfig, kist_backend::sftp::SftpAuth)> {
    let (Some(url), Some(password), Some(known_hosts)) = (
        std::env::var("KIST_TEST_SFTP_URL").ok(),
        std::env::var("KIST_TEST_SFTP_PASSWORD").ok(),
        std::env::var("KIST_TEST_SFTP_KNOWN_HOSTS").ok(),
    ) else {
        return None;
    };
    std::env::set_var("KIST_SFTP_PASSWORD", &password);
    std::env::set_var("KIST_SFTP_KNOWN_HOSTS", &known_hosts);
    let cfg = kist_backend::sftp::parse_sftp_url(&url).unwrap();
    let auth = kist_backend::sftp::SftpAuth {
        known_hosts: Some(known_hosts.into()),
        key: None,
        password: Some(password),
    };
    Some((cfg, auth))
}

fn sftp_url(cfg: &kist_backend::sftp::SftpConfig) -> String {
    format!(
        "sftp://{}@{}:{}/{}",
        cfg.user.clone().unwrap_or_default(),
        cfg.host,
        cfg.port,
        cfg.path.trim_matches('/')
    )
}

/// SFTP 來源：讀回的樹與寫進去的相同；沒變的第二次 backup 全部重讀（SFTP 沒有快速路徑，ADR 016），靠去重不寫
/// 新的 chunk。
#[tokio::test]
async fn sftp_source_backs_up_and_rereads_unchanged_files() {
    let Some((mut cfg, auth)) = sftp_env() else {
        eprintln!("SFTP source test skipped (tests/sftp-setup.sh 未跑)");
        return;
    };
    cfg.path = format!("{}/src-{}", cfg.path.trim_matches('/'), std::process::id());
    let writer = Backend::sftp(&cfg, auth).await.unwrap();
    let files = source_files();
    for (rel, content) in &files {
        writer.put_if_absent(rel, content.clone()).await.unwrap();
    }
    let url = sftp_url(&cfg);
    let t = TestRepo::new().await;
    let repo = t.open().await;
    let first = backup_url(&repo, &url).await;
    assert_eq!(
        read_tree(&repo, &first.snapshot).await,
        expected_tree(&url, &files)
    );
    assert_eq!(first.stats.files, files.len() as u64);

    let second = backup_url(&repo, &url).await;
    let r = &second.report;
    assert_eq!(second.parent.as_deref(), Some(first.snapshot.as_str()));
    assert_eq!(r.files_reused, 0, "no fast path for SFTP: {r:?}");
    assert_eq!(r.chunks_new, 0, "everything re-read is deduplicated: {r:?}");
    assert!(r.chunks_reused > 0, "{r:?}");
    assert_eq!(
        read_tree(&repo, &second.snapshot).await,
        expected_tree(&url, &files)
    );
    for (rel, _) in &files {
        writer.delete(rel).await.unwrap();
    }
}

/// S3 來源：讀回的樹與寫進去的相同；沒變的第二次 backup 每個物件都以 etag 與大小走快速路徑，不重讀。
#[tokio::test]
async fn s3_source_backs_up_and_reuses_unchanged_objects() {
    let (Some(endpoint), Some(bucket)) = (
        std::env::var("KIST_TEST_S3_ENDPOINT").ok(),
        std::env::var("KIST_TEST_S3_BUCKET").ok(),
    ) else {
        eprintln!("S3 source test skipped (tests/minio-setup.sh 未跑)");
        return;
    };
    std::env::set_var("AWS_ENDPOINT", endpoint);
    std::env::set_var("AWS_ALLOW_HTTP", "true");
    if std::env::var("AWS_DEFAULT_REGION").is_err() {
        std::env::set_var("AWS_DEFAULT_REGION", "us-east-1");
    }
    let prefix = format!("src-{}", std::process::id());
    let writer = Backend::s3(&bucket, &prefix).unwrap();
    let files = source_files();
    for (rel, content) in &files {
        writer.put_if_absent(rel, content.clone()).await.unwrap();
    }
    let url = format!("s3://{bucket}/{prefix}");
    let t = TestRepo::new().await;
    let repo = t.open().await;
    let first = backup_url(&repo, &url).await;
    assert_eq!(
        read_tree(&repo, &first.snapshot).await,
        expected_tree(&url, &files)
    );
    assert_eq!(first.stats.files, files.len() as u64);

    let second = backup_url(&repo, &url).await;
    let r = &second.report;
    assert_eq!(
        r.files_reused,
        files.len() as u64,
        "etag and size prove each object: {r:?}"
    );
    assert_eq!(r.chunks_new, 0, "{r:?}");
    assert_eq!(
        read_tree(&repo, &second.snapshot).await,
        expected_tree(&url, &files)
    );
    for (rel, _) in &files {
        writer.delete(rel).await.unwrap();
    }
}
