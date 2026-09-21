//! mount 的核心邏輯：inode 表、snapshot 瀏覽、chunk 隨機讀。**不知道 FUSE 的存在**
//! —— [`FsCore`] 吃 `Repository`、吐 `Attr`/`Listing`/bytes，`crate::fuse` 只做
//! fuser 回覆的轉接；測試直接開 `FsCore`，不用真的掛載。
//!
//! 佈局（對齊 Go 參考實作）：`<client id hex>/<timestamp>/<備份的樹>`。頂兩層
//! volatile（每次看都重列、TTL 1 秒）、snapshot 內容 immutable（內容定址，TTL
//! 24 小時）。看到沒見過的 snapshot key 就主動重載一次 index——新 snapshot 可能
//! 引用 mount 當下那份 index 沒有的 pack（限流 1 秒一次，key 一律記為「看過」，
//! 重載失敗也不能讓每次瀏覽都變成 reload 風暴）。

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use kist_core::restore::ReloadableIndex;
use kist_core::{CoreError, Repository};
use kist_format::keys;
use kist_format::tree::{content_type, node_type, ChunkList, Entry};
use kist_format::{cbor, ChunkId};

use crate::offsets::ChunkOffsets;
use crate::vpath::{RootContents, VEntry, VirtualRoot};

/// 頂兩層（client、timestamp 列表）的快取期限。
pub const VOLATILE_TTL: Duration = Duration::from_secs(1);
/// snapshot 內容（內容定址、不可變）的快取期限。
pub const IMMUTABLE_TTL: Duration = Duration::from_secs(24 * 3600);

/// mount 設定。`chunk_cache` = 解密後 chunk 的快取顆數（0 = 8；8 MiB chunk 時
/// 上限 64 MiB）。
#[derive(Debug, Clone)]
pub struct MountConfig {
    pub chunk_cache: usize,
}

impl Default for MountConfig {
    fn default() -> Self {
        Self { chunk_cache: 8 }
    }
}

/// FUSE 無關的檔案種類。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    File,
    Dir,
    Symlink,
}

/// 平台無關的檔案屬性（fuse.rs 轉成 fuser 的 `FileAttr`）。
#[derive(Debug, Clone, Copy)]
pub struct Attr {
    pub kind: Kind,
    pub perm: u32,
    pub size: u64,
    pub uid: u32,
    pub gid: u32,
    pub mtime_ns: i64,
    pub ctime_ns: i64,
}

impl Attr {
    fn dir(perm: u32, mtime_ns: i64) -> Self {
        Self {
            kind: Kind::Dir,
            perm,
            size: 0,
            uid: 0,
            gid: 0,
            mtime_ns,
            ctime_ns: mtime_ns,
        }
    }
}

/// 錯誤：fuse.rs 對映到 errno（`NotFound` → ENOENT、`EroFs` → EROFS、其餘 EIO）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsError {
    NotFound,
    Io,
    InvalidInput,
    BadHandle,
}

/// readdir 的一筆項目。
#[derive(Debug, Clone)]
pub struct DirEntryData {
    pub name: Vec<u8>,
    pub kind: Kind,
}

/// opendir 當下拍下的目錄快照：之後的 readdir 分頁都從這份讀，
/// 新 snapshot 掃到一半出現也不會重複或漏掉。
#[derive(Debug)]
pub struct Listing {
    pub entries: Vec<DirEntryData>,
}

/// lookup 的結果：inode 號、屬性、這個名字該用的 TTL（entry 與 attr 共用）。
#[derive(Debug, Clone)]
pub struct Lookup {
    pub ino: u64,
    pub attr: Attr,
    pub ttl: Duration,
}

/// 檔案讀取用的已解析狀態：資料 chunk 清單 + 邊界表。
struct ResolvedFile {
    chunks: Arc<[ChunkId]>,
    offsets: Arc<ChunkOffsets>,
}

