//! v4 在真的後端上：init、backup、讀回、沒變的第二次 backup 不寫新東西、另一台 client 取回驗證後沿用、
//! restore、check --read-data。MinIO、SFTP、rclone 有環境變數才跑（tests/README.md）；本機與記憶體後端的規則
//! 測試在 `v4_backup*.rs`、`v4_restore*.rs`、`v4_check.rs`。

#[path = "v4_common/mod.rs"]
mod v4_common;

use kist_backend::Backend;
use kist_core::v4::Repo;
use kist_format::v4::keys::Key;
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
    let second = repo
        .backup(std::slice::from_ref(&src), opts(1))
        .await
        .unwrap();
    assert_eq!(second.parent.as_deref(), Some(first.snapshot.as_str()));
    assert_eq!(
        second.report.packs_new + second.report.mpacks_new + second.report.deltas_new,
        0,
        "{:?}",
        second.report
    );
    // 另一台 client 備同樣的內容：別台的副本要先取回驗證（B2，在 blocking 執行緒上以 block_on 走真的網路），
    // B4 讀只從 delta 得知的 pack 的 trailer
    let other = Repo::open(backend.clone(), PASSWORD, Some(&dir.path().join("cache2")))
        .await
        .unwrap();
    let copy = dir.path().join("copy");
    sample_source(&copy);
    let theirs = other.backup(&[copy], opts(2)).await.unwrap();
    assert_eq!(theirs.report.chunks_new, 0, "{:?}", theirs.report);
    assert!(theirs.report.verified > 0, "{:?}", theirs.report);
    assert!(read_tree(&other, &theirs.snapshot)
        .await
        .values()
        .any(|n| *n == Node::File(data("big", 300 * 1024))));
    // restore（第 5 段）：落點是目標加上定位的各段，內容與來源相同
    let target = dir.path().join("restored");
    let restored = repo
        .restore(
            &Key::parse(&first.snapshot).unwrap(),
            &target,
            kist_core::RestoreOptions::default(),
        )
        .await
        .unwrap();
    assert!(restored.errors.is_empty(), "{restored:?}");
    assert_eq!(source_tree(&target.join(segs.join("/"))), source_tree(&src));
    // check --read-data：每個 pack 與 mpack 整份下載驗證，沒有錯誤與警告
    let report = repo
        .check(kist_core::v4::check::CheckOptions {
            read_data: true,
            repair: false,
        })
        .await
        .unwrap();
    assert!(
        report.errors.is_empty() && report.warnings.is_empty(),
        "{report:?}"
    );
    assert_eq!(report.snapshots, 3);
    // prune（第 6 段）：真的後端上時間不能快轉，只驗 P0 到 P8 跑得完、寫出下一代、沒有到期的東西就不刪，之後
    // check 照樣乾淨。寬鬆後端（rclone）上 P0 拒絕、什麼都不寫。
    let before = *repo
        .list_heads(None)
        .await
        .unwrap()
        .keys()
        .next_back()
        .unwrap();
    let pruned = repo.prune(kist_core::v4::PruneOptions::default()).await;
    if backend.create_only() {
        let pruned = pruned.unwrap();
        assert_eq!(pruned.generation, before + 1, "{pruned:?}");
        assert!(pruned.newcomers > 0);
        assert_eq!(
            (pruned.deleted_packs, pruned.condemned, pruned.tombstoned),
            (0, 0, 0)
        );
        assert!(backend.list("deltas").await.unwrap().is_empty(), "absorbed");
        let report = repo
            .check(kist_core::v4::check::CheckOptions {
                read_data: true,
                repair: false,
            })
            .await
            .unwrap();
        assert!(
            report.errors.is_empty() && report.warnings.is_empty(),
            "{report:?}"
        );
    } else {
        let err = pruned.unwrap_err();
        assert!(err.to_string().contains("probe"), "{err}");
        assert_eq!(
            *repo
                .list_heads(None)
                .await
                .unwrap()
                .keys()
                .next_back()
                .unwrap(),
            before
        );
    }
    // 清掉（只有測試用的前綴）
    for prefix in [
        "keys",
        "packs",
        "mpacks",
        "parity",
        "deltas",
        "snapshots",
        "maint",
    ] {
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

/// 依 client 分前綴的憑證（format-v4.md §1 的建議部署，負責人決定 1）：tests/minio-setup.sh 的 kistclient 只能在
/// `*/{packs,mpacks,parity,deltas,snapshots}/<client id>/` 底下建立物件、讀全部、列 bucket。它備得了份；寫別台
/// client 的前綴、`maint/`、`keys/`、刪除都被擋；以另一個 client id 備份失敗（pack 寫不進去）；prune 在 P0 就
/// 失敗（探針建不起來），不寫任何東西。`s3:if-none-match` 的 bucket policy 條件沒有測（MinIO 是否支援
/// UNVERIFIED）。
#[tokio::test]
async fn s3_client_prefix_credentials_confine_a_client() {
    let (Some(endpoint), Some(bucket)) = (
        std::env::var("KIST_TEST_S3_ENDPOINT").ok(),
        std::env::var("KIST_TEST_S3_BUCKET").ok(),
    ) else {
        eprintln!("S3 client-prefix test skipped");
        return;
    };
    let (Some(key), Some(secret), Some(cid)) = (
        std::env::var("KIST_TEST_S3_CLIENT_KEY").ok(),
        std::env::var("KIST_TEST_S3_CLIENT_SECRET").ok(),
        std::env::var("KIST_TEST_S3_CLIENT_ID").ok(),
    ) else {
        eprintln!("KIST_TEST_S3_CLIENT_KEY / _SECRET / _ID not set; skipped");
        return;
    };
    std::env::set_var("AWS_ENDPOINT", endpoint);
    std::env::set_var("AWS_ALLOW_HTTP", "true");
    if std::env::var("AWS_DEFAULT_REGION").is_err() {
        std::env::set_var("AWS_DEFAULT_REGION", "us-east-1");
    }
    let me = kist_format::v4::ids::ClientId::from_hex(&cid).unwrap();
    let prefix = format!("client-{}", std::process::id());
    // root 的憑證 init；client 的憑證做其他事
    let root = Backend::from_url(&format!("s3://{bucket}/{prefix}"))
        .await
        .unwrap();
    Repo::init(root.clone(), PASSWORD, init_options(0))
        .await
        .unwrap();
    let mine = Backend::s3_with_credentials(&bucket, &prefix, &key, &secret).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let repo = Repo::open(mine.clone(), PASSWORD, Some(&dir.path().join("cache")))
        .await
        .unwrap();
    let src = dir.path().join("src");
    sample_source(&src);
    let mut o = opts(1);
    o.client = me;
    let s = repo.backup(std::slice::from_ref(&src), o).await.unwrap();
    assert!(
        s.report.packs_new > 0 && s.report.mpacks_new > 0,
        "{:?}",
        s.report
    );
    assert_eq!(repo.list_snapshots().await.unwrap().len(), 1);

    // 別台 client 的前綴、maint/、keys/：建立被擋；刪除被擋
    let other = kist_format::v4::ids::ClientId::from_bytes([0xc2; 16]);
    let zero = kist_format::v4::ids::ObjectId::ZERO;
    for k in [
        Key::Pack {
            client: other,
            id: zero,
        }
        .path(),
        Key::Snapshot {
            client: other,
            ts: kist_format::v4::keys::SnapTs::from_nanos(0).unwrap(),
            replica: false,
        }
        .path(),
        Key::Probe([7; 16]).path(),
        Key::Head {
            g: 1,
            replica: false,
        }
        .path(),
        Key::Slot([7; 16]).path(),
    ] {
        let err = mine.put_if_absent(&k, vec![1]).await.unwrap_err();
        assert!(
            matches!(err, kist_backend::BackendError::Store(_)),
            "{k}: {err}"
        );
        assert!(!root.exists(&k).await.unwrap(), "{k} was created");
    }
    let err = mine.delete(&s.snapshot).await.unwrap_err();
    assert!(matches!(err, kist_backend::BackendError::Store(_)), "{err}");
    assert!(root.exists(&s.snapshot).await.unwrap());

    // 同一組憑證冒充另一台 client：pack 寫不進去，backup 失敗、沒有 snapshot
    let mut o = opts(2);
    o.client = other;
    let copy = dir.path().join("copy");
    sample_source(&copy);
    assert!(repo.backup(&[copy], o).await.is_err());
    assert_eq!(repo.list_snapshots().await.unwrap().len(), 1);

    // prune：P0 的探針建不起來，什麼都不寫（探針的 key 在錯誤裡；maint/ 底下只有世代 0 的 head）
    let err = repo
        .prune(kist_core::v4::PruneOptions::default())
        .await
        .unwrap_err();
    assert!(err.to_string().contains("maint/probe-"), "{err}");
    let maint: Vec<String> = root
        .list("maint")
        .await
        .unwrap()
        .into_iter()
        .map(|o| o.key)
        .collect();
    assert_eq!(
        maint,
        vec![
            "maint/gen/0000000000000000".to_owned(),
            "maint/gen/0000000000000000.r1".to_owned()
        ]
    );

    for prefix in [
        "keys",
        "packs",
        "mpacks",
        "parity",
        "deltas",
        "snapshots",
        "maint",
    ] {
        for o in root.list(prefix).await.unwrap() {
            root.delete(&o.key).await.unwrap();
        }
    }
    root.delete("config").await.unwrap();
}
