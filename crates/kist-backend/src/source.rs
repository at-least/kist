//! 備份**來源**抽象（v3）：backup 走訪的對象不必是本機目錄——SFTP、S3
//! 都能當來源，client 當轉運（切塊、加密都在 client，金鑰不出機器）。
//!
//! 與 [`crate::Backend`]（repo 端）的分工：Backend 寫入的是 kist 格式物件
//! （conditional put、range read）；Source 提供的是「檔案系統形狀」的讀取
//! （列目錄、串流讀檔），metadata 依來源種類遞減——格式的 metadata 聯集
//! （docs/format.md §8）就是為此設計：「來源能證明什麼就記什麼」。
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
    /// 本地來源會另帶完整 POSIX metadata（由走訪端處理條目時 lstat 取得）。
    File {
        size: u64,
        /// 奈秒單位；精度依來源（SFTP 秒、S3 清單毫秒、本機奈秒）。
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
///
/// 刻意**不**攜帶完整 metadata：list 會把整個目錄（可能 100 萬條目）常駐
/// 記憶體（排序所需），每條目多幾十 bytes 就是上百 MiB（大 repo 記憶體
/// 門檻的回歸教訓）。本地來源的 ctime/inode/mode 等由走訪端在處理該
/// 條目時對路徑再 lstat 一次取得——每檔一次系統呼叫。
/// 已知極小視窗：條目的 kind 在 yield 時判定，走訪端第二次 lstat 之間
/// 檔案被換成別種型別的話，會以「當下的 metadata + 讀到的內容」收尾
/// （content 已驗 size 與 chunk；racy 變更由下一次備份的 ctime guard
/// 兜底）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceItem {
    /// 名稱（單一路徑元件；Unix = 原 OS bytes）。
    pub name: Vec<u8>,
    pub kind: SourceItemKind,
}

/// 一次目錄列舉：`list` 回傳的惰性迭代器，條目依名稱 bytes 排序。
/// 記憶體合約（大 repo 門檻，ADR 011）：實作**不得**在迭代開始前把
/// 條目 metadata 整批物化——本機來源只常駐排序後的名稱，每個條目的
/// 其餘欄位在 yield 當下取得。
pub trait SortedItems: Send {
    fn next_item(&mut self) -> Option<std::result::Result<SourceItem, BackendError>>;
}