enum Node {
    Root,
    Client {
        client: String,
    },
    SnapshotRoot {
        vroot: Arc<VirtualRoot>,
    },
    /// 絕對路徑根名的中介目錄（repo 裡沒有它）。
    SyntheticDir {
        vroot: Arc<VirtualRoot>,
        level_key: Vec<u8>,
    },
    Dir {
        entries: Arc<Vec<Entry>>,
    },
    File {
        chunks: Arc<[ChunkId]>,
        content: u8,
        resolved: tokio::sync::OnceCell<ResolvedFile>,
    },
    Symlink {
        target: Arc<[u8]>,
    },
}

struct Inode {
    node: Node,
    attr: Attr,
    volatile: bool,
    xattrs: Option<Arc<BTreeMap<Vec<u8>, Vec<u8>>>>,
}

#[derive(Default)]
struct InodeTable {
    next: u64,
    map: HashMap<u64, Arc<Inode>>,
}

impl InodeTable {
    fn with_root() -> Self {
        // insert 先加號再配號：next 從 0 起跳，根目錄才會拿到 FUSE 慣例的 ino 1。
        let mut t = Self {
            next: 0,
            map: HashMap::new(),
        };
        t.insert(Node::Root, Attr::dir(0o555, 0), true, None);
        t
    }

    /// 配一個全新的 inode 號：**永不重用**，所以 generation 恆為 0、
    /// forget 可以是 no-op（記憶體上限 = 這個 session 瀏覽過的條目數）。
    fn insert(
        &mut self,
        node: Node,
        attr: Attr,
        volatile: bool,
        xattrs: Option<Arc<BTreeMap<Vec<u8>, Vec<u8>>>>,
    ) -> u64 {
        self.next += 1;
        let ino = self.next;
        self.map.insert(
            ino,
            Arc::new(Inode {
                node,
                attr,
                volatile,
                xattrs,
            }),
        );
        ino
    }
}

/// 解密後 chunk 的小 LRU（`cap` 顆；8 MiB chunk × 8 = 64 MiB 上限）。
struct Lru {
    cap: usize,
    map: HashMap<ChunkId, bytes::Bytes>,
    order: VecDeque<ChunkId>,
}

impl Lru {
    fn new(cap: usize) -> Self {
        Self {
            cap: cap.max(1),
            map: HashMap::new(),
            order: VecDeque::new(),
        }
    }

    fn get(&mut self, id: &ChunkId) -> Option<bytes::Bytes> {
        let data = self.map.get(id)?.clone();
        self.touch(id);
        Some(data)
    }

    fn put(&mut self, id: ChunkId, data: bytes::Bytes) {
        if self.map.contains_key(&id) {
            self.touch(&id);
            return;
        }
        self.map.insert(id, data);
        self.order.push_front(id);
        while self.order.len() > self.cap {
            let victim = self.order.pop_back().expect("order 與 map 同步");
            self.map.remove(&victim);
        }
    }

    fn touch(&mut self, id: &ChunkId) {
        if let Some(pos) = self.order.iter().position(|c| c == id) {
            self.order.remove(pos);
            self.order.push_front(*id);
        }
    }
}

/// mount 的核心狀態。Sync：fuser 0.18 的 callback 是 `&self`、可多執行緒併發；
/// 內部鎖都是短區間（拿鎖 → clone → 放鎖 → await），絕不跨 `.await`。
pub struct FsCore {
    repo: Repository,
    index: ReloadableIndex,
    lru: Mutex<Lru>,
    inodes: Mutex<InodeTable>,
    listings: Mutex<(u64, HashMap<u64, Arc<Listing>>)>,
    seen: Mutex<Seen>,
}

#[derive(Default)]
struct Seen {
    keys: HashSet<String>,
    last_reload: Option<Instant>,
}

const RELOAD_EAGER_MIN_INTERVAL: Duration = Duration::from_secs(1);

