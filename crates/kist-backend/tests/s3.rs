//! S3 後端的整合測試。需要一個 S3 相容服務（本機用 MinIO 容器），由環境變數啟用：
//!
//! ```sh
//! export KIST_TEST_S3_ENDPOINT=http://127.0.0.1:19000 KIST_TEST_S3_BUCKET=kist-test
//! export AWS_ACCESS_KEY_ID=kistadmin AWS_SECRET_ACCESS_KEY=kistsecret123
//! cargo test -p kist-backend --test s3
//! ```
//!
//! 沒設環境變數時全部略過（印一行說明）。每個測試用隨機 prefix，同一個 bucket 可以重複跑。

use kist_backend::{Backend, BackendError};

fn s3_env() -> Option<(String, String)> {
    let endpoint = std::env::var("KIST_TEST_S3_ENDPOINT").ok()?;
    let bucket = std::env::var("KIST_TEST_S3_BUCKET").ok()?;
    Some((endpoint, bucket))
}

/// 用環境變數建一個指到隨機 prefix 的後端；順便回傳同一 bucket 無 prefix 的後端做驗證。
fn s3_backends(test: &str) -> Option<(Backend, Backend, String)> {
    let (endpoint, bucket) = s3_env().or_else(|| {
        eprintln!("KIST_TEST_S3_ENDPOINT / KIST_TEST_S3_BUCKET not set; S3 test skipped");
        None
    })?;
    // AWS_ENDPOINT / AWS_ALLOW_HTTP 是 object_store 讀的名字
    std::env::set_var("AWS_ENDPOINT", &endpoint);
    std::env::set_var("AWS_ALLOW_HTTP", "true");
    if std::env::var("AWS_DEFAULT_REGION").is_err() {
        std::env::set_var("AWS_DEFAULT_REGION", "us-east-1");
    }
    let prefix = format!("test-{test}-{}", std::process::id());
    let with_prefix = Backend::from_url(&format!("s3://{bucket}/{prefix}")).unwrap();
    let root = Backend::from_url(&format!("s3://{bucket}")).unwrap();
    Some((with_prefix, root, prefix))
}

#[tokio::test]
async fn prefix_is_applied_and_basic_ops_work() {
    let Some((b, root, prefix)) = s3_backends("prefix") else {
        return;
    };
    b.put("config", vec![1, 2, 3]).await.unwrap();
    b.put("packs/aa", vec![9; 100]).await.unwrap();
    // 從 bucket 根看：物件必須在 <prefix>/ 底下，不是 bucket 根
    assert!(
        root.exists(&format!("{prefix}/config")).await.unwrap(),
        "prefix 沒有套用"
    );
    assert!(!root.exists("config").await.unwrap());

    assert_eq!(b.get("config").await.unwrap(), vec![1, 2, 3]);
    assert_eq!(b.get_range("packs/aa", 10..20).await.unwrap(), vec![9; 10]);
    assert_eq!(b.size("packs/aa").await.unwrap(), 100);
    let mut listed = b.list("packs").await.unwrap();
    listed.sort();
    assert_eq!(
        listed,
        vec![("packs/aa".to_owned(), 100)],
        "list 回傳的 key 不含 prefix"
    );
    assert!(matches!(
        b.get("nope").await,
        Err(BackendError::NotFound(_))
    ));

    b.delete("packs/aa").await.unwrap();
    b.delete("config").await.unwrap();
}

#[tokio::test]
async fn conditional_put_never_overwrites_on_s3() {
    let Some((b, _, _)) = s3_backends("cond") else {
        return;
    };
    b.put_if_absent("snapshots/c/1", vec![1]).await.unwrap();
    let second = b.put_if_absent("snapshots/c/1", vec![2]).await;
    assert!(
        matches!(second, Err(BackendError::AlreadyExists(_))),
        "{second:?}"
    );
    assert_eq!(b.get("snapshots/c/1").await.unwrap(), vec![1]);
    b.delete("snapshots/c/1").await.unwrap();
}

#[tokio::test]
async fn large_single_put() {
    let Some((b, _, _)) = s3_backends("big") else {
        return;
    };
    let data = vec![7u8; 64 * 1024 * 1024];
    b.put("packs/big", data.clone()).await.unwrap();
    assert_eq!(b.size("packs/big").await.unwrap(), data.len() as u64);
    assert_eq!(
        b.get_range("packs/big", (64 * 1024 * 1024 - 5)..(64 * 1024 * 1024))
            .await
            .unwrap(),
        vec![7; 5]
    );
    b.delete("packs/big").await.unwrap();
}

/// 只有 Put / Get / List 權限（沒有 Delete）的使用者：backup 需要的操作都要成功，delete 要被拒。
/// 需要 tests/minio-setup.sh 建的 kistbackup 使用者（KIST_TEST_S3_PUTONLY_KEY / _SECRET）。
#[tokio::test]
async fn put_only_user_can_write_and_read_but_not_delete() {
    let Some((endpoint, bucket)) = s3_env() else {
        eprintln!("S3 env not set; skipped");
        return;
    };
    let (Some(key), Some(secret)) = (
        std::env::var("KIST_TEST_S3_PUTONLY_KEY").ok(),
        std::env::var("KIST_TEST_S3_PUTONLY_SECRET").ok(),
    ) else {
        eprintln!("KIST_TEST_S3_PUTONLY_KEY / _SECRET not set; skipped");
        return;
    };
    // 憑證明確給定，不碰 process 共用的 AWS_ACCESS_KEY_ID（其他測試同時在用 root 帳號）
    std::env::set_var("AWS_ENDPOINT", &endpoint);
    std::env::set_var("AWS_ALLOW_HTTP", "true");
    if std::env::var("AWS_DEFAULT_REGION").is_err() {
        std::env::set_var("AWS_DEFAULT_REGION", "us-east-1");
    }
    let b = Backend::s3_with_credentials(
        &bucket,
        &format!("putonly-{}", std::process::id()),
        &key,
        &secret,
    )
    .unwrap();

    b.put("config", vec![1]).await.unwrap();
    b.put_if_absent("snapshots/c/1", vec![2]).await.unwrap();
    assert!(matches!(
        b.put_if_absent("snapshots/c/1", vec![3]).await,
        Err(BackendError::AlreadyExists(_))
    ));
    assert_eq!(b.get("config").await.unwrap(), vec![1]);
    assert_eq!(b.get_range("config", 0..1).await.unwrap(), vec![1]);
    assert_eq!(b.size("config").await.unwrap(), 1);
    assert_eq!(b.list("snapshots").await.unwrap().len(), 1);
    let denied = b.delete("config").await;
    assert!(
        matches!(denied, Err(BackendError::Store(_))),
        "delete 必須被拒：{denied:?}"
    );
    assert!(
        b.exists("config").await.unwrap(),
        "被拒的 delete 不能真的刪掉"
    );
}
