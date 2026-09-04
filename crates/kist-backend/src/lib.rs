//! kist 的儲存後端：把 `object_store` 收斂成 kist 需要的少數幾個操作。
//!
//! 刻意只暴露這些操作，因為權限模型依賴它們：
//! - backup 只用 `put` / `put_if_absent` / `get` / `get_range` / `list`；
//! - `delete` 只有 maintenance（prune）會用。
//!
//! 支援兩種位置（見 [`RepoLocation`]）：本機目錄，以及 `s3://bucket/prefix`（AWS S3 與
//! MinIO 等相容服務）。S3 的憑證與端點走 `object_store` 讀的環境變數：
//! `AWS_ACCESS_KEY_ID`、`AWS_SECRET_ACCESS_KEY`、`AWS_DEFAULT_REGION`、
//! `AWS_ENDPOINT`（MinIO 等自架服務）、`AWS_ALLOW_HTTP=true`（端點不是 https 時）。
//!
//! TLS：reqwest 用 rustls 但不帶 crypto provider，由這裡在建構時安裝 ring
//! （避免 aws-lc-sys 在 Windows 上需要 CMake + NASM）。

#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used)]

use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures::TryStreamExt;
use object_store::aws::AmazonS3Builder;
use object_store::path::Path as StorePath;
use object_store::prefix::PrefixStore;
use object_store::{ObjectStore, ObjectStoreExt, PutMode, PutOptions};

#[derive(Debug, thiserror::Error)]
pub enum BackendError {
    #[error("object not found: {0}")]
    NotFound(String),
    #[error("object already exists: {0}")]
    AlreadyExists(String),
    #[error("invalid object key {0:?}: {1}")]
    InvalidKey(String, String),
    #[error(
        "invalid repository location {0:?}: expected a directory path or s3://bucket[/prefix]"
    )]
    InvalidUrl(String),
    #[error("cannot open local repository at {path}: {source}")]
    LocalPath {
        path: String,
        source: std::io::Error,
    },
    #[error("storage error: {0}")]
    Store(#[from] object_store::Error),
    #[error("object {0} has an unrepresentable timestamp")]
    BadTimestamp(String),
}

/// list / head 回傳的物件資訊。`modified` 是後端記的最後修改時間
/// （本機 = 檔案 mtime；S3 = LastModified），GC 用它判斷物件夠不夠「老」。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectInfo {
    pub key: String,
    pub size: u64,
    pub modified: time::OffsetDateTime,
}

impl ObjectInfo {
    fn from_meta(m: object_store::ObjectMeta) -> Result<Self> {
        let key = m.location.to_string();
        let nanos = i128::from(m.last_modified.timestamp()) * 1_000_000_000
            + i128::from(m.last_modified.timestamp_subsec_nanos());
        let modified = time::OffsetDateTime::from_unix_timestamp_nanos(nanos)
            .map_err(|_| BackendError::BadTimestamp(key.clone()))?;
        Ok(Self {
            key,
            size: m.size,
            modified,
        })
    }
}

pub type Result<T> = std::result::Result<T, BackendError>;

/// repo 在哪裡：使用者給 `--repo` 的字串解析後的結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepoLocation {
    Local(PathBuf),
    S3 { bucket: String, prefix: String },
}

impl RepoLocation {
    /// `s3://bucket/prefix` → S3；其他任何字串都當本機路徑。
    pub fn parse(s: &str) -> Result<Self> {
        if let Some(rest) = s.strip_prefix("s3://") {
            let (bucket, prefix) = match rest.split_once('/') {
                Some((b, p)) => (b, p),
                None => (rest, ""),
            };
            if bucket.is_empty() {
                return Err(BackendError::InvalidUrl(s.to_owned()));
            }
            return Ok(Self::S3 {
                bucket: bucket.to_owned(),
                prefix: prefix.trim_matches('/').to_owned(),
            });
        }
        if s.contains("://") || s.is_empty() {
            return Err(BackendError::InvalidUrl(s.to_owned()));
        }
        Ok(Self::Local(PathBuf::from(s)))
    }

    pub fn is_remote(&self) -> bool {
        matches!(self, Self::S3 { .. })
    }
}

impl std::fmt::Display for RepoLocation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Local(p) => write!(f, "{}", p.display()),
            Self::S3 { bucket, prefix } if prefix.is_empty() => write!(f, "s3://{bucket}"),
            Self::S3 { bucket, prefix } => write!(f, "s3://{bucket}/{prefix}"),
        }
    }
}

#[derive(Clone)]
pub struct Backend {
    store: Arc<dyn ObjectStore>,
    location: RepoLocation,
}

impl std::fmt::Debug for Backend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Backend({})", self.location)
    }
}

impl Backend {
    /// 依 `--repo` 字串開後端。
    pub fn from_url(s: &str) -> Result<Self> {
        match RepoLocation::parse(s)? {
            RepoLocation::Local(p) => Self::local(&p),
            RepoLocation::S3 { bucket, prefix } => Self::s3(&bucket, &prefix),
        }
    }

    pub fn location(&self) -> &RepoLocation {
        &self.location
    }

    /// 本機目錄。不存在會建立。
    pub fn local(path: &Path) -> Result<Self> {
        std::fs::create_dir_all(path).map_err(|source| BackendError::LocalPath {
            path: path.display().to_string(),
            source,
        })?;
        let store = object_store::local::LocalFileSystem::new_with_prefix(path)?;
        Ok(Self {
            store: Arc::new(store),
            location: RepoLocation::Local(path.to_path_buf()),
        })
    }

