//! v4 的後端契約（docs/format-v4.md §1、§12）：每個後端都要滿足，v4 的 GC 依賴它們。
//!
//! 1. 只在不存在時建立：第二次建立得到 AlreadyExists，內容與修改時間都不變；
//! 2. `list` 與 `head` 的修改時間一致（截到秒），剛建立的物件的修改時間接近本機時鐘；
//! 3. 依 client 分的前綴：`packs/` 列出所有 client，`packs/<c>` 只列那一台；
//! 4. 以 start-after 列出：`maint/gen/` 從某個 key 之後列，不含那個 key 本身，含它的 `.r1`；
//! 5. 刪除：之後讀不到；刪除不存在的物件也成功（P9 依序刪 `.r1`、parity、主體，前兩個常常不存在）；
//! 6. P0 的探針：本機、S3、SFTP 通過，rclone 橋接拒絕（它的條件建立是先 stat 再 rename），都不留下探針。
//!
//! v3 的契約（重寫會刷新修改時間）已隨 v3 與 `Backend::put` 一起移除。
//! 本機一定跑；S3、SFTP、rclone 有環境變數才跑（tests/README.md）。

use kist_backend::{Backend, BackendError};
use kist_format::v4::ids::ClientId;
use kist_format::v4::keys::Key;
use kist_format::ObjectId;

fn pack(client: u8, id: u8) -> String {
    Key::Pack {
        client: ClientId::from_bytes([client; 16]),
        id: ObjectId::from_bytes([id; 32]),
    }
    .path()
}

fn head(g: u64, replica: bool) -> String {
    Key::Head { g, replica }.path()
}

async fn keys(b: &Backend, prefix: &str) -> Vec<String> {
    let mut keys: Vec<String> = b
        .list(prefix)
        .await
        .unwrap()
        .into_iter()
        .map(|o| o.key)
        .collect();
    keys.sort();
    keys
}

async fn keys_after(b: &Backend, prefix: &str, after: &str) -> Vec<String> {
    let mut keys: Vec<String> = b
        .list_after(prefix, after)
        .await
        .unwrap()
        .into_iter()
        .map(|o| o.key)
        .collect();
    keys.sort();
    keys
}

async fn contract_v4(b: &Backend, strict: bool) {
    // 6. P0 的探針
    match b.probe().await {
        Ok(p) => {
            assert!(
                strict,
                "{}: a relaxed backend passed the probe",
                b.location()
            );
            assert!((p.backend - p.local).abs() <= time::Duration::minutes(5));
            let since = time::OffsetDateTime::now_utc() - p.local;
            assert!(since.abs() < time::Duration::minutes(1), "{p:?}");
        }
        Err(BackendError::Probe(msg)) => assert!(!strict, "{msg}"),
        Err(e) => panic!("{e}"),
    }
    assert!(keys(b, "maint").await.is_empty(), "探針沒有刪掉");

    // 1. 只在不存在時建立
    let p = pack(1, 0xaa);
    b.put_if_absent(&p, b"one".to_vec()).await.unwrap();
    let t1 = b.head(&p).await.unwrap();
    // 修改時間截到秒：要看到「變了」得等過一秒
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    let err = b.put_if_absent(&p, b"two".to_vec()).await.unwrap_err();
    assert!(matches!(err, BackendError::AlreadyExists(_)), "{err}");
    assert_eq!(b.get(&p).await.unwrap(), b"one", "第二次建立改了內容");
    assert_eq!(
        b.head(&p).await.unwrap().modified,
        t1.modified,
        "第二次建立動了修改時間"
    );

    // 2. list 與 head 一致；剛建立的物件接近本機時鐘
    let listed = b
        .list("packs")
        .await
        .unwrap()
        .into_iter()
        .find(|o| o.key == p)
        .unwrap();
    assert_eq!(
        listed.modified, t1.modified,
        "list 與 head 的修改時間不一致"
    );
    assert_eq!((listed.size, t1.size), (3, 3));
    let age = time::OffsetDateTime::now_utc() - t1.modified;
    assert!(
        age.abs() < time::Duration::minutes(5),
        "修改時間 {} 離本機時鐘太遠",
        t1.modified
    );

    // range read
    let big = pack(1, 0xbb);
    b.put_if_absent(&big, (0..=255u8).collect()).await.unwrap();
    assert_eq!(b.get_range(&big, 10..13).await.unwrap(), vec![10, 11, 12]);

    // 3. 依 client 分的前綴
    let other = pack(2, 0xaa);
    b.put_if_absent(&other, vec![2]).await.unwrap();
    assert_eq!(keys(b, "packs").await, {
        let mut all = vec![p.clone(), big.clone(), other.clone()];
        all.sort();
        all
    });
    let client1 = format!("packs/{}", ClientId::from_bytes([1; 16]));
    assert_eq!(keys(b, &client1).await.len(), 2);
    assert!(keys(b, "nothing").await.is_empty());

    // 4. start-after：不含那個 key 本身，含它的 .r1 與之後的世代
    for (g, replica) in [(1, false), (1, true), (2, false), (2, true), (3, false)] {
        b.put_if_absent(&head(g, replica), vec![g as u8])
            .await
            .unwrap();
    }
    assert_eq!(
        keys_after(b, "maint/gen", &head(1, false)).await,
        [head(1, true), head(2, false), head(2, true), head(3, false)]
    );
    assert_eq!(
        keys_after(b, "maint/gen", &head(1, true)).await,
        [head(2, false), head(2, true), head(3, false)]
    );
    assert!(keys_after(b, "maint/gen", &head(3, false)).await.is_empty());
    // 起點不必存在（上次看過的世代之後才有新的）
    assert_eq!(
        keys_after(b, "maint/gen", &head(2, true)).await,
        [head(3, false)]
    );

    // 5. 刪除
    b.delete(&p).await.unwrap();
    assert!(matches!(
        b.get(&p).await.unwrap_err(),
        BackendError::NotFound(_)
    ));
    assert!(matches!(
        b.head(&p).await.unwrap_err(),
        BackendError::NotFound(_)
    ));
    b.delete(&p).await.unwrap();
    b.delete(&format!("{p}.r1")).await.unwrap();

    // 清掉
    for prefix in ["packs", "maint"] {
        for o in b.list(prefix).await.unwrap() {
            b.delete(&o.key).await.unwrap();
        }
    }
}

