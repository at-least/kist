//! 備份**來源**抽象（v3）：backup 走訪的對象不必是本機目錄——SFTP、S3
//! 都能當來源，client 當轉運（切塊、加密都在 client，金鑰不出機器）。
//!
//! 與 [`crate::Backend`]（repo 端）的分工：Backend 寫入的是 kist 格式物件
//! （conditional put、range read）；Source 提供的是「檔案系統形狀」的讀取
//! （列目錄、串流讀檔），metadata 依來源種類遞減——格式的 metadata 聯集
//! （format-v3-draft §8）就是為此設計：「來源能證明什麼就記什麼」。
//!
//! 快速路徑合約（§8.2）：posix 來源用 ctime/inode（kernel 背書）；s3 來源
//! 用 etag（來源計算的內容指紋）；sftp/generic **沒有**安全快速路徑——
//! mtime 是來源聲稱的，不是可證明的。
//!
//! 讀取是 blocking [`std::io::Read`]：走訪的切塊程式在 blocking thread 上
//! 串流消費；遠端來源由內部的 async→sync 橋接餵資料（在同一個 tokio
//! runtime 的 blocking thread 上 `Handle::block_on` 是合法的）。

use std::io::Read;
use std::sync::Arc;

use object_store::{ObjectStore, ObjectStoreExt};

use crate::{sftp, BackendError, Result};

/// 來源條目的種類與它**能提供**的 metadata。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceItemKind {
    Dir,
    /// `posix` 只在本地來源出現（ctime/inode 等由 [`SourceItem::posix`] 帶）。
    File {
        size: u64,
        /// 秒精度（遠端來源只有秒；本地是奈秒）。
        mtime_ns: i64,
        /// 來源計算的內容指紋（S3 ETag 等）。
        etag: Option<Vec<u8>>,
        /// 來源物件版本 ID（S3 versioning）。
        vern: Option<Vec<u8>>,
    },
    /// 只有本機（與 SFTP 的 readlink，v1 未做）來源有符號連結。
    Symlink {
        target: Vec<u8>,
    },
}

/// 目錄裡的一個條目。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceItem {
    /// 名稱（單一路徑元件；Unix = 原 OS bytes）。
    pub name: Vec<u8>,
    pub kind: SourceItemKind,
    /// 本機來源的完整 POSIX metadata（快速路徑用）；遠端來源 = None。
    pub posix: Option<crate::fsmeta::PosixMeta>,
}

/// 備份來源。名稱一律 bytes；`list` 回傳的條目依名稱 bytes 排序。
pub trait Source: Send + Sync + 'static {
    /// 掛進 `Root.path` 的來源定位字串。
    fn locator(&self) -> &[u8];
    /// [`kist_format::tree::meta_kind`] 的值。
    fn meta_kind(&self) -> u8;
    /// 列一個目錄（`[]` = 根）；回傳名稱排序的條目。
    fn list(&self, dir: &[u8]) -> std::result::Result<Vec<SourceItem>, BackendError>;
    /// 串流讀取一個檔案。
    fn read(&self, file: &[u8]) -> std::result::Result<Box<dyn Read + Send>, BackendError>;
}

// ---------------------------------------------------------------------------
// 本機來源
// ---------------------------------------------------------------------------

/// 本機目錄/檔案來源：完整的 POSIX metadata（mtime/ctime/inode/dev/nlink、
/// `user.*` xattr 由走訪端在 posix 條目上讀取）。
pub struct LocalSource {
    root: std::path::PathBuf,
    locator: Vec<u8>,
}

impl LocalSource {
    pub fn new(root: std::path::PathBuf) -> Result<Self> {
        let abs = std::fs::canonicalize(&root)
            .map_err(|e| BackendError::Source(format!("{}: {e}", root.display())))?;
        let locator = crate::fsmeta::path_to_bytes(&abs).map_err(|_| {
            BackendError::Source(format!("{}: unrepresentable path", abs.display()))
        })?;
        Ok(Self { root: abs, locator })
    }

    fn join(&self, rel: &[u8]) -> std::path::PathBuf {
        let mut p = self.root.clone();
        for comp in rel.split(|&b| b == b'/') {
            if comp.is_empty() {
                continue;
            }
            p.push(crate::fsmeta::bytes_to_os(comp));
        }
        p
    }
}

impl Source for LocalSource {
    fn locator(&self) -> &[u8] {
        &self.locator
    }

