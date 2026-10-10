//! P0 的探針（format-v4.md §11.3）在不守規矩的後端上：真的後端很難穩定做出這些情況，所以用包裝
//! `InMemory` 的 store 模擬——忽略條件建立的 S3 相容服務、時鐘偏差的後端、rclone 橋接的位置。
//! 正常後端上的探針在 contract_v4.rs。

use std::fmt;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::BoxStream;
use futures::StreamExt as _;
use object_store::memory::InMemory;
use object_store::path::Path as StorePath;
use object_store::{
    GetOptions, GetResult, ListResult, ObjectMeta, ObjectStore, PutMode, PutOptions, PutPayload,
    PutResult,
};

use kist_backend::{Backend, BackendError, RepoLocation};

/// `ignore_create`：把只在不存在時建立當成覆寫（沒有文件的服務可能這樣）。
/// `shift`：回報的修改時間比真的晚這麼多秒（負的就是早）。
#[derive(Debug, Default)]
struct Misbehaving {
    inner: InMemory,
    ignore_create: bool,
    shift: i64,
    puts: AtomicUsize,
}

impl fmt::Display for Misbehaving {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Misbehaving")
    }
}

fn shifted(mut meta: ObjectMeta, shift: i64) -> ObjectMeta {
    meta.last_modified += chrono::Duration::seconds(shift);
    meta
}

#[async_trait]
impl ObjectStore for Misbehaving {
    async fn put_opts(
        &self,
        location: &StorePath,
        payload: PutPayload,
        mut opts: PutOptions,
    ) -> object_store::Result<PutResult> {
        self.puts.fetch_add(1, Ordering::Relaxed);
        if self.ignore_create {
            opts.mode = PutMode::Overwrite;
        }
        self.inner.put_opts(location, payload, opts).await
    }

    async fn put_multipart_opts(
        &self,
        location: &StorePath,
        opts: object_store::PutMultipartOptions,
    ) -> object_store::Result<Box<dyn object_store::MultipartUpload>> {
        self.inner.put_multipart_opts(location, opts).await
    }

    async fn get_opts(
        &self,
        location: &StorePath,
        options: GetOptions,
    ) -> object_store::Result<GetResult> {
        let mut result = self.inner.get_opts(location, options).await?;
        result.meta = shifted(result.meta, self.shift);
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
        let shift = self.shift;
        self.inner
            .list(prefix)
            .map(move |m| m.map(|m| shifted(m, shift)))
            .boxed()
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

fn backend(store: Arc<Misbehaving>, location: RepoLocation) -> Backend {
    Backend::from_store(store, location)
}

fn local() -> RepoLocation {
    RepoLocation::Local("/nowhere".into())
}

async fn leftovers(b: &Backend) -> usize {
    b.list("maint").await.unwrap().len()
}

#[tokio::test]
async fn a_backend_that_ignores_conditional_create_is_refused() {
    let store = Arc::new(Misbehaving {
        ignore_create: true,
        ..Default::default()
    });
    let b = backend(store, local());
    match b.probe().await {
        Err(BackendError::Probe(msg)) => assert!(msg.contains("second create"), "{msg}"),
        other => panic!("expected a probe refusal, got {other:?}"),
    }
    assert_eq!(leftovers(&b).await, 0, "探針沒有刪掉");
}

#[tokio::test]
async fn a_backend_clock_off_by_more_than_five_minutes_is_refused() {
    for shift in [10 * 60, -10 * 60] {
        let store = Arc::new(Misbehaving {
            shift,
            ..Default::default()
        });
        let b = backend(store, local());
        match b.probe().await {
            Err(BackendError::Probe(msg)) => assert!(msg.contains("clock"), "{msg}"),
            other => panic!("shift {shift}: expected a probe refusal, got {other:?}"),
        }
        assert_eq!(leftovers(&b).await, 0, "shift {shift}: 探針沒有刪掉");
    }
}

#[tokio::test]
async fn a_backend_clock_within_five_minutes_passes() {
    for shift in [4 * 60, -4 * 60] {
        let store = Arc::new(Misbehaving {
            shift,
            ..Default::default()
        });
        let b = backend(store, local());
        let p = b.probe().await.unwrap();
        let skew = (p.backend - p.local).whole_seconds();
        // 修改時間截到秒、兩次讀時鐘之間有延遲：只要求落在偏差附近
        assert!((skew - shift).abs() <= 2, "shift {shift}, skew {skew}");
        assert_eq!(leftovers(&b).await, 0, "shift {shift}: 探針沒有刪掉");
    }
}

#[tokio::test]
async fn the_rclone_bridge_is_refused_without_writing_anything() {
    let store = Arc::new(Misbehaving::default());
    let cfg = kist_backend::sftp::parse_rclone_url("rclone://remote/path").unwrap();
    let b = backend(store.clone(), RepoLocation::Rclone(cfg));
    assert!(!b.create_only());
    match b.probe().await {
        Err(BackendError::Probe(msg)) => assert!(msg.contains("stat + rename"), "{msg}"),
        other => panic!("expected a probe refusal, got {other:?}"),
    }
    assert_eq!(store.puts.load(Ordering::Relaxed), 0);
}
