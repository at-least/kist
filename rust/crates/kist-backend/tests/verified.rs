//! `Backend::put_if_absent_verified` 的讀回驗證：init 的 config 用。
//! 「寫入成功但讀回不一致」在真後端上很難穩定重現，所以用一個會竄改讀取內容的
//! 包裝 store 模擬寬鬆後端上的 race（另一個 init 在讀回前覆蓋了 config）。

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::BoxStream;
use futures::StreamExt as _;
use object_store::local::LocalFileSystem;
use object_store::path::Path as StorePath;
use object_store::{
    Error as StoreError, GetOptions, GetResult, GetResultPayload, ListResult, ObjectMeta,
    ObjectStore, PutOptions, PutPayload, PutResult,
};
use std::fmt;
use std::sync::Arc;

use kist_backend::{Backend, BackendError, RepoLocation};

/// 把所有讀取的內容換成 `b"tampered"`：模擬「寫入與讀回之間被別的寫入者覆蓋」。
#[derive(Debug)]
struct TamperingStore {
    inner: LocalFileSystem,
}

impl fmt::Display for TamperingStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "TamperingStore")
    }
}

#[async_trait]
impl ObjectStore for TamperingStore {
    async fn put_opts(
        &self,
        location: &StorePath,
        payload: PutPayload,
        opts: PutOptions,
    ) -> object_store::Result<PutResult> {
        self.inner.put_opts(location, payload, opts).await
    }

    async fn put_multipart_opts(
        &self,
        _location: &StorePath,
        _opts: object_store::PutMultipartOptions,
    ) -> object_store::Result<Box<dyn object_store::MultipartUpload>> {
        Err(StoreError::NotImplemented {
            operation: "put_multipart_opts".to_owned(),
            implementer: "tampering".to_owned(),
        })
    }

    async fn get_opts(
        &self,
        location: &StorePath,
        options: GetOptions,
    ) -> object_store::Result<GetResult> {
        let mut result = self.inner.get_opts(location, options).await?;
        result.payload = GetResultPayload::Stream(
            futures::stream::once(async { Ok(Bytes::from_static(b"tampered")) }).boxed(),
        );
        Ok(result)
    }

    async fn get_ranges(
        &self,
        location: &StorePath,
        ranges: &[std::ops::Range<u64>],
    ) -> object_store::Result<Vec<Bytes>> {
        self.inner.get_ranges(location, ranges).await
    }

    fn delete_stream(
        &self,
        locations: BoxStream<'static, object_store::Result<StorePath>>,
    ) -> BoxStream<'static, object_store::Result<StorePath>> {
        self.inner.delete_stream(locations)
    }

    fn list(
        &self,
        prefix: Option<&StorePath>,
    ) -> BoxStream<'static, object_store::Result<ObjectMeta>> {
        self.inner.list(prefix)
    }

    async fn list_with_delimiter(
        &self,
        prefix: Option<&StorePath>,
    ) -> object_store::Result<ListResult> {
        self.inner.list_with_delimiter(prefix).await
    }

    async fn copy_opts(
        &self,
        from: &StorePath,
        to: &StorePath,
        options: object_store::CopyOptions,
    ) -> object_store::Result<()> {
        self.inner.copy_opts(from, to, options).await
    }
}

#[tokio::test]
async fn read_back_mismatch_is_concurrent_write() {
    let dir = tempfile::tempdir().unwrap();
    let store: Arc<dyn ObjectStore> = Arc::new(TamperingStore {
        inner: LocalFileSystem::new_with_prefix(dir.path()).unwrap(),
    });
    let b = Backend::from_store(store, RepoLocation::Local(dir.path().to_path_buf()));
    match b
        .put_if_absent_verified("config", b"real-config".to_vec())
        .await
    {
        Err(BackendError::ConcurrentWrite(key)) => assert_eq!(key, "config"),
        other => panic!("expected ConcurrentWrite, got {other:?}"),
    }
    // 磁碟上還是我們寫入的內容（竄改只發生在讀取路徑）——ConcurrentWrite 是把
    // 「讀回對不上」變成錯誤，不對磁碟做任何事
    assert_eq!(
        std::fs::read(dir.path().join("config")).unwrap(),
        b"real-config"
    );
}

#[tokio::test]
async fn read_back_ok_and_already_exists_pass_through() {
    let dir = tempfile::tempdir().unwrap();
    let b = Backend::local(&dir.path().join("plain")).unwrap();
    // 寫入成功、讀回一致 → Ok
    b.put_if_absent_verified("config", b"cfg".to_vec())
        .await
        .unwrap();
    // 已存在 → AlreadyExists 原樣傳回（不做讀回）
    match b.put_if_absent_verified("config", b"x".to_vec()).await {
        Err(BackendError::AlreadyExists(_)) => {}
        other => panic!("expected AlreadyExists, got {other:?}"),
    }
    assert_eq!(b.get("config").await.unwrap(), b"cfg");
}