impl FsCore {
    pub async fn new(repo: Repository, config: MountConfig) -> Result<Self, CoreError> {
        let index = repo.load_index().await?;
        Ok(Self {
            repo,
            index: ReloadableIndex::new(index),
            lru: Mutex::new(Lru::new(config.chunk_cache)),
            inodes: Mutex::new(InodeTable::with_root()),
            listings: Mutex::new((0, HashMap::new())),
            seen: Mutex::new(Seen::default()),
        })
    }

    fn node(&self, ino: u64) -> Option<Arc<Inode>> {
        self.inodes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .map
            .get(&ino)
            .cloned()
    }

    // —— 瀏覽 ——

    /// 看過的 snapshot keys；有新的就重載 index（限流 1 秒）。key 一律記為
    /// 「看過」——重載失敗也不能讓每次瀏覽都重試。
    async fn refresh_if_unseen(&self, keys: &[String]) {
        let fresh: Vec<String> = {
            let mut seen = self.seen.lock().unwrap_or_else(|e| e.into_inner());
            let fresh: Vec<String> = keys
                .iter()
                .filter(|k| !seen.keys.contains(*k))
                .cloned()
                .collect();
            for k in &fresh {
                seen.keys.insert(k.clone());
            }
            drop(seen);
            fresh
        };
        if fresh.is_empty() {
            return;
        }
        tracing::debug!(
            count = fresh.len(),
            "new snapshots appeared; reloading the index"
        );
        let due = {
            let mut seen = self.seen.lock().unwrap_or_else(|e| e.into_inner());
            let due = seen
                .last_reload
                .is_none_or(|t| t.elapsed() >= RELOAD_EAGER_MIN_INTERVAL);
            if due {
                seen.last_reload = Some(Instant::now());
            }
            due
        };
        if due {
            if let Ok(fresh_index) = self.repo.load_index().await {
                self.index.store(fresh_index).await;
            }
        }
    }

    async fn snapshot_keys_for(&self, client: Option<&str>) -> Result<Vec<String>, FsError> {
        let keys = self
            .repo
            .list_snapshot_keys()
            .await
            .map_err(|_| FsError::Io)?;
        self.refresh_if_unseen(&keys).await;
        let prefix = match client {
            Some(c) => format!("{}/{}/", keys::SNAPSHOTS_PREFIX, c),
            None => String::new(),
        };
        Ok(keys
            .into_iter()
            .filter(|k| k.starts_with(&prefix))
            .collect())
    }

    async fn list_clients(&self) -> Result<Vec<String>, FsError> {
        let keys = self.snapshot_keys_for(None).await?;
        let mut clients: Vec<String> = keys
            .iter()
            .filter_map(|k| {
                k.strip_prefix(keys::SNAPSHOTS_PREFIX)
                    .and_then(|s| s.strip_prefix('/'))
                    .and_then(|s| s.split('/').next())
                    .map(str::to_owned)
            })
            .collect();
        clients.sort();
        clients.dedup();
        Ok(clients)
    }

    // —— lookup ——

