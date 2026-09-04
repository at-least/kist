//! 每個後端都要滿足的合約（M3 的 GC 安全性依賴它們）：
//! 1. 對既有 key 重 put 會刷新 `modified`；
//! 2. `put_if_absent` 對既有 key 失敗（AlreadyExists）而且**不動** `modified`；
//! 3. `list` 與 `head` 回的 `modified` 一致。
//!
//! 另外：range read、遞迴 list、delete 後 NotFound。
//! 本機一定跑；S3 / SFTP 有環境變數才跑。

use kist_backend::{Backend, BackendError};

async fn contract(b: &Backend) {
    // `modified` 統一是整秒：要看到「變新」得等過一秒
    let settle = || async {
        tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    };
    b.put("trees/aa", b"one".to_vec()).await.unwrap();
    let t1 = b.head("trees/aa").await.unwrap();
    // 3. list == head
    let listed = b
        .list("trees")
        .await
        .unwrap()
        .into_iter()
        .find(|o| o.key == "trees/aa")
        .unwrap();
    assert_eq!(
        listed.modified, t1.modified,
        "list 與 head 的 modified 不一致"
    );
    assert_eq!(listed.size, 3);
    // 1. 重 put 刷新
    settle().await;
    b.put("trees/aa", b"one".to_vec()).await.unwrap();
    let t2 = b.head("trees/aa").await.unwrap();
    assert!(
        t2.modified > t1.modified,
        "重 put 沒有刷新 modified ({} → {})",
        t1.modified,
        t2.modified
    );
    // 2. conditional put 失敗且不動時間
    settle().await;
    let err = b
        .put_if_absent("trees/aa", b"two".to_vec())
        .await
        .unwrap_err();
    assert!(matches!(err, BackendError::AlreadyExists(_)), "{err}");
    let t3 = b.head("trees/aa").await.unwrap();
    assert_eq!(
        t3.modified, t2.modified,
        "put_if_absent 失敗卻動了 modified"
    );
    assert_eq!(b.get("trees/aa").await.unwrap(), b"one");
    // conditional put 成功
    b.put_if_absent("gc/bb", b"KISTGC1\n".to_vec())
        .await
        .unwrap();
    assert!(b.exists("gc/bb").await.unwrap());
    // range read
    b.put("packs/cc", (0..=255u8).collect()).await.unwrap();
    assert_eq!(
        b.get_range("packs/cc", 10..13).await.unwrap(),
        vec![10, 11, 12]
    );
    // 遞迴 list
    b.put("snapshots/c1/t1", vec![1]).await.unwrap();
    b.put("snapshots/c1/t2", vec![2]).await.unwrap();
    b.put("snapshots/c2/t1", vec![3]).await.unwrap();
    let mut keys: Vec<String> = b
        .list("snapshots")
        .await
        .unwrap()
        .into_iter()
        .map(|o| o.key)
        .collect();
    keys.sort();
    assert_eq!(
        keys,
        ["snapshots/c1/t1", "snapshots/c1/t2", "snapshots/c2/t1"]
    );
    let keys: Vec<String> = b
        .list("snapshots/c1")
        .await
        .unwrap()
        .into_iter()
        .map(|o| o.key)
        .collect();
    assert_eq!(keys.len(), 2);
    assert!(b.list("nothing").await.unwrap().is_empty());
    // delete → NotFound
    b.delete("trees/aa").await.unwrap();
    assert!(matches!(
        b.get("trees/aa").await.unwrap_err(),
        BackendError::NotFound(_)
    ));
    assert!(!b.exists("trees/aa").await.unwrap());
    assert!(matches!(
        b.head("trees/aa").await.unwrap_err(),
        BackendError::NotFound(_)
    ));
    // 清掉
    for prefix in ["gc", "packs", "snapshots"] {
        for o in b.list(prefix).await.unwrap() {
            b.delete(&o.key).await.unwrap();
        }
    }
}

#[tokio::test]
async fn local_backend_meets_the_contract() {
    let dir = tempfile::tempdir().unwrap();
    let b = Backend::local(&dir.path().join("repo")).unwrap();
    contract(&b).await;
}

#[tokio::test]
async fn s3_backend_meets_the_contract() {
    let (Some(endpoint), Some(bucket)) = (
        std::env::var("KIST_TEST_S3_ENDPOINT").ok(),
        std::env::var("KIST_TEST_S3_BUCKET").ok(),
    ) else {
        eprintln!("S3 contract test skipped");
        return;
    };
    std::env::set_var("AWS_ENDPOINT", endpoint);
    std::env::set_var("AWS_ALLOW_HTTP", "true");
    if std::env::var("AWS_DEFAULT_REGION").is_err() {
        std::env::set_var("AWS_DEFAULT_REGION", "us-east-1");
    }
    let b = Backend::from_url(&format!("s3://{bucket}/contract-{}", std::process::id())).unwrap();
    contract(&b).await;
}
