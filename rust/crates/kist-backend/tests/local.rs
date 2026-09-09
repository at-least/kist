//! 本機後端的行為測試（object_store::local）。

use kist_backend::{Backend, BackendError};

async fn temp_backend() -> (tempfile::TempDir, Backend) {
    let dir = tempfile::tempdir().unwrap();
    let backend = Backend::local(dir.path()).unwrap();
    (dir, backend)
}

#[tokio::test]
async fn put_get_head_list_round_trip() {
    let (_dir, b) = temp_backend().await;
    b.put("packs/aa", vec![1, 2, 3, 4, 5]).await.unwrap();
    b.put("packs/bb", vec![9]).await.unwrap();
    b.put("trees/cc", vec![7, 7]).await.unwrap();

    assert_eq!(b.get("packs/aa").await.unwrap(), vec![1, 2, 3, 4, 5]);
    assert_eq!(b.get_range("packs/aa", 1..4).await.unwrap(), vec![2, 3, 4]);
    assert_eq!(b.size("packs/aa").await.unwrap(), 5);
    assert!(b.exists("packs/aa").await.unwrap());
    assert!(!b.exists("packs/zz").await.unwrap());

    let mut listed: Vec<(String, u64)> = b
        .list("packs")
        .await
        .unwrap()
        .into_iter()
        .map(|o| (o.key, o.size))
        .collect();
    listed.sort();
    assert_eq!(
        listed,
        vec![("packs/aa".to_owned(), 5), ("packs/bb".to_owned(), 1)]
    );
    // list 與 head 的 modified 一致，而且是最近的時間
    let head = b.head("packs/aa").await.unwrap();
    let from_list = b
        .list("packs")
        .await
        .unwrap()
        .into_iter()
        .find(|o| o.key == "packs/aa")
        .unwrap();
    assert_eq!(head.modified, from_list.modified);
    let age = time::OffsetDateTime::now_utc() - head.modified;
    assert!(
        age.abs() < time::Duration::minutes(5),
        "modified {} 太離譜",
        head.modified
    );
    assert!(b.list("nothing").await.unwrap().is_empty());
}

#[tokio::test]
async fn missing_key_is_not_found() {
    let (_dir, b) = temp_backend().await;
    assert!(matches!(b.get("nope").await, Err(BackendError::NotFound(k)) if k == "nope"));
    assert!(matches!(
        b.size("nope").await,
        Err(BackendError::NotFound(_))
    ));
    assert!(matches!(
        b.get_range("nope", 0..1).await,
        Err(BackendError::NotFound(_))
    ));
}

#[tokio::test]
async fn put_if_absent_never_overwrites() {
    let (_dir, b) = temp_backend().await;
    b.put_if_absent("snapshots/c/1", vec![1]).await.unwrap();
    let second = b.put_if_absent("snapshots/c/1", vec![2]).await;
    assert!(matches!(second, Err(BackendError::AlreadyExists(k)) if k == "snapshots/c/1"));
    assert_eq!(
        b.get("snapshots/c/1").await.unwrap(),
        vec![1],
        "第一次寫入的內容必須保留"
    );
}

#[tokio::test]
async fn put_overwrites() {
    let (_dir, b) = temp_backend().await;
    b.put("config", vec![1]).await.unwrap();
    b.put("config", vec![2]).await.unwrap();
    assert_eq!(b.get("config").await.unwrap(), vec![2]);
}

#[tokio::test]
async fn delete_removes_key() {
    let (_dir, b) = temp_backend().await;
    b.put("gc/x", vec![1]).await.unwrap();
    b.delete("gc/x").await.unwrap();
    assert!(!b.exists("gc/x").await.unwrap());
}

#[tokio::test]
async fn local_creates_directory_if_missing() {
    let dir = tempfile::tempdir().unwrap();
    let nested = dir.path().join("a").join("b");
    let b = Backend::local(&nested).unwrap();
    b.put("config", vec![1]).await.unwrap();
    assert!(nested.join("config").is_file());
}