    pub async fn lookup(&self, parent: u64, name: &[u8]) -> Result<Lookup, FsError> {
        let parent = self.node(parent).ok_or(FsError::NotFound)?;
        match &parent.node {
            Node::Root => {
                let client = std::str::from_utf8(name).map_err(|_| FsError::NotFound)?;
                let clients = self.list_clients().await?;
                if !clients.iter().any(|c| c == client) {
                    return Err(FsError::NotFound);
                }
                let attr = Attr::dir(0o555, 0);
                let ino = {
                    self.inodes
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .insert(
                            Node::Client {
                                client: client.to_owned(),
                            },
                            attr,
                            true,
                            None,
                        )
                };
                Ok(Lookup {
                    ino,
                    attr,
                    ttl: VOLATILE_TTL,
                })
            }
            Node::Client { client } => {
                if !looks_like_timestamp(name) {
                    return Err(FsError::NotFound);
                }
                let ts = String::from_utf8_lossy(name);
                let key = format!("{}/{}/{}", keys::SNAPSHOTS_PREFIX, client, ts);
                let info = match self.repo.read_snapshot_by_key(&key).await {
                    Ok(i) => i,
                    Err(CoreError::SnapshotNotFound(_)) => return Err(FsError::NotFound),
                    Err(_) => return Err(FsError::Io),
                };
                // v3：roots 的定位字串不是單一組件——展開成虛擬層級。
                // 每個 root 讀 tree 判別形態：恰好一個非目錄 entry 且名稱 =
                // 定位末段 → 檔案/symlink 來源（葉子 = 那個 entry，與 restore
                // 落點一致）；定位沒有組件（`/`）→ 內容攤平到頂層；否則目錄
                // 來源（葉子 = 攜帶 subtree 的合成 DIR，children 懶載入）。
                let mut pairs = Vec::with_capacity(info.roots.len());
                for root in &info.roots {
                    let comps: Vec<&[u8]> = root
                        .path
                        .as_slice()
                        .split(|&b| b == b'/')
                        .filter(|c| !c.is_empty() && *c != b".")
                        .collect();
                    let contents = match comps.split_last() {
                        // 定位沒有組件：攤平（`kist backup /` 的 v3 形態）。
                        None => self
                            .repo
                            .read_tree_chain(&root.tree)
                            .await
                            .map(RootContents::Flatten)
                            .unwrap_or(RootContents::Dir),
                        Some((last, _)) => {
                            let leaf = match self.repo.read_tree_chain(&root.tree).await {
                                Ok(entries) => matches!(
                                    entries.as_slice(),
                                    [e] if e.kind != node_type::DIR && e.name == *last
                                )
                                .then(|| Box::new(entries[0].clone())),
                                Err(_) => None,
                            };
                            match leaf {
                                Some(e) => RootContents::Leaf(e),
                                None => RootContents::Dir,
                            }
                        }
                    };
                    pairs.push((root.clone(), contents));
                }
                // 敵意 locator（NUL 等非法組件）在這裡拒成 I/O 錯——
                // 與 Go 的 mount Lookup（ErrInvalid）同款。
                let vroot = Arc::new(VirtualRoot::build(pairs).map_err(|_| FsError::InvalidInput)?);
                let attr = Attr::dir(0o555, info.time_ns);
                let ino = {
                    self.inodes
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .insert(Node::SnapshotRoot { vroot }, attr, false, None)
                };
                Ok(Lookup {
                    ino,
                    attr,
                    ttl: IMMUTABLE_TTL,
                })
            }
            Node::SnapshotRoot { vroot } => {
                let v = VirtualRoot::lookup(vroot.top(), name).ok_or(FsError::NotFound)?;
                // 頂層合成目錄的虛擬路徑 = "/<name>"（層級表 key 的慣例）
                let mut child_key = Vec::with_capacity(name.len() + 1);
                child_key.push(b'/');
                child_key.extend_from_slice(name);
                self.vchild_inode(v, Arc::clone(vroot), child_key).await
            }
            Node::SyntheticDir { vroot, level_key } => {
                let level = vroot.level(level_key);
                let v = VirtualRoot::lookup(level, name).ok_or(FsError::NotFound)?;
                let mut child_key = level_key.clone();
                child_key.push(b'/');
                child_key.extend_from_slice(name);
                self.vchild_inode(v, Arc::clone(vroot), child_key).await
            }
            Node::Dir { entries } => {
                let e = lookup_entry(entries, name).ok_or(FsError::NotFound)?;
                self.real_inode(e).await
            }
            Node::File { .. } | Node::Symlink { .. } => Err(FsError::NotFound),
        }
    }