#[tokio::test]
async fn local_backend_meets_the_v4_contract() {
    let dir = tempfile::tempdir().unwrap();
    let b = Backend::local(&dir.path().join("repo")).unwrap();
    contract_v4(&b, true).await;
}

#[tokio::test]
async fn s3_backend_meets_the_v4_contract() {
    let (Some(endpoint), Some(bucket)) = (
        std::env::var("KIST_TEST_S3_ENDPOINT").ok(),
        std::env::var("KIST_TEST_S3_BUCKET").ok(),
    ) else {
        eprintln!("S3 v4 contract test skipped");
        return;
    };
    std::env::set_var("AWS_ENDPOINT", endpoint);
    std::env::set_var("AWS_ALLOW_HTTP", "true");
    if std::env::var("AWS_DEFAULT_REGION").is_err() {
        std::env::set_var("AWS_DEFAULT_REGION", "us-east-1");
    }
    let b = Backend::from_url(&format!("s3://{bucket}/contract-v4-{}", std::process::id()))
        .await
        .unwrap();
    contract_v4(&b, true).await;
}

#[tokio::test]
async fn sftp_backend_meets_the_v4_contract() {
    let (Some(url), Some(password), Some(known_hosts)) = (
        std::env::var("KIST_TEST_SFTP_URL").ok(),
        std::env::var("KIST_TEST_SFTP_PASSWORD").ok(),
        std::env::var("KIST_TEST_SFTP_KNOWN_HOSTS").ok(),
    ) else {
        eprintln!("SFTP v4 contract test skipped (tests/sftp-setup.sh 未跑)");
        return;
    };
    let mut cfg = kist_backend::sftp::parse_sftp_url(&url).unwrap();
    cfg.path = format!("{}/contract-v4-{}", cfg.path, std::process::id());
    let auth = kist_backend::sftp::SftpAuth {
        known_hosts: Some(known_hosts.into()),
        key: None,
        password: Some(password),
    };
    let b = Backend::sftp(&cfg, auth).await.unwrap();
    contract_v4(&b, true).await;
}

#[tokio::test]
async fn rclone_bridge_meets_the_v4_contract() {
    if std::env::var("KIST_TEST_RCLONE").ok().as_deref() != Some("1") {
        eprintln!("rclone v4 contract test skipped (tests/rclone-setup.sh 未跑)");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let b = Backend::from_url(&format!("rclone://{}", dir.path().display()))
        .await
        .unwrap();
    contract_v4(&b, false).await;
}