    fn meta_kind(&self) -> u8 {
        kist_format::tree::meta_kind::POSIX
    }

    fn list(&self, dir: &[u8]) -> std::result::Result<Vec<SourceItem>, BackendError> {
        let path = self.join(dir);
        let rd = std::fs::read_dir(&path)
            .map_err(|e| BackendError::Source(format!("{}: {e}", path.display())))?;
        let mut out = Vec::new();
        for entry in rd {
            let entry =
                entry.map_err(|e| BackendError::Source(format!("{}: {e}", path.display())))?;
            let name = crate::fsmeta::os_to_bytes(entry.file_name());
            let meta = match std::fs::symlink_metadata(entry.path()) {
                Ok(m) => m,
                // 走訪途中消失：略過這一個條目，其他兄弟照常列出。
                Err(_) => {
                    tracing::debug!(
                        "{}: vanished during listing, skipped",
                        entry.path().display()
                    );
                    continue;
                }
            };
            let ft = meta.file_type();
            let kind = if ft.is_symlink() {
                // readlink 讀不到（競態刪除、權限）：同樣只略過這一個條目，
                // 不讓整個目錄的列舉失敗（與走訪端「單一項目讀不到不擋整個
                // 目錄」同一語意）。
                let Ok(target) = std::fs::read_link(entry.path()) else {
                    tracing::warn!("{}: cannot read symlink, skipped", entry.path().display());
                    continue;
                };
                SourceItemKind::Symlink {
                    target: crate::fsmeta::path_to_bytes(&target).map_err(|_| {
                        BackendError::Source(format!(
                            "{}: unrepresentable symlink target",
                            entry.path().display()
                        ))
                    })?,
                }
            } else if ft.is_dir() {
                SourceItemKind::Dir
            } else {
                let fs = crate::fsmeta::capture(&meta);
                SourceItemKind::File {
                    size: meta.len(),
                    mtime_ns: fs.mtime_ns,
                    etag: None,
                    vern: None,
                }
            };
            // posix 來源的每個條目（symlink 也不例外）都帶 lstat 的完整
            // metadata：§8.1 的 posix kind 必填 mode/uid/gid/mtime，缺了會被
            // 讀取端的 Entry::validate 拒絕。symlink 的 posix 是連結本身的。
            let posix = Some(crate::fsmeta::capture(&meta));
            out.push(SourceItem { name, kind, posix });
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    fn read(&self, file: &[u8]) -> std::result::Result<Box<dyn Read + Send>, BackendError> {
        let path = self.join(file);
        let f = std::fs::File::open(&path)
            .map_err(|e| BackendError::Source(format!("{}: {e}", path.display())))?;
        Ok(Box::new(f))
    }
}

// ---------------------------------------------------------------------------
// 物件儲存來源（SFTP / S3；共用 object_store 抽象）
// ---------------------------------------------------------------------------

/// 任何 `object_store` 實作當來源：SFTP（mtime/size；無快速路徑）與 S3
/// （mtime/size/etag/version；etag 快速路徑）。列出是一層一層的
///（`list_with_delimiter`）；讀取把 async 串流橋接成 blocking Read。
pub struct ObjectStoreSource {
    store: Arc<dyn ObjectStore>,
    /// 來源 root 的 store 路徑（空 = bucket 根）。
    root: object_store::path::Path,
    locator: Vec<u8>,
    meta_kind: u8,
    handle: tokio::runtime::Handle,
}

impl ObjectStoreSource {
    /// 從 URL 構造：`sftp://host[:port]/abs/path` 或 `s3://bucket/prefix`。
    pub async fn open(spec: &str) -> Result<Self> {
        let handle = tokio::runtime::Handle::try_current().map_err(|_| {
            BackendError::Source(format!(
                "{}: {}",
                spec.to_owned(),
                "object-store source requires a tokio runtime context".to_owned()
            ))
        })?;
        let (store, root, locator, meta_kind) = if let Some(rest) = spec.strip_prefix("sftp://") {
            let cfg = sftp::parse_sftp_url(spec)
                .map_err(|e| BackendError::Source(format!("{}: {e}", spec.to_owned())))?;
            let store = sftp::SftpStore::open(&cfg, &sftp::auth_from_env()).await?;
            // root 目錄 = URL 的 path 部分（SftpStore 內部已帶 cfg.root 的
            // 語意不同——這裡的 root 是「來源」的前綴，相對路徑從它算）。
            let url_path = sftp_url_path(rest);
            let root = object_store::path::Path::from(url_path.as_str());
            (
                Arc::new(store) as Arc<dyn ObjectStore>,
                root,
                spec.as_bytes().to_vec(),
                kist_format::tree::meta_kind::SFTP,
            )
        } else if let Some(rest) = spec.strip_prefix("s3://") {
            let (bucket, prefix) = match rest.split_once('/') {
                Some((b, p)) => (b.to_owned(), p.to_owned()),
                None => (rest.to_owned(), String::new()),
            };
            let store = s3_store_for_prefix(&bucket, &prefix)?;
            let root = object_store::path::Path::from(prefix.as_str());
            (
                store,
                root,
                spec.as_bytes().to_vec(),
                kist_format::tree::meta_kind::S3,
            )
        } else {
            return Err(BackendError::Source(format!(
                "{}: {}",
                spec.to_owned(),
                "unsupported source scheme (want sftp:// or s3://)".to_owned()
            )));
        };
        Ok(Self {
            store,
            root,
            locator,
            meta_kind,
            handle,
        })
    }

    /// 相對路徑 → store 路徑（root 底下）。
    fn to_store_path(&self, rel: &[u8]) -> object_store::path::Path {
        let joined = if self.root.as_ref().is_empty() {
            String::from_utf8_lossy(rel).into_owned()
        } else if rel.is_empty() {
            self.root.to_string()
        } else {
            format!("{}/{}", self.root, String::from_utf8_lossy(rel))
        };
        object_store::path::Path::from(joined.as_str())
    }
}

/// `sftp://` URL 的 path 部分（host[:port] 之後）。
fn sftp_url_path(after_host: &str) -> String {
    match after_host.find('/') {
        Some(i) => after_host[i..].to_owned(),
        None => "/".to_owned(),
    }
}

/// S3 store（bucket + 可選 prefix）；與 `Backend::s3_with` 同一構造。
/// 來源端的憑證走環境變數（與 repo 端的預設一致）。
fn s3_store_for_prefix(bucket: &str, prefix: &str) -> Result<Arc<dyn ObjectStore>> {
    crate::s3_store(bucket, prefix, None)
}

impl Source for ObjectStoreSource {
    fn locator(&self) -> &[u8] {
        &self.locator
    }

    fn meta_kind(&self) -> u8 {
        self.meta_kind
    }

    fn list(&self, dir: &[u8]) -> std::result::Result<Vec<SourceItem>, BackendError> {
        let prefix = self.to_store_path(dir);
        let store = Arc::clone(&self.store);
        let result = self.handle.block_on(async move {
            store
                .list_with_delimiter(Some(&prefix))
                .await
                .map_err(|e| BackendError::Source(format!("{}: {e}", prefix)))
        })?;
        let mut out = Vec::new();
        // 子目錄（common prefix）。
        for p in &result.common_prefixes {
            let name = p
                .as_ref()
                .rsplit('/')
                .next()
                .unwrap_or("")
                .as_bytes()
                .to_vec();
            out.push(SourceItem {
                name,
                kind: SourceItemKind::Dir,
                posix: None,
            });
        }
        for meta in &result.objects {
            let name = meta
                .location
                .as_ref()
                .rsplit('/')
                .next()
                .unwrap_or("")
                .as_bytes()
                .to_vec();
            out.push(SourceItem {
                name,
                kind: SourceItemKind::File {
                    size: meta.size,
                    mtime_ns: meta
                        .last_modified
                        .timestamp()
                        .saturating_mul(1_000_000_000)
                        .saturating_add(i64::from(meta.last_modified.timestamp_subsec_nanos())),
                    etag: meta.e_tag.as_ref().map(|e| e.as_bytes().to_vec()),
                    vern: meta.version.as_ref().map(|v| v.as_bytes().to_vec()),
                },
                posix: None,
            });
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    fn read(&self, file: &[u8]) -> std::result::Result<Box<dyn Read + Send>, BackendError> {
        let path = self.to_store_path(file);
        let store = Arc::clone(&self.store);
        let (tx, rx) = tokio::sync::mpsc::channel::<std::io::Result<bytes::Bytes>>(4);
        self.handle.spawn(async move {
            let get = match store.get(&path).await {
                Ok(g) => g,
                Err(e) => {
                    let _ = tx.send(Err(std::io::Error::other(e.to_string()))).await;
                    return;
                }
            };
            use futures::StreamExt;
            let mut stream = std::pin::pin!(get.into_stream());
            while let Some(chunk) = stream.next().await {
                let ok = match chunk {
                    Ok(bytes) => tx.send(Ok(bytes)).await.is_ok(),
                    Err(e) => tx
                        .send(Err(std::io::Error::other(e.to_string())))
                        .await
                        .is_ok(),
                };
                if !ok {
                    return; // 讀端已放棄
                }
            }
        });
        Ok(Box::new(StreamBridge {
            handle: self.handle.clone(),
            rx,
            buf: bytes::Bytes::new(),
            pos: 0,
        }))
    }
}

/// async 位元組流 → blocking [`Read`] 的橋接：spawn 的 pump task 把串流推進
/// channel，讀端在 blocking thread 上 `Handle::block_on` 等下一塊。
struct StreamBridge {
    handle: tokio::runtime::Handle,
    rx: tokio::sync::mpsc::Receiver<std::io::Result<bytes::Bytes>>,
    buf: bytes::Bytes,
    pos: usize,
}

impl Read for StreamBridge {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        loop {
            if self.pos < self.buf.len() {
                let n = (self.buf.len() - self.pos).min(out.len());
                out[..n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
                self.pos += n;
                return Ok(n);
            }
            match self.handle.block_on(self.rx.recv()) {
                Some(Ok(b)) if !b.is_empty() => {
                    self.buf = b;
                    self.pos = 0;
                }
                Some(Ok(_)) => continue, // 空 chunk：等下一個
                Some(Err(e)) => return Err(e),
                None => return Ok(0), // 串流結束
            }
        }
    }
}

/// 從來源 URL 構造 Source：本機路徑或 `sftp://`/`s3://`。
pub async fn open_source(spec: &str) -> Result<Box<dyn Source>> {
    if spec.starts_with("sftp://") || spec.starts_with("s3://") {
        return Ok(Box::new(ObjectStoreSource::open(spec).await?));
    }
    Ok(Box::new(LocalSource::new(std::path::PathBuf::from(spec))?))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn local_source_lists_sorted_with_posix_metadata() {
        let dir = std::env::temp_dir().join(format!("kist-source-test-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("zsub")).unwrap();
        std::fs::write(dir.join("a.txt"), b"hello").unwrap();
        std::fs::write(dir.join("m.bin"), vec![0u8; 10]).unwrap();
        let src = LocalSource::new(dir.clone()).unwrap();
        assert!(src.locator().starts_with(b"/"), "locator 是絕對路徑");
        assert_eq!(src.meta_kind(), kist_format::tree::meta_kind::POSIX);

        let items = src.list(b"").unwrap();
        let names: Vec<&[u8]> = items.iter().map(|i| i.name.as_slice()).collect();
        assert_eq!(names, vec![b"a.txt".as_slice(), b"m.bin", b"zsub"]);

        let file = items.iter().find(|i| i.name == b"a.txt").unwrap();
        match &file.kind {
            SourceItemKind::File { size, mtime_ns, .. } => {
                assert_eq!(*size, 5);
                assert!(*mtime_ns != 0);
            }
            other => panic!("expected file, got {other:?}"),
        }
        assert!(file.posix.is_some(), "本地來源要帶 posix metadata");

        let mut content = String::new();
        src.read(b"a.txt")
            .unwrap()
            .read_to_string(&mut content)
            .unwrap();
        assert_eq!(content, "hello");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn local_source_symlinks_carry_posix_metadata() {
        let dir = std::env::temp_dir().join(format!("kist-source-symlink-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::os::unix::fs::symlink("target.txt", dir.join("link")).unwrap();
        let src = LocalSource::new(dir.clone()).unwrap();
        let items = src.list(b"").unwrap();
        let link = items.iter().find(|i| i.name == b"link").unwrap();
        match &link.kind {
            SourceItemKind::Symlink { target } => assert_eq!(target, b"target.txt"),
            other => panic!("expected symlink, got {other:?}"),
        }
        // §8.1：posix kind 的每個條目都要 mode/uid/gid/mtime——缺了會被
        // 讀取端的 Entry::validate 拒絕。
        assert!(link.posix.is_some(), "symlink 條目也要帶 posix metadata");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn open_source_rejects_unknown_schemes() {
        assert!(open_source("gopher://x").await.is_err());
    }
}