    /// 虛擬層級裡的條目 → inode（合成的中介目錄或真實的子樹）。
    async fn vchild_inode(
        &self,
        v: &VEntry,
        vroot: Arc<VirtualRoot>,
        level_key: Vec<u8>,
    ) -> Result<Lookup, FsError> {
        match v {
            VEntry::Synthetic { .. } => {
                let attr = Attr::dir(0o555, 0);
                let ino = {
                    self.inodes
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .insert(Node::SyntheticDir { vroot, level_key }, attr, false, None)
                };
                Ok(Lookup {
                    ino,
                    attr,
                    ttl: IMMUTABLE_TTL,
                })
            }
            VEntry::Real(e) => self.real_inode(e).await,
        }
    }

    /// 真實 tree entry → inode。目錄在這裡就載好整段 chain（immutable）。
    async fn real_inode(&self, e: &Entry) -> Result<Lookup, FsError> {
        let xattrs = e.xattrs.as_ref().map(|m| {
            Arc::new(
                m.iter()
                    .map(|(k, v)| (k.to_vec(), v.to_vec()))
                    .collect::<BTreeMap<_, _>>(),
            )
        });
        let (node, attr) = match e.kind {
            node_type::DIR => {
                let entries: Arc<Vec<Entry>> = if e.subtree.is_zero() {
                    Arc::new(Vec::new())
                } else {
                    Arc::new(
                        self.repo
                            .read_tree_chain(&e.subtree)
                            .await
                            .map_err(|_| FsError::Io)?,
                    )
                };
                (Node::Dir { entries }, entry_attr(e))
            }
            node_type::FILE => (
                Node::File {
                    chunks: e.chunks.clone().into(),
                    content: e.content,
                    resolved: tokio::sync::OnceCell::new(),
                },
                entry_attr(e),
            ),
            node_type::SYMLINK => (
                Node::Symlink {
                    target: e.target.clone().into(),
                },
                entry_attr(e),
            ),
            other => {
                tracing::warn!("unknown tree entry kind {other}; refusing to serve it");
                return Err(FsError::Io);
            }
        };
        let ino = {
            self.inodes
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(node, attr, false, xattrs)
        };
        Ok(Lookup {
            ino,
            attr,
            ttl: IMMUTABLE_TTL,
        })
    }

    // —— 屬性與連結 ——

    pub fn getattr(&self, ino: u64) -> Result<(Attr, Duration), FsError> {
        let ino_entry = self.node(ino).ok_or(FsError::NotFound)?;
        Ok((
            ino_entry.attr,
            if ino_entry.volatile {
                VOLATILE_TTL
            } else {
                IMMUTABLE_TTL
            },
        ))
    }

    pub fn readlink(&self, ino: u64) -> Result<Vec<u8>, FsError> {
        let ino_entry = self.node(ino).ok_or(FsError::NotFound)?;
        match &ino_entry.node {
            Node::Symlink { target } => Ok(target.to_vec()),
            _ => Err(FsError::InvalidInput),
        }
    }

    // —— 目錄列表 ——

    /// opendir：拍下目錄快照，回 fh。readdir 分頁都從這份讀。
    pub async fn opendir(&self, ino: u64) -> Result<u64, FsError> {
        let ino_entry = self.node(ino).ok_or(FsError::NotFound)?;
        let listing = match &*ino_entry {
            i if matches!(i.node, Node::Root) => {
                let clients = self.list_clients().await?;
                Listing {
                    entries: clients
                        .into_iter()
                        .map(|name| DirEntryData {
                            name: name.into_bytes(),
                            kind: Kind::Dir,
                        })
                        .collect(),
                }
            }
            i => match &i.node {
                Node::Client { client } => {
                    let keys = self.snapshot_keys_for(Some(client)).await?;
                    Listing {
                        entries: keys
                            .iter()
                            .filter_map(|k| k.rsplit('/').next())
                            .map(|ts| DirEntryData {
                                name: ts.as_bytes().to_vec(),
                                kind: Kind::Dir,
                            })
                            .collect(),
                    }
                }
                Node::SnapshotRoot { vroot } => listing_of_vlevel(vroot.top()),
                Node::SyntheticDir { vroot, level_key } => {
                    listing_of_vlevel(vroot.level(level_key))
                }
                Node::Dir { entries } => Listing {
                    entries: entries.iter().map(dir_entry_data).collect(),
                },
                _ => return Err(FsError::InvalidInput),
            },
        };
        let fh = {
            let mut t = self.listings.lock().unwrap_or_else(|e| e.into_inner());
            t.0 += 1;
            let fh = t.0;
            t.1.insert(fh, Arc::new(listing));
            fh
        };
        Ok(fh)
    }