    /// S3（或相容服務）。憑證與端點來自環境變數（見 crate 說明）。
    /// `prefix` 非空時所有 key 都放在它底下（`object_store` 的 `with_url` 不會自己套 prefix）。
    pub fn s3(bucket: &str, prefix: &str) -> Result<Self> {
        Self::s3_with(bucket, prefix, None)
    }

    /// 同 [`Self::s3`]，但憑證明確給定（不從環境變數讀）。端點與 region 仍來自環境變數。
    pub fn s3_with_credentials(
        bucket: &str,
        prefix: &str,
        access_key_id: &str,
        secret_access_key: &str,
    ) -> Result<Self> {
        Self::s3_with(bucket, prefix, Some((access_key_id, secret_access_key)))
    }

    fn s3_with(bucket: &str, prefix: &str, credentials: Option<(&str, &str)>) -> Result<Self> {
        install_tls_provider();
        let mut builder = AmazonS3Builder::from_env().with_bucket_name(bucket);
        if let Some((key, secret)) = credentials {
            builder = builder
                .with_access_key_id(key)
                .with_secret_access_key(secret);
        }
        let s3 = builder.build()?;
        let location = RepoLocation::S3 {
            bucket: bucket.to_owned(),
            prefix: prefix.to_owned(),
        };
        let store: Arc<dyn ObjectStore> = if prefix.is_empty() {
            Arc::new(s3)
        } else {
            Arc::new(PrefixStore::new(s3, Self::path(prefix)?))
        };
        Ok(Self { store, location })
    }

    /// 包任何 `object_store` 實作（測試用）。
    pub fn from_store(store: Arc<dyn ObjectStore>, location: RepoLocation) -> Self {
        Self { store, location }
    }

    fn path(key: &str) -> Result<StorePath> {
        StorePath::parse(key).map_err(|e| BackendError::InvalidKey(key.to_owned(), e.to_string()))
    }

    fn map_err(key: &str, e: object_store::Error) -> BackendError {
        match e {
            object_store::Error::NotFound { .. } => BackendError::NotFound(key.to_owned()),
            object_store::Error::AlreadyExists { .. } => {
                BackendError::AlreadyExists(key.to_owned())
            }
            other => BackendError::Store(other),
        }
    }

    /// 寫入（覆蓋既有物件）。只有 `config` 應該用這個。
    pub async fn put(&self, key: &str, bytes: Vec<u8>) -> Result<()> {
        self.store
            .put(&Self::path(key)?, bytes.into())
            .await
            .map_err(|e| Self::map_err(key, e))?;
        Ok(())
    }

    /// 只在物件不存在時寫入；已存在回 [`BackendError::AlreadyExists`]。snapshot 用這個。
    pub async fn put_if_absent(&self, key: &str, bytes: Vec<u8>) -> Result<()> {
        let opts = PutOptions {
            mode: PutMode::Create,
            ..Default::default()
        };
        self.store
            .put_opts(&Self::path(key)?, bytes.into(), opts)
            .await
            .map_err(|e| Self::map_err(key, e))?;
        Ok(())
    }

    pub async fn get(&self, key: &str) -> Result<Vec<u8>> {
        let result = self
            .store
            .get(&Self::path(key)?)
            .await
            .map_err(|e| Self::map_err(key, e))?;
        let bytes = result.bytes().await.map_err(|e| Self::map_err(key, e))?;
        Ok(bytes.to_vec())
    }

    pub async fn get_range(&self, key: &str, range: Range<u64>) -> Result<Vec<u8>> {
        let bytes = self
            .store
            .get_range(&Self::path(key)?, range)
            .await
            .map_err(|e| Self::map_err(key, e))?;
        Ok(bytes.to_vec())
    }

    /// 物件的大小與最後修改時間（HEAD）。
    pub async fn head(&self, key: &str) -> Result<ObjectInfo> {
        let meta = self
            .store
            .head(&Self::path(key)?)
            .await
            .map_err(|e| Self::map_err(key, e))?;
        ObjectInfo::from_meta(meta)
    }

    /// 物件大小（bytes）。
    pub async fn size(&self, key: &str) -> Result<u64> {
        Ok(self.head(key).await?.size)
    }

    pub async fn exists(&self, key: &str) -> Result<bool> {
        match self.size(key).await {
            Ok(_) => Ok(true),
            Err(BackendError::NotFound(_)) => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// 列出某個 prefix 底下的所有物件。順序不保證。
    pub async fn list(&self, prefix: &str) -> Result<Vec<ObjectInfo>> {
        let prefix = Self::path(prefix)?;
        let items: Vec<_> = self.store.list(Some(&prefix)).try_collect().await?;
        items.into_iter().map(ObjectInfo::from_meta).collect()
    }

    /// 刪除。只有 maintenance 操作會用。
    pub async fn delete(&self, key: &str) -> Result<()> {
        self.store
            .delete(&Self::path(key)?)
            .await
            .map_err(|e| Self::map_err(key, e))
    }
}

/// rustls 需要程序層級的 crypto provider；`install_default` 第二次會回 Err，忽略即可。
/// 公開給其他用 reqwest 的地方（webhook）用：不安裝的話 reqwest 會 panic。
pub fn install_tls_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}
