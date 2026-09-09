//! `Backend::from_url`：本機路徑、s3://、sftp:// 與 rclone:// URL 的解析。

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

#[test]
fn rclone_url_parses_remote_and_path() {
    use kist_backend::sftp::parse_rclone_url;

    // remote:path 形式
    let cfg = parse_rclone_url("rclone://gdrive/backups/laptop").unwrap();
    assert_eq!(cfg.remote, "gdrive");
    assert_eq!(cfg.path, "backups/laptop");

    // remote 留空 = rclone 的本機檔案系統；路徑視為絕對路徑
    let cfg = parse_rclone_url("rclone:///srv/backups").unwrap();
    assert_eq!(cfg.remote, "");
    assert_eq!(cfg.path, "srv/backups");

    // Display 往返
    assert_eq!(
        RepoLocation::parse("rclone://gdrive/backups")
            .unwrap()
            .to_string(),
        "rclone://gdrive/backups"
    );
    assert_eq!(
        RepoLocation::parse("rclone:///srv/backups")
            .unwrap()
            .to_string(),
        "rclone:///srv/backups"
    );
    // 尾斜線正規化掉
    assert_eq!(
        RepoLocation::parse("rclone://gdrive/backups/")
            .unwrap()
            .to_string(),
        "rclone://gdrive/backups"
    );

    // 拒絕：沒有路徑
    assert!(parse_rclone_url("rclone://gdrive").is_err());
    assert!(parse_rclone_url("rclone://gdrive/").is_err());
    assert!(parse_rclone_url("rclone://").is_err());
    // 拒絕：remote 名字裡出現不是 remote name 的字元（可能是把 sftp:// 的寫法搬過來）
    assert!(parse_rclone_url("rclone://gd:rive/x").is_err());
    assert!(parse_rclone_url("rclone://bob@gdrive/x").is_err());
    // 拒絕：remote 名字會被 rclone 當成旗標解析（remote/path 合成一個 argv 元素，
    // rclone 的旗標解析穿插在位置參數之間——加密設定檔下 --password-command 的值
    // 會被 shell 執行，見 ADR 014）
    assert!(parse_rclone_url("rclone://--config=evil/x").is_err());
    assert!(parse_rclone_url("rclone://--password-command=id/x").is_err());
    assert!(parse_rclone_url("rclone://-x/y").is_err());
    assert!(parse_rclone_url("rclone://a b/c").is_err());
    assert!(parse_rclone_url("rclone://a=b/c").is_err());
    // 白名單內的合法 remote 名（字母數字、-、_、.；rclone 的 section 名允許點）
    assert!(parse_rclone_url("rclone://gdrive-2_x/backups").is_ok());
    assert!(parse_rclone_url("rclone://my.remote/backups").is_ok());
    // rclone:// 不吃路徑以外的 scheme 誤用
    assert!(matches!(
        parse_rclone_url("sftp://example.com/repo"),
        Err(BackendError::InvalidUrl(_))
    ));
    // RepoLocation 路由與 is_remote
    assert!(matches!(
        RepoLocation::parse("rclone://gdrive/backups").unwrap(),
        RepoLocation::Rclone(_)
    ));
    assert!(RepoLocation::parse("rclone://gdrive/backups")
        .unwrap()
        .is_remote());
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