    /// readdir 分頁：從 fh 的快照第 `offset` 筆起給最多 `max` 筆。
    /// offset 是 kernel 回報的續讀位置（FUSE 慣例 = 已給筆數）。
    pub fn readdir(&self, fh: u64, offset: u64, max: usize) -> Result<Vec<DirEntryData>, FsError> {
        let t = self.listings.lock().unwrap_or_else(|e| e.into_inner());
        let listing = t.1.get(&fh).ok_or(FsError::BadHandle)?;
        let skip = offset.min(usize::MAX as u64) as usize;
        Ok(listing
            .entries
            .iter()
            .skip(skip)
            .take(max)
            .cloned()
            .collect())
    }

    pub fn releasedir(&self, fh: u64) {
        self.listings
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .1
            .remove(&fh);
    }

    // —— 檔案讀取 ——

    pub async fn read_file(&self, ino: u64, offset: u64, size: u32) -> Result<Vec<u8>, FsError> {
        let ino_entry = self.node(ino).ok_or(FsError::NotFound)?;
        let Node::File {
            chunks,
            content,
            resolved,
        } = &ino_entry.node
        else {
            return Err(FsError::InvalidInput);
        };
        let r = resolved
            .get_or_try_init(|| self.resolve_file(chunks, *content))
            .await
            .map_err(|_| FsError::Io)?;
        let total = r.offsets.total();
        if offset >= total {
            return Ok(Vec::new());
        }
        let end = total.min(offset.saturating_add(u64::from(size)));
        let mut buf = Vec::with_capacity((end - offset) as usize);
        let mut pos = offset;
        while pos < end {
            let (i, in_chunk) = r.offsets.locate(pos).ok_or(FsError::Io)?;
            let data = self.chunk_of(&r.chunks[i]).await?;
            let avail = u64::try_from(data.len())
                .unwrap_or(u64::MAX)
                .saturating_sub(in_chunk);
            let take = avail.min(end - pos);
            if take == 0 {
                // index 的 raw_len 跟實際 chunk 長度對不上：防禦性避免死迴圈
                return Err(FsError::Io);
            }
            let from = in_chunk as usize;
            buf.extend_from_slice(&data[from..from + take as usize]);
            pos += take;
        }
        Ok(buf)
    }

    /// 直接內容回原清單；間接內容把清單 chunk 讀出來解成 `ChunkList`，
    /// 再用 index 的 raw_len 建邊界表。
    async fn resolve_file(
        &self,
        chunks: &[ChunkId],
        content: u8,
    ) -> Result<ResolvedFile, CoreError> {
        let ids: Vec<ChunkId> = if content == content_type::DIRECT {
            chunks.to_vec()
        } else {
            let mut bytes = Vec::new();
            for id in chunks {
                bytes.extend_from_slice(&self.repo.read_chunk_reloading(id, &self.index).await?);
            }
            let list: ChunkList = cbor::decode(&bytes).map_err(|e| CoreError::Corrupt {
                key: "<chunk list>".to_owned(),
                reason: e.to_string(),
            })?;
            list.chunks
        };
        let mut raw_lens = Vec::with_capacity(ids.len());
        for id in &ids {
            let len = self
                .index
                .raw_len_reloading(&self.repo, id)
                .await?
                .ok_or(CoreError::ChunkMissing(*id))?;
            raw_lens.push(len);
        }
        Ok(ResolvedFile {
            chunks: ids.into(),
            offsets: Arc::new(ChunkOffsets::from_raw_lens(raw_lens.iter())),
        })
    }

