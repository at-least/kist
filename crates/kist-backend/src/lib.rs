//! kist 的儲存後端：把 `object_store` 收斂成 kist 需要的少數幾個操作。
//!
//! 刻意只暴露這些操作，因為權限模型依賴它們：
//! - backup 只用 `put` / `put_if_absent` / `get` / `get_range` / `list`；
//! - `delete` 只有 maintenance（prune）會用。
//!
//! M1 只有本機目錄後端；M2 透過 `object_store` 加上 S3。本地 index 快取也在 M2。

#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used)]

use std::ops::Range;
use std::sync::Arc;

use futures::TryStreamExt;
use object_store::path::Path as StorePath;
use object_store::{ObjectStore, ObjectStoreExt, PutMode, PutOptions};

#[derive(Debug, thiserror::Error)]
pub enum BackendError {
    #[error("object not found: {0}")]
    NotFound(String),
    #[error("object already exists: {0}")]
    AlreadyExists(String),
    #[error("invalid object key {0:?}: {1}")]
    InvalidKey(String, String),
    #[error("cannot open local repository at {path}: {source}")]
    LocalPath {
        path: String,
        source: std::io::Error,
    },
    #[error("storage error: {0}")]
    Store(#[from] object_store::Error),
}

pub type Result<T> = std::result::Result<T, BackendError>;

#[derive(Clone)]
pub struct Backend {
    store: Arc<dyn ObjectStore>,
}

impl std::fmt::Debug for Backend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Backend({})", self.store)
    }
}

impl Backend {
    /// 本機目錄。不存在會建立。
    pub fn local(path: &std::path::Path) -> Result<Self> {
        std::fs::create_dir_all(path).map_err(|source| BackendError::LocalPath {
            path: path.display().to_string(),
            source,
        })?;
        let store = object_store::local::LocalFileSystem::new_with_prefix(path)?;
        Ok(Self {
            store: Arc::new(store),
        })
    }

    /// 包任何 `object_store` 實作（測試或 M2 的 S3 用）。
    pub fn from_store(store: Arc<dyn ObjectStore>) -> Self {
        Self { store }
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

    /// 物件大小（bytes）。
    pub async fn size(&self, key: &str) -> Result<u64> {
        let meta = self
            .store
            .head(&Self::path(key)?)
            .await
            .map_err(|e| Self::map_err(key, e))?;
        Ok(meta.size)
    }

    pub async fn exists(&self, key: &str) -> Result<bool> {
        match self.size(key).await {
            Ok(_) => Ok(true),
            Err(BackendError::NotFound(_)) => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// 列出某個 prefix 底下的所有 (key, size)。順序不保證。
    pub async fn list(&self, prefix: &str) -> Result<Vec<(String, u64)>> {
        let prefix = Self::path(prefix)?;
        let items: Vec<_> = self.store.list(Some(&prefix)).try_collect().await?;
        Ok(items
            .into_iter()
            .map(|m| (m.location.to_string(), m.size))
            .collect())
    }

    /// 刪除。只有 maintenance 操作會用。
    pub async fn delete(&self, key: &str) -> Result<()> {
        self.store
            .delete(&Self::path(key)?)
            .await
            .map_err(|e| Self::map_err(key, e))
    }
}