/// 備份來源。名稱一律 bytes；`list` 回傳的條目依名稱 bytes 排序。
pub trait Source: Send + Sync + 'static {
    /// 掛進 `Root.path` 的來源定位字串。
    fn locator(&self) -> &[u8];
    /// [`kist_format::tree::meta_kind`] 的值。
    fn meta_kind(&self) -> u8;
    /// 列一個目錄（`[]` = 根）；回傳名稱排序的惰性條目迭代器。
    fn list(&self, dir: &[u8]) -> std::result::Result<Box<dyn SortedItems + Send>, BackendError>;
    /// 串流讀取一個檔案。
    fn read(&self, file: &[u8]) -> std::result::Result<Box<dyn Read + Send>, BackendError>;
    /// rel 路徑 → 本機檔案系統路徑。只有本機來源會有（走訪端處理條目時
    /// 對路徑 lstat 取得 posix metadata、讀 xattr）；遠端來源回 None。
    fn local_path(&self, _rel: &[u8]) -> Option<std::path::PathBuf> {
        None
    }
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
        let locator = crate::fsmeta::path_to_bytes(&abs).ok_or_else(|| {
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

    fn local_path(&self, rel: &[u8]) -> Option<std::path::PathBuf> {
        Some(self.join(rel))
    }

    fn meta_kind(&self) -> u8 {
        kist_format::tree::meta_kind::POSIX
    }

    fn list(&self, dir: &[u8]) -> std::result::Result<Box<dyn SortedItems + Send>, BackendError> {
        let path = self.join(dir);
        let rd = std::fs::read_dir(&path)
            .map_err(|e| BackendError::Source(format!("{}: {e}", path.display())))?;
        // 只常駐名稱（排序所需）；metadata 在 yield 時逐條 lstat——
        // 100 萬條目的目錄，名稱 ~45 MiB，條目結構會是它的數倍。
        let mut names: Vec<Vec<u8>> = Vec::new();
        for entry in rd {
            let entry =
                entry.map_err(|e| BackendError::Source(format!("{}: {e}", path.display())))?;
            names.push(crate::fsmeta::os_to_bytes(entry.file_name()));
        }
        names.sort();
        Ok(Box::new(LocalListing {
            root: path,
            names: names.into_iter(),
        }))
    }

    fn read(&self, file: &[u8]) -> std::result::Result<Box<dyn Read + Send>, BackendError> {
        let path = self.join(file);
        let f = std::fs::File::open(&path)
            .map_err(|e| BackendError::Source(format!("{}: {e}", path.display())))?;
        Ok(Box::new(f))
    }
}

/// 本機列舉：排序後的名稱 → 逐條 lstat 產生 SourceItem。
/// 走訪途中消失的條目被略過（與 backup 的容錯同一語意）。
struct LocalListing {
    root: std::path::PathBuf,
    names: std::vec::IntoIter<Vec<u8>>,
}

impl SortedItems for LocalListing {
    fn next_item(&mut self) -> Option<std::result::Result<SourceItem, BackendError>> {
        for name in self.names.by_ref() {
            let path = self.root.join(crate::fsmeta::bytes_to_os(&name));
            let Ok(meta) = std::fs::symlink_metadata(&path) else {
                continue; // 走訪途中消失：略過
            };
            let ft = meta.file_type();
            let kind = if ft.is_symlink() {
                let target = match std::fs::read_link(&path) {
                    Ok(t) => t,
                    Err(e) => {
                        return Some(Err(BackendError::Source(format!(
                            "{}: {e}",
                            path.display()
                        ))))
                    }
                };
                let Some(t) = crate::fsmeta::path_to_bytes(&target) else {
                    continue; // 無法表示的連結目標：略過
                };
                SourceItemKind::Symlink { target: t }
            } else if ft.is_dir() {
                SourceItemKind::Dir
            } else {
                SourceItemKind::File {
                    size: meta.len(),
                    mtime_ns: crate::fsmeta::mtime_ns_of(&meta),
                    etag: None,
                    vern: None,
                }
            };
            return Some(Ok(SourceItem { name, kind }));
        }
        None
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
                "{spec}: object-store source requires a tokio runtime context"
            ))
        })?;
        let locator = spec.as_bytes().to_vec();
        let (store, root, meta_kind) = if spec.starts_with("sftp://") {
            let cfg = sftp::parse_sftp_url(spec)
                .map_err(|e| BackendError::Source(format!("{spec}: {e}")))?;
            // 來源模式：listing 不做 dot-skip——列的是使用者的資料，
            // 不是 repo 命名空間（`.bashrc` 是內容，本地/s3 來源也列）。
            let store = sftp::SftpStore::open_source(&cfg, &sftp::auth_from_env()).await?;
            // SftpStore 內部已把 cfg.root（URL 的 path）當根：key 相對於它。
            // 來源的 root 因此是空（避免雙重前綴；rel 直接是 store key）。
            let root = object_store::path::Path::default();
            (
                Arc::new(store) as Arc<dyn ObjectStore>,
                root,
                kist_format::tree::meta_kind::SFTP,
            )
        } else if let Some(rest) = spec.strip_prefix("s3://") {
            let (bucket, prefix) = rest.split_once('/').unwrap_or((rest, ""));
            // 來源端用**裸 bucket**（不掛 PrefixStore；憑證走環境變數）：前綴由
            // self.root 負責拼接（to_store_path 會 join root+rel）。若再包
            // PrefixStore 會雙重前綴（實機 E2E 抓到：讀檔變成 prefix/prefix/key）。
            let store = crate::s3_store(bucket, "", None)?;
            let root = s3_source_root(spec, prefix)?;
            (store, root, kist_format::tree::meta_kind::S3)
        } else {
            return Err(BackendError::Source(format!(
                "{spec}: unsupported source scheme (want sftp:// or s3://)"
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
    ///
    /// 一律用 `Path::parse`（不編碼）：清單端產生名稱用的就是 parse
    /// （sftp.rs 的 to_meta 與 list_with_delimiter、object_store 的 S3 client），
    /// 讀檔與列子目錄必須送出同一個字串。`Path::from` 會把 `~ # % [ ]` 等
    /// 百分比編碼，同一個名字列出來與讀的時候變成兩個字串：檔讀不到，
    /// 子目錄被列成空的（ADR 019 A1）。名稱不合命名規則（控制字元、`.`、
    /// `..`）時回 Source 錯誤，由走訪端記進 skip 帳。
    fn to_store_path(&self, rel: &[u8]) -> Result<object_store::path::Path> {
        let joined = if self.root.as_ref().is_empty() {
            String::from_utf8_lossy(rel).into_owned()
        } else if rel.is_empty() {
            self.root.to_string()
        } else {
            format!("{}/{}", self.root, String::from_utf8_lossy(rel))
        };
        object_store::path::Path::parse(joined.as_str())
            .map_err(|e| BackendError::Source(format!("{joined}: {e}")))
    }

    /// HEAD prefix 本身（僅根列舉用）：存在 → 它是一顆「檔案來源」物件，
    /// 包成單一 File 條目；404 → None（目錄前綴，正常走清單）。
    fn head_root_file(&self) -> std::result::Result<Option<SourceItem>, BackendError> {
        // 來源的 store 是**裸 bucket**（無 PrefixStore）：root 就是完整前綴。
        // HEAD 完整前綴路徑——目錄來源 404（→ None，正常走清單）；檔案來源
        // 200（→ 單一 File 條目，走檔案來源分支）。
        let path = self.to_store_path(b"")?;
        let store = Arc::clone(&self.store);
        let head_path = path.clone();
        let meta = match self
            .handle
            .block_on(async move { store.head(&head_path).await })
        {
            Ok(m) => m,
            Err(object_store::Error::NotFound { .. }) => return Ok(None),
            Err(e) => return Err(BackendError::Source(format!("{path}: {e}"))),
        };
        let name = path.filename().unwrap_or("root").as_bytes().to_vec();
        Ok(Some(SourceItem {
            name,
            kind: file_kind(&meta),
        }))
    }
}

/// 遠端物件的 metadata → 檔案條目（大小、mtime 奈秒、etag／版本）。
fn file_kind(meta: &object_store::ObjectMeta) -> SourceItemKind {
    SourceItemKind::File {
        size: meta.size,
        mtime_ns: meta
            .last_modified
            .timestamp()
            .saturating_mul(1_000_000_000)
            .saturating_add(i64::from(meta.last_modified.timestamp_subsec_nanos())),
        etag: meta.e_tag.as_ref().map(|e| e.as_bytes().to_vec()),
        vern: meta.version.as_ref().map(|v| v.as_bytes().to_vec()),
    }
}

/// `s3://bucket/<prefix>` 的 prefix → 來源的 root。與 to_store_path 同理：
/// prefix 是遠端上的原始名稱，用 parse 不編碼（ADR 019 A1）。
/// 獨立成函式是為了不建 S3 client 就能測（建 client 會讀 `AWS_*` 環境變數）。
fn s3_source_root(spec: &str, prefix: &str) -> Result<object_store::path::Path> {
    object_store::path::Path::parse(prefix)
        .map_err(|e| BackendError::Source(format!("{spec}: {e}")))
}

impl Source for ObjectStoreSource {
    fn locator(&self) -> &[u8] {
        &self.locator
    }

    fn meta_kind(&self) -> u8 {
        self.meta_kind
    }

    fn list(&self, dir: &[u8]) -> std::result::Result<Box<dyn SortedItems + Send>, BackendError> {
        let prefix = self.to_store_path(dir)?;
        let store = Arc::clone(&self.store);
        // 根列舉時先 HEAD prefix 本身：若它是「檔案來源」（prefix 即一顆
        // 物件），list_with_delimiter 會把同名物件藏起來、回傳空清單——
        // 實機測試（MinIO）證實。此時回傳該物件單一條目，走訪端即可走
        // 檔案來源分支。
        // 守衛：定位以 `/` 收尾是
        // 「目錄」的明示，不能拿去 HEAD 同名物件——s3fs 一類工具會放
        // 0-byte folder marker，HEAD 200 會讓整個來源退化成單一空檔案、
        // 真正的內容全部不見；裸 bucket（空 root）也沒有可 HEAD 的鍵。
        // sftp 來源的 root 恆為空，本守衛因此不啟動。
        if dir.is_empty() && !self.root.as_ref().is_empty() && !self.locator.ends_with(b"/") {
            if let Some(item) = self.head_root_file()? {
                return Ok(Box::new(StoreListing {
                    items: vec![item].into_iter(),
                }));
            }
        }
        let result = self.handle.block_on(async move {
            store
                .list_with_delimiter(Some(&prefix))
                .await
                .map_err(|e| BackendError::Source(format!("{prefix}: {e}")))
        })?;
        let mut out = Vec::new();
        // 子目錄（common prefix）。
        for p in &result.common_prefixes {
            out.push(SourceItem {
                name: p.filename().unwrap_or("").as_bytes().to_vec(),
                kind: SourceItemKind::Dir,
            });
        }
        for meta in &result.objects {
            out.push(SourceItem {
                name: meta.location.filename().unwrap_or("").as_bytes().to_vec(),
                kind: file_kind(meta),
            });
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        // 遠端清單已整批在記憶體（伺服器回應的形狀）；迭代器只是包裝。
        Ok(Box::new(StoreListing {
            items: out.into_iter(),
        }))
    }

    fn read(&self, file: &[u8]) -> std::result::Result<Box<dyn Read + Send>, BackendError> {
        let path = self.to_store_path(file)?;
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
                let msg = chunk.map_err(|e| std::io::Error::other(e.to_string()));
                if tx.send(msg).await.is_err() {
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

/// 遠端列舉的迭代器包裝（清單已在 `list_with_delimiter` 回應裡整批到齊）。
struct StoreListing {
    items: std::vec::IntoIter<SourceItem>,
}

impl SortedItems for StoreListing {
    fn next_item(&mut self) -> Option<std::result::Result<SourceItem, BackendError>> {
        self.items.next().map(Ok)
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

/// `spec` 是不是遠端來源 URL（`sftp://`／`s3://`）；其他都是本機路徑。CLI 用
/// 同一條規則判斷 backup 的第一個路徑是不是遠端來源。
pub fn is_remote_spec(spec: &str) -> bool {
    spec.starts_with("sftp://") || spec.starts_with("s3://")
}

/// 從來源 URL 構造 Source：本機路徑或 `sftp://`/`s3://`。
pub async fn open_source(spec: &str) -> Result<Box<dyn Source>> {
    if is_remote_spec(spec) {
        return Ok(Box::new(ObjectStoreSource::open(spec).await?));
    }
    Ok(Box::new(LocalSource::new(std::path::PathBuf::from(spec))?))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn local_source_lists_sorted() {
        let dir = std::env::temp_dir().join(format!("kist-source-test-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("zsub")).unwrap();
        std::fs::write(dir.join("a.txt"), b"hello").unwrap();
        std::fs::write(dir.join("m.bin"), vec![0u8; 10]).unwrap();
        let src = LocalSource::new(dir.clone()).unwrap();
        assert!(src.locator().starts_with(b"/"), "locator 是絕對路徑");
        assert_eq!(src.meta_kind(), kist_format::tree::meta_kind::POSIX);

        let mut items = src.list(b"").unwrap();
        let mut names: Vec<Vec<u8>> = Vec::new();
        let mut file_kind = None;
        while let Some(item) = items.next_item() {
            let item = item.unwrap();
            if item.name == b"a.txt" {
                file_kind = Some(item.kind.clone());
            }
            names.push(item.name);
        }
        assert_eq!(
            names,
            vec![b"a.txt".to_vec(), b"m.bin".to_vec(), b"zsub".to_vec()]
        );

        let file_kind = file_kind.expect("a.txt must be listed");
        match &file_kind {
            SourceItemKind::File { size, mtime_ns, .. } => {
                assert_eq!(*size, 5);
                assert!(*mtime_ns != 0);
            }
            other => panic!("expected file, got {other:?}"),
        }

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
    fn local_source_reports_symlinks_with_targets() {
        let dir = std::env::temp_dir().join(format!("kist-source-symlink-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::os::unix::fs::symlink("target.txt", dir.join("link")).unwrap();
        let src = LocalSource::new(dir.clone()).unwrap();
        let mut items = src.list(b"").unwrap();
        let link = items
            .next_item()
            .map(|r| r.unwrap())
            .filter(|i| i.name == b"link")
            .expect("link must be listed");
        match &link.kind {
            SourceItemKind::Symlink { target } => assert_eq!(target, b"target.txt"),
            other => panic!("expected symlink, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn open_source_rejects_unknown_schemes() {
        assert!(open_source("gopher://x").await.is_err());
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod s3_root_probe_tests {
    use super::*;
    use object_store::path::Path as StorePath;

    /// `s3://bucket/data/`（尾巴的 `/`＝明示「這是目錄」）不可以拿去 HEAD
    /// 同名物件：s3fs 一類工具會放 0-byte folder marker，HEAD 200 會讓整個
    /// 來源退化成單一空檔案、`data/` 的真正內容全部不見。守衛：root 非空
    /// 且定位不以 `/` 收尾，才 HEAD。
    #[tokio::test]
    async fn trailing_slash_locator_skips_the_root_file_probe() {
        let store = object_store::memory::InMemory::new();
        store
            .put(
                &StorePath::from("data"),
                object_store::PutPayload::from_static(b""),
            )
            .await
            .unwrap();
        store
            .put(
                &StorePath::from("data/x.txt"),
                object_store::PutPayload::from_static(b"real content"),
            )
            .await
            .unwrap();
        let src = ObjectStoreSource {
            store: Arc::new(store),
            root: StorePath::from("data"),
            locator: b"s3://bucket/data/".to_vec(),
            meta_kind: kist_format::tree::meta_kind::S3,
            handle: tokio::runtime::Handle::current(),
        };
        // 生產端是從 blocking 執行緒呼叫 list（list 內部 block_on）；
        // 測試照同一個呼叫環境，不然 block_on 會在 runtime 執行緒上 panic。
        let mut listing = tokio::task::spawn_blocking(move || src.list(b"").unwrap())
            .await
            .unwrap();
        let mut items = Vec::new();
        while let Some(item) = listing.next_item() {
            items.push(item.unwrap());
        }
        // 退化（probe 命中）＝整個來源只有 folder-marker 一顆 0-byte
        // 「data」；守衛生效＝prefix 下的真實內容照常列出、marker 不見。
        assert!(
            items
                .iter()
                .any(|i| i.name == b"x.txt"
                    && matches!(i.kind, SourceItemKind::File { size: 12, .. })),
            "prefix 的真實內容必須照常列出，got {items:?}"
        );
        assert!(
            !items.iter().any(|i| i.name == b"data"),
            "folder marker 不得作為檔案來源出現，got {items:?}"
        );
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod store_path_tests {
    use super::*;
    use object_store::path::Path as StorePath;

    /// 走訪 `dir` 底下的所有檔案（遞迴），回傳 (rel, 讀到的內容或錯誤)。
    /// 錯誤收成字串而不是 unwrap：斷言失敗時要看得到是哪個名稱讀不到。
    fn walk_and_read(
        src: &ObjectStoreSource,
        dir: &[u8],
        out: &mut Vec<(String, std::result::Result<Vec<u8>, String>)>,
    ) {
        let mut listing = src.list(dir).unwrap();
        while let Some(item) = listing.next_item() {
            let item = item.unwrap();
            let mut rel = dir.to_vec();
            if !rel.is_empty() {
                rel.push(b'/');
            }
            rel.extend_from_slice(&item.name);
            match item.kind {
                SourceItemKind::Dir => walk_and_read(src, &rel, out),
                _ => {
                    let mut content = Vec::new();
                    let got = src.read(&rel).map_err(|e| e.to_string()).and_then(|mut r| {
                        r.read_to_end(&mut content)
                            .map(|_| content)
                            .map_err(|e| e.to_string())
                    });
                    out.push((String::from_utf8_lossy(&rel).into_owned(), got));
                }
            }
        }
    }

    /// 遠端上的名稱含 `~ [ ]`（ADR 019 A1）：清單端（S3 client、SFTP）用不
    /// 編碼的 `Path::parse` 產生名稱，讀檔與列子目錄必須用同一個字串。
    /// 用 `Path::from` 的話 `a~1.txt` 會變成 `a%7E1.txt`（讀不到）、`d[1]`
    /// 變成 `d%5B1%5D`（子目錄被列成空的，整棵靜默備成空目錄）。
    #[tokio::test]
    async fn names_with_reserved_chars_round_trip_from_list_to_read() {
        let store = object_store::memory::InMemory::new();
        // 以 parse 放入＝遠端上真實的名稱（S3 client 列出時也是 parse）。
        store
            .put(
                &StorePath::parse("a~1.txt").unwrap(),
                object_store::PutPayload::from_static(b"tilde"),
            )
            .await
            .unwrap();
        store
            .put(
                &StorePath::parse("d[1]/x").unwrap(),
                object_store::PutPayload::from_static(b"bracket"),
            )
            .await
            .unwrap();
        let src = ObjectStoreSource {
            store: Arc::new(store),
            root: StorePath::default(),
            locator: b"s3://bucket".to_vec(),
            meta_kind: kist_format::tree::meta_kind::S3,
            handle: tokio::runtime::Handle::current(),
        };
        // list 與讀取的橋接內部都 block_on：照生產端在 blocking 執行緒上跑。
        let got = tokio::task::spawn_blocking(move || {
            let mut out = Vec::new();
            walk_and_read(&src, b"", &mut out);
            out
        })
        .await
        .unwrap();
        assert_eq!(
            got,
            vec![
                ("a~1.txt".to_owned(), Ok(b"tilde".to_vec())),
                ("d[1]/x".to_owned(), Ok(b"bracket".to_vec())),
            ]
        );
    }

    /// `s3://bucket/<prefix>` 的 prefix 也是遠端上的原始名稱：root 必須原樣
    /// 保存，不能被編碼成 `photos%5B2024%5D`。直接測 s3_source_root，不走
    /// open：open 會建 S3 client、讀 `AWS_*` 環境變數，殘缺的環境（例如只設
    /// 了 AWS_ACCESS_KEY_ID）會讓這個純字串的測試誤失敗。
    #[test]
    fn s3_root_prefix_keeps_reserved_chars() {
        let root = s3_source_root("s3://bucket/photos[2024]", "photos[2024]").unwrap();
        assert_eq!(root.as_ref(), "photos[2024]");
        // 沒有 prefix（`s3://bucket`）＝整個 bucket，root 是空的。
        let root = s3_source_root("s3://bucket", "").unwrap();
        assert_eq!(root, StorePath::default());
    }

    /// 名稱不合 object_store 的命名規則（控制字元、`.`、`..`）時，回
    /// `BackendError::Source`，由走訪端記進 skip 帳，不送出另一個字串。
    #[tokio::test]
    async fn unrepresentable_name_is_a_source_error() {
        let src = ObjectStoreSource {
            store: Arc::new(object_store::memory::InMemory::new()),
            root: StorePath::default(),
            locator: b"s3://bucket".to_vec(),
            meta_kind: kist_format::tree::meta_kind::S3,
            handle: tokio::runtime::Handle::current(),
        };
        let (read, list) = tokio::task::spawn_blocking(move || {
            let read = src.read(b"bad\x01name").err();
            let list = src.list(b"d/..").err();
            (read, list)
        })
        .await
        .unwrap();
        assert!(
            matches!(read, Some(BackendError::Source(_))),
            "read: {read:?}"
        );
        assert!(
            matches!(list, Some(BackendError::Source(_))),
            "list: {list:?}"
        );
    }
}