    async fn chunk_of(&self, id: &ChunkId) -> Result<Vec<u8>, FsError> {
        if let Some(data) = self.lru.lock().unwrap_or_else(|e| e.into_inner()).get(id) {
            return Ok(data.to_vec());
        }
        let data = self
            .repo
            .read_chunk_reloading(id, &self.index)
            .await
            .map_err(|_| FsError::Io)?;
        self.lru
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .put(*id, bytes::Bytes::from(data.clone()));
        Ok(data)
    }

    // —— xattr ——

    pub fn get_xattr(&self, ino: u64, name: &[u8]) -> Result<Option<Vec<u8>>, FsError> {
        let ino_entry = self.node(ino).ok_or(FsError::NotFound)?;
        Ok(ino_entry.xattrs.as_ref().and_then(|m| m.get(name).cloned()))
    }

    pub fn list_xattrs(&self, ino: u64) -> Result<Vec<Vec<u8>>, FsError> {
        let ino_entry = self.node(ino).ok_or(FsError::NotFound)?;
        Ok(ino_entry
            .xattrs
            .as_ref()
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default())
    }

    /// 測試用：目前存在的 inode 數。
    pub fn inode_count(&self) -> usize {
        self.inodes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .map
            .len()
    }
}

fn entry_attr(e: &Entry) -> Attr {
    Attr {
        kind: match e.kind {
            node_type::DIR => Kind::Dir,
            node_type::SYMLINK => Kind::Symlink,
            _ => Kind::File,
        },
        perm: e.mode.unwrap_or(0o555) & 0o7777,
        size: if e.kind == node_type::FILE {
            e.size
        } else {
            u64::try_from(e.target.len()).unwrap_or(0)
        },
        uid: e.uid.unwrap_or(0),
        gid: e.gid.unwrap_or(0),
        mtime_ns: e.mtime_ns.unwrap_or(0),
        ctime_ns: match (e.ctime_ns, e.mtime_ns) {
            (Some(c), _) if c != 0 => c,
            (_, m) => m.unwrap_or(0),
        },
    }
}

fn listing_of_vlevel(level: &[VEntry]) -> Listing {
    Listing {
        entries: level
            .iter()
            .map(|v| DirEntryData {
                name: v.name().to_vec(),
                kind: match v.entry() {
                    None => Kind::Dir, // 合成的中介目錄
                    Some(e) => kind_of(e.kind),
                },
            })
            .collect(),
    }
}

fn dir_entry_data(e: &Entry) -> DirEntryData {
    DirEntryData {
        name: e.name.clone(),
        kind: kind_of(e.kind),
    }
}

fn kind_of(kind: u8) -> Kind {
    match kind {
        node_type::DIR => Kind::Dir,
        node_type::SYMLINK => Kind::Symlink,
        _ => Kind::File,
    }
}

/// 目錄的 entries 依名稱排序（格式規範），二分搜尋。
fn lookup_entry<'a>(entries: &'a [Entry], name: &[u8]) -> Option<&'a Entry> {
    let i = entries.partition_point(|e| e.name.as_slice() < name);
    entries.get(i).filter(|e| e.name.as_slice() == name)
}

/// timestamp 的形狀：`20260906T185051413374167Z`（8 + T + 15 + Z = 25 bytes）。
/// 只當早退檢查——真正的存在性由 repo 讀取決定。
fn looks_like_timestamp(name: &[u8]) -> bool {
    name.len() == 25
        && name[8] == b'T'
        && name[24] == b'Z'
        && name
            .iter()
            .enumerate()
            .all(|(i, b)| i == 8 || i == 24 || b.is_ascii_digit())
}
