//! `Backend::from_url`：本機路徑與 s3:// URL 的解析。

use kist_backend::{Backend, BackendError, RepoLocation};

#[test]
fn plain_path_is_local() {
    let dir = tempfile::tempdir().unwrap();
    let loc = RepoLocation::parse(dir.path().to_str().unwrap()).unwrap();
    assert!(matches!(loc, RepoLocation::Local(ref p) if p == dir.path()));
    assert!(RepoLocation::parse("relative/dir").is_ok());
}

#[test]
fn s3_url_splits_bucket_and_prefix() {
    match RepoLocation::parse("s3://my-bucket/backups/laptop/").unwrap() {
        RepoLocation::S3 { bucket, prefix } => {
            assert_eq!(bucket, "my-bucket");
            assert_eq!(prefix, "backups/laptop");
        }
        other => panic!("{other:?}"),
    }
    match RepoLocation::parse("s3://only-bucket").unwrap() {
        RepoLocation::S3 { bucket, prefix } => {
            assert_eq!(bucket, "only-bucket");
            assert_eq!(prefix, "");
        }
        other => panic!("{other:?}"),
    }
    assert!(matches!(
        RepoLocation::parse("s3://"),
        Err(BackendError::InvalidUrl(_))
    ));
    assert!(matches!(
        RepoLocation::parse("ftp://x/y"),
        Err(BackendError::InvalidUrl(_))
    ));
}

#[tokio::test]
async fn from_url_local_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let b = Backend::from_url(dir.path().join("repo").to_str().unwrap()).unwrap();
    b.put("config", vec![1]).await.unwrap();
    assert!(dir.path().join("repo/config").is_file());
    assert_eq!(
        b.location().to_string(),
        dir.path().join("repo").display().to_string()
    );
}
