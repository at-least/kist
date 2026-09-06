//! `Backend::from_url`：本機路徑、s3:// 與 sftp:// URL 的解析。

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

#[test]
fn sftp_url_parses_user_host_port_path() {
    use kist_backend::sftp::parse_sftp_url;

    let cfg = parse_sftp_url("sftp://bob@example.com:2200/backups/laptop").unwrap();
    assert_eq!(cfg.user.as_deref(), Some("bob"));
    assert_eq!(cfg.host, "example.com");
    assert_eq!(cfg.port, 2200);
    assert_eq!(cfg.path, "backups/laptop");

    // 預設：port 22、user 留空（連線時用目前使用者）
    let cfg = parse_sftp_url("sftp://example.com/repo").unwrap();
    assert_eq!(cfg.user, None);
    assert_eq!(cfg.port, 22);
    assert_eq!(cfg.path, "repo");

    // IPv6
    let cfg = parse_sftp_url("sftp://[2001:db8::1]/repo").unwrap();
    assert_eq!(cfg.host, "2001:db8::1");
    assert_eq!(cfg.port, 22);
    let cfg = parse_sftp_url("sftp://bob@[2001:db8::1]:2222/repo").unwrap();
    assert_eq!(cfg.user.as_deref(), Some("bob"));
    assert_eq!(cfg.host, "2001:db8::1");
    assert_eq!(cfg.port, 2222);

    // Display 往返：同樣的東西要印得回來
    let url = "sftp://bob@example.com:2200/backups";
    assert_eq!(RepoLocation::parse(url).unwrap().to_string(), url);
    // 標準 port 不印
    assert_eq!(
        RepoLocation::parse("sftp://bob@example.com/repo")
            .unwrap()
            .to_string(),
        "sftp://bob@example.com/repo"
    );

    // 拒絕：URL 裡帶密碼（會進 shell 歷史與 process 清單）
    assert!(parse_sftp_url("sftp://bob:secret@example.com/repo").is_err());
    // 拒絕：沒有路徑、沒有主機
    assert!(parse_sftp_url("sftp://example.com").is_err());
    assert!(parse_sftp_url("sftp://example.com/").is_err());
    assert!(parse_sftp_url("sftp:///repo").is_err());
    // 拒絕：爛 port
    assert!(parse_sftp_url("sftp://example.com:0/repo").is_err());
    assert!(parse_sftp_url("sftp://example.com:99999/repo").is_err());
    assert!(matches!(
        parse_sftp_url("https://example.com/repo"),
        Err(BackendError::InvalidUrl(_))
    ));
}

#[tokio::test]
async fn from_url_local_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let b = Backend::from_url(dir.path().join("repo").to_str().unwrap())
        .await
        .unwrap();
    b.put("config", vec![1]).await.unwrap();
    assert!(dir.path().join("repo/config").is_file());
    assert_eq!(
        b.location().to_string(),
        dir.path().join("repo").display().to_string()
    );
}
