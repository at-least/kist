//! mount 的核心（docs/format-v4.md）：inode 表、snapshot 瀏覽、chunk 隨機讀。**不知道 FUSE 的存在**——[`FsCore`]
//! 吃 [`Repo`]、吐 [`Attr`]／[`Listing`]／bytes，`crate::fuse` 只做 fuser 回覆的轉接；測試直接開 `FsCore`，不用真的
//! 掛載（`tests/v4_fs_read.rs`）。介面的型別原本在 v3 的 `corefs`，v3 移除時搬來這裡。
//!
//! 佈局同 v3：`<client>/<時間戳>/<roots>`，頂兩層 volatile（TTL 1 秒），snapshot 內容 immutable（TTL 24 小時）。
//! 與 v3 不同的地方：
//! - snapshot 的頂層照 `Root.t` 展開（§9）：目錄 root 的最後一段是帶 tree 的目錄，`/` 這種沒有段的目錄 root 攤平
//!   到頂層，檔案與符號連結 root 放在去掉最後一段的層級。不再讀內容猜形態，所以也沒有 v3 的「退回形態」：roots
//!   讀不到就是那一次 lookup 的 I/O 錯誤，不記進去重表。
//! - 目錄的 lookup 沿 Dir 的索引只讀涵蓋名稱的那一段（§7）；opendir 才讀整個目錄。
//! - 檔案第一次讀時解析整個 chunk 清單（Reader::file_chunks，§7 的大小檢查），每個 chunk 的長度取自持有者記錄的
//!   `r`（同 v3 用 index 的 raw_len）。ChunkIndex 的 `size` 可以讓隨機讀只讀需要的段，還沒做。
//! - 讀取經 [`Live`]：持有者全部失敗時重新載入一次世代（60 秒限流）；看到沒見過的 snapshot 也重新載入（1 秒
//!   限流）：新的 snapshot 可能引用 mount 載入時還不知道的 pack。

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use kist_core::v4::{Live, Repo};
use kist_core::CoreError;
use kist_format::v4::ids::{ChunkId, ClientId, MetaId};
use kist_format::v4::keys::{Key, SnapTs};
use kist_format::v4::meta::{meta_kind, node_type, Entry};
use kist_format::v4::snapshot::{split_locator, Root, Snapshot};

use crate::offsets::ChunkOffsets;

/// 頂兩層（client、timestamp 列表）的快取期限。
pub const VOLATILE_TTL: Duration = Duration::from_secs(1);
/// snapshot 內容（內容定址、不可變）的快取期限。
pub const IMMUTABLE_TTL: Duration = Duration::from_secs(24 * 3600);

/// mount 設定。`chunk_cache` = 解密後 chunk 的快取顆數（0 視為 1；預設 8；
/// 8 MiB chunk 時上限 64 MiB）。
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

/// 看到新的 snapshot 時，兩次重新載入之間至少隔這麼久（同 v3）。
const RELOAD_EAGER_MIN_INTERVAL: Duration = Duration::from_secs(1);

fn ttl_of(volatile: bool) -> Duration {
    if volatile {
        VOLATILE_TTL
    } else {
        IMMUTABLE_TTL
    }
}

fn dir_attr(mtime_ns: i64) -> Attr {
    Attr {
        kind: Kind::Dir,
        perm: 0o555,
        size: 0,
        uid: 0,
        gid: 0,
        mtime_ns,
        ctime_ns: mtime_ns,
    }
}

// —— snapshot 頂層的虛擬層級 ——

/// 一個虛擬層級裡的條目：真實的（root 的 entry、目錄 root 的最後一段）或合成的中介目錄。
#[derive(Debug, Clone)]
pub enum VEntry {
    Real(Arc<Entry>),
    Synthetic { name: Vec<u8> },
}

impl VEntry {
    pub fn name(&self) -> &[u8] {
        match self {
            VEntry::Real(e) => &e.n,
            VEntry::Synthetic { name } => name,
        }
    }

    pub fn entry(&self) -> Option<&Entry> {
        match self {
            VEntry::Real(e) => Some(e),
            VEntry::Synthetic { .. } => None,
        }
    }
}

/// roots 展開後的層級表。頂層的 key 是空的，中介層的 key 是 `/段/段`（只當 key 用）。
#[derive(Debug, Default)]
pub struct VirtualRoot {
    levels: HashMap<Vec<u8>, Vec<VEntry>>,
}

impl VirtualRoot {
    /// roots → 層級（§9 的落點）。`flat` 只給沒有段的目錄 root：它的內容，攤平到頂層；其他 root 給空的。
    pub fn build(roots: Vec<(Root, Vec<Entry>)>) -> Result<Self, String> {
        let mut levels: HashMap<Vec<u8>, Vec<VEntry>> = HashMap::new();
        for (root, flat) in roots {
            let segs = split_locator(&root.path).map_err(|e| e.to_string())?;
            let (leaf, parents) = if root.t == node_type::DIR {
                let tree = root
                    .tree
                    .ok_or_else(|| "a directory root without a tree".to_owned())?;
                let Some((last, parents)) = segs.split_last() else {
                    for e in flat {
                        push_real(&mut levels, b"", e);
                    }
                    continue;
                };
                (root_dir_entry(last, tree), parents)
            } else {
                let e = root
                    .entry
                    .clone()
                    .ok_or_else(|| "a file or symlink root without its entry".to_owned())?;
                (e, &segs[..segs.len().saturating_sub(1)])
            };
            let mut key = Vec::new();
            for seg in parents {
                let level = levels.entry(key.clone()).or_default();
                if !level.iter().any(|v| v.name() == seg.as_slice()) {
                    level.push(VEntry::Synthetic { name: seg.clone() });
                }
                key.push(b'/');
                key.extend_from_slice(seg);
            }
            push_real(&mut levels, &key, leaf);
        }
        for level in levels.values_mut() {
            level.sort_by(|a, b| a.name().cmp(b.name()));
        }
        Ok(Self { levels })
    }

    pub fn level(&self, key: &[u8]) -> &[VEntry] {
        self.levels.get(key).map(Vec::as_slice).unwrap_or(&[])
    }

    /// 層級已排序，二分搜尋。
    pub fn lookup<'a>(level: &'a [VEntry], name: &[u8]) -> Option<&'a VEntry> {
        let i = level.partition_point(|v| v.name() < name);
        level.get(i).filter(|v| v.name() == name)
    }
}

/// 目錄 root 的最後一段：帶 tree 的目錄（屬性唯讀、擁有者 0、時間由 snapshot 的時間覆蓋，同 v3）。
fn root_dir_entry(name: &[u8], tree: MetaId) -> Entry {
    let mut e = Entry::new(name.to_vec(), node_type::DIR, meta_kind::POSIX);
    e.tree = Some(tree);
    e.mode = Some(0o040_555);
    e.uid = Some(0);
    e.gid = Some(0);
    e.mtime = Some(0);
    e
}

/// 同名的條目被取代（後到的 root 贏，同 v3）：兩個 root 切出同一組段時不列出兩個同名的條目。
fn push_real(levels: &mut HashMap<Vec<u8>, Vec<VEntry>>, key: &[u8], e: Entry) {
    let level = levels.entry(key.to_vec()).or_default();
    let name = e.n.clone();
    let v = VEntry::Real(Arc::new(e));
    match level.iter().position(|x| x.name() == name.as_slice()) {
        Some(i) => level[i] = v,
        None => level.push(v),
    }
}

// —— inode 表 ——

struct ResolvedFile {
    chunks: Arc<[ChunkId]>,
    offsets: Arc<ChunkOffsets>,
}

enum Node {
    Root,
    Client {
        client: ClientId,
    },
    /// snapshot 根目錄（`level_key` 空）或定位的中介目錄。
    SyntheticDir {
        vroot: Arc<VirtualRoot>,
        level_key: Vec<u8>,
    },
    Dir {
        tree: MetaId,
    },
    File {
        entry: Arc<Entry>,
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

/// 同 v3（ADR 019 A6）：同一個（父 ino, 名稱）配過就回原來的號，號碼永不重用。
#[derive(Default)]
struct InodeTable {
    next: u64,
    map: HashMap<u64, Arc<Inode>>,
    names: HashMap<(u64, Vec<u8>), u64>,
}

impl InodeTable {
    fn with_root() -> Self {
        let mut t = Self::default();
        t.insert(Node::Root, dir_attr(0), true, None);
        t
    }

    fn insert_named(
        &mut self,
        parent: u64,
        name: &[u8],
        node: Node,
        attr: Attr,
        volatile: bool,
        xattrs: Option<Arc<BTreeMap<Vec<u8>, Vec<u8>>>>,
    ) -> u64 {
        let key = (parent, name.to_vec());
        if let Some(&ino) = self.names.get(&key) {
            return ino;
        }
        let ino = self.insert(node, attr, volatile, xattrs);
        self.names.insert(key, ino);
        ino
    }

    fn known(&self, parent: u64, name: &[u8]) -> Option<u64> {
        self.names.get(&(parent, name.to_vec())).copied()
    }

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

/// 解密後 chunk 的小 LRU（同 v3）。
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
            let Some(victim) = self.order.pop_back() else {
                break;
            };
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

/// v4 的 mount 核心狀態。鎖都是短區間，不跨 `.await`。
pub struct FsCore {
    live: Live,
    lru: Mutex<Lru>,
    inodes: Mutex<InodeTable>,
    listings: Mutex<(u64, HashMap<u64, Arc<Listing>>)>,
    seen: Mutex<Seen>,
}

/// 看過的 snapshot 與上一次因此重新載入的時間（與持有者失敗時的限流分開計，同 v3）。
struct Seen {
    keys: HashSet<String>,
    last_refresh: Option<Instant>,
}

fn io(e: CoreError) -> FsError {
    tracing::warn!("{e}");
    FsError::Io
}

impl FsCore {
    /// 先列出 snapshot、再載入世代：列出的 snapshot 用到的 pack 都在那一代的 map 或當下的 delta 裡（delta 在
    /// snapshot 之前寫，B3、B5），所以它們不必觸發重新載入。
    pub async fn new(repo: Repo, config: MountConfig) -> Result<Self, CoreError> {
        let keys = repo
            .list_snapshot_keys()
            .await?
            .iter()
            .map(Key::path)
            .collect();
        Ok(Self {
            live: repo.live().await?,
            lru: Mutex::new(Lru::new(config.chunk_cache)),
            inodes: Mutex::new(InodeTable::with_root()),
            listings: Mutex::new((0, HashMap::new())),
            seen: Mutex::new(Seen {
                keys,
                last_refresh: None,
            }),
        })
    }

    fn table(&self) -> std::sync::MutexGuard<'_, InodeTable> {
        self.inodes.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn node(&self, ino: u64) -> Option<Arc<Inode>> {
        self.table().map.get(&ino).cloned()
    }

    // —— 瀏覽 ——

    /// 全部 snapshot 主體的 key；有沒見過的就重新載入世代（1 秒限流）。key 一律記為看過：重新載入失敗也不能讓
    /// 每次瀏覽都重試。
    async fn snapshot_keys(&self) -> Result<Vec<Key>, FsError> {
        let keys = self.live.repo().list_snapshot_keys().await.map_err(io)?;
        let due = {
            let mut seen = self.seen.lock().unwrap_or_else(|e| e.into_inner());
            let fresh = keys.iter().filter(|k| seen.keys.insert(k.path())).count();
            let due = fresh > 0
                && seen
                    .last_refresh
                    .is_none_or(|t| t.elapsed() >= RELOAD_EAGER_MIN_INTERVAL);
            if due {
                seen.last_refresh = Some(Instant::now());
            }
            due
        };
        if due {
            self.live.refresh("new snapshots appeared").await;
        }
        Ok(keys)
    }

    async fn list_clients(&self) -> Result<Vec<ClientId>, FsError> {
        let mut clients: Vec<ClientId> = self
            .snapshot_keys()
            .await?
            .into_iter()
            .filter_map(|k| match k {
                Key::Snapshot { client, .. } => Some(client),
                _ => None,
            })
            .collect();
        clients.sort();
        clients.dedup();
        Ok(clients)
    }

    /// snapshot 的頂層：讀 roots，沒有段的目錄 root 再讀它的內容（攤平）。
    async fn vroot(&self, snap: &Snapshot) -> Result<VirtualRoot, FsError> {
        let roots = self
            .live
            .run(async |r| r.roots(&snap.roots).await)
            .await
            .map_err(io)?;
        let mut pairs = Vec::with_capacity(roots.len());
        for root in roots {
            let segs = split_locator(&root.path).map_err(|_| FsError::InvalidInput)?;
            let flat = match root.tree {
                Some(tree) if root.t == node_type::DIR && segs.is_empty() => self
                    .live
                    .run(async |r| r.entries(&tree).await)
                    .await
                    .map_err(io)?,
                _ => Vec::new(),
            };
            pairs.push((root, flat));
        }
        VirtualRoot::build(pairs).map_err(|e| {
            tracing::warn!("{e}");
            FsError::InvalidInput
        })
    }

    // —— lookup ——

    pub async fn lookup(&self, parent_ino: u64, name: &[u8]) -> Result<Lookup, FsError> {
        let parent = self.node(parent_ino).ok_or(FsError::NotFound)?;
        match &parent.node {
            Node::Root => {
                let client = std::str::from_utf8(name)
                    .ok()
                    .and_then(|s| ClientId::from_hex(s).ok())
                    .ok_or(FsError::NotFound)?;
                if !self.list_clients().await?.contains(&client) {
                    return Err(FsError::NotFound);
                }
                let attr = dir_attr(0);
                let ino = self.table().insert_named(
                    parent_ino,
                    name,
                    Node::Client { client },
                    attr,
                    true,
                    None,
                );
                Ok(Lookup {
                    ino,
                    attr,
                    ttl: VOLATILE_TTL,
                })
            }
            Node::Client { client } => {
                let ts = std::str::from_utf8(name)
                    .ok()
                    .and_then(|s| SnapTs::parse(s).ok())
                    .ok_or(FsError::NotFound)?;
                let key = Key::Snapshot {
                    client: *client,
                    ts,
                    replica: false,
                };
                // 每次都讀：snapshot 被 forget 之後同一個名字要回 NotFound（去重只管身分，不管存在）
                let snap = match self.live.repo().read_snapshot(&key).await {
                    Ok(s) => s,
                    Err(CoreError::SnapshotNotFound(_)) => return Err(FsError::NotFound),
                    Err(e) => return Err(io(e)),
                };
                let vroot = Arc::new(self.vroot(&snap).await?);
                let attr = dir_attr(snap.time);
                let node = Node::SyntheticDir {
                    vroot,
                    level_key: Vec::new(),
                };
                let ino = self
                    .table()
                    .insert_named(parent_ino, name, node, attr, false, None);
                Ok(Lookup {
                    ino,
                    attr,
                    ttl: IMMUTABLE_TTL,
                })
            }
            Node::SyntheticDir { vroot, level_key } => {
                let v =
                    VirtualRoot::lookup(vroot.level(level_key), name).ok_or(FsError::NotFound)?;
                match v {
                    VEntry::Synthetic { .. } => {
                        let mut child_key = level_key.clone();
                        child_key.push(b'/');
                        child_key.extend_from_slice(name);
                        let attr = dir_attr(0);
                        let node = Node::SyntheticDir {
                            vroot: Arc::clone(vroot),
                            level_key: child_key,
                        };
                        let ino = self
                            .table()
                            .insert_named(parent_ino, name, node, attr, false, None);
                        Ok(Lookup {
                            ino,
                            attr,
                            ttl: IMMUTABLE_TTL,
                        })
                    }
                    VEntry::Real(e) => Ok(self.real_inode(parent_ino, name, e)),
                }
            }
            Node::Dir { tree } => {
                // 同名已配過號：內容定址、形態不變，不必再讀那一段
                let known = self.table().known(parent_ino, name);
                if let Some((ino, inode)) = known.and_then(|ino| Some((ino, self.node(ino)?))) {
                    return Ok(Lookup {
                        ino,
                        attr: inode.attr,
                        ttl: IMMUTABLE_TTL,
                    });
                }
                let tree = *tree;
                let e = self
                    .live
                    .run(async |r| r.lookup(&tree, name).await)
                    .await
                    .map_err(io)?
                    .ok_or(FsError::NotFound)?;
                Ok(self.real_inode(parent_ino, name, &e))
            }
            Node::File { .. } | Node::Symlink { .. } => Err(FsError::NotFound),
        }
    }

    /// 真實的 entry → inode（目錄只記 tree，內容等 lookup 或 opendir 才讀）。
    fn real_inode(&self, parent_ino: u64, name: &[u8], e: &Entry) -> Lookup {
        let attr = entry_attr(e);
        let xattrs = e.xattrs.as_ref().map(|m| {
            Arc::new(
                m.iter()
                    .map(|(k, v)| (k.to_vec(), v.to_vec()))
                    .collect::<BTreeMap<_, _>>(),
            )
        });
        let node = match (e.t, e.tree) {
            (node_type::DIR, Some(tree)) => Node::Dir { tree },
            (node_type::SYMLINK, _) => Node::Symlink {
                target: e.target.clone().into(),
            },
            // 目錄一定有 tree、種類只有三種（解碼時已檢查，§8）
            _ => Node::File {
                entry: Arc::new(e.clone()),
                resolved: tokio::sync::OnceCell::new(),
            },
        };
        let ino = self
            .table()
            .insert_named(parent_ino, name, node, attr, false, xattrs);
        Lookup {
            ino,
            attr,
            ttl: IMMUTABLE_TTL,
        }
    }

    // —— 屬性與連結 ——

    pub fn getattr(&self, ino: u64) -> Result<(Attr, Duration), FsError> {
        let inode = self.node(ino).ok_or(FsError::NotFound)?;
        Ok((inode.attr, ttl_of(inode.volatile)))
    }

    pub fn readlink(&self, ino: u64) -> Result<Vec<u8>, FsError> {
        let inode = self.node(ino).ok_or(FsError::NotFound)?;
        match &inode.node {
            Node::Symlink { target } => Ok(target.to_vec()),
            _ => Err(FsError::InvalidInput),
        }
    }

    // —— 目錄列表 ——

    /// opendir：拍下目錄的列表，之後的 readdir 分頁都從這份讀。
    pub async fn opendir(&self, ino: u64) -> Result<u64, FsError> {
        let inode = self.node(ino).ok_or(FsError::NotFound)?;
        let entries = match &inode.node {
            Node::Root => self
                .list_clients()
                .await?
                .into_iter()
                .map(|c| DirEntryData {
                    name: c.to_string().into_bytes(),
                    kind: Kind::Dir,
                })
                .collect(),
            Node::Client { client } => self
                .snapshot_keys()
                .await?
                .into_iter()
                .filter_map(|k| match k {
                    Key::Snapshot { client: c, ts, .. } if c == *client => Some(DirEntryData {
                        name: ts.as_str().as_bytes().to_vec(),
                        kind: Kind::Dir,
                    }),
                    _ => None,
                })
                .collect(),
            Node::SyntheticDir { vroot, level_key } => vroot
                .level(level_key)
                .iter()
                .map(|v| DirEntryData {
                    name: v.name().to_vec(),
                    kind: v.entry().map_or(Kind::Dir, |e| kind_of(e.t)),
                })
                .collect(),
            Node::Dir { tree } => {
                let tree = *tree;
                self.live
                    .run(async |r| r.entries(&tree).await)
                    .await
                    .map_err(io)?
                    .iter()
                    .map(|e| DirEntryData {
                        name: e.n.clone(),
                        kind: kind_of(e.t),
                    })
                    .collect()
            }
            Node::File { .. } | Node::Symlink { .. } => return Err(FsError::InvalidInput),
        };
        let mut t = self.listings.lock().unwrap_or_else(|e| e.into_inner());
        t.0 += 1;
        let fh = t.0;
        t.1.insert(fh, Arc::new(Listing { entries }));
        Ok(fh)
    }

    /// readdir 分頁：從 fh 的列表第 `offset` 筆起給最多 `max` 筆。
    pub fn readdir(&self, fh: u64, offset: u64, max: usize) -> Result<Vec<DirEntryData>, FsError> {
        let t = self.listings.lock().unwrap_or_else(|e| e.into_inner());
        let listing = t.1.get(&fh).ok_or(FsError::BadHandle)?;
        let skip = usize::try_from(offset).unwrap_or(usize::MAX);
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
        let inode = self.node(ino).ok_or(FsError::NotFound)?;
        let Node::File { entry, resolved } = &inode.node else {
            return Err(FsError::InvalidInput);
        };
        let r = resolved
            .get_or_try_init(|| self.resolve(entry))
            .await
            .map_err(io)?;
        let total = r.offsets.total();
        if offset >= total {
            return Ok(Vec::new());
        }
        let end = total.min(offset.saturating_add(u64::from(size)));
        let mut buf = Vec::with_capacity(usize::try_from(end - offset).unwrap_or(0));
        let mut pos = offset;
        while pos < end {
            let (i, in_chunk) = r.offsets.locate(pos).ok_or(FsError::Io)?;
            let data = self.chunk(&r.chunks[i]).await?;
            let avail = u64::try_from(data.len())
                .unwrap_or(u64::MAX)
                .saturating_sub(in_chunk);
            let take = avail.min(end - pos);
            if take == 0 {
                // 持有者記錄的長度與讀到的不符：不讓迴圈空轉
                return Err(FsError::Io);
            }
            let from = usize::try_from(in_chunk).map_err(|_| FsError::Io)?;
            let to = from + usize::try_from(take).map_err(|_| FsError::Io)?;
            buf.extend_from_slice(&data[from..to]);
            pos += take;
        }
        Ok(buf)
    }

    /// chunk 清單（§7 的大小檢查）與每個 chunk 的長度（持有者記錄的 `r`）→ 邊界表。
    async fn resolve(&self, entry: &Entry) -> Result<ResolvedFile, CoreError> {
        let (chunks, lens) = self
            .live
            .run(async |r| {
                let chunks = r.file_chunks(entry).await?;
                let lens = chunks
                    .iter()
                    .map(|c| r.chunk_len(c))
                    .collect::<Result<Vec<u64>, CoreError>>()?;
                Ok((chunks, lens))
            })
            .await?;
        Ok(ResolvedFile {
            chunks: chunks.into(),
            offsets: Arc::new(ChunkOffsets::from_raw_lens(lens.iter())),
        })
    }

    async fn chunk(&self, id: &ChunkId) -> Result<bytes::Bytes, FsError> {
        if let Some(data) = self.lru.lock().unwrap_or_else(|e| e.into_inner()).get(id) {
            return Ok(data);
        }
        let data = bytes::Bytes::from(
            self.live
                .run(async |r| r.chunk(id).await)
                .await
                .map_err(io)?,
        );
        self.lru
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .put(*id, data.clone());
        Ok(data)
    }

    // —— xattr ——

    pub fn get_xattr(&self, ino: u64, name: &[u8]) -> Result<Option<Vec<u8>>, FsError> {
        let inode = self.node(ino).ok_or(FsError::NotFound)?;
        Ok(inode.xattrs.as_ref().and_then(|m| m.get(name).cloned()))
    }

    pub fn list_xattrs(&self, ino: u64) -> Result<Vec<Vec<u8>>, FsError> {
        let inode = self.node(ino).ok_or(FsError::NotFound)?;
        Ok(inode
            .xattrs
            .as_ref()
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default())
    }

    /// 測試用：目前存在的 inode 數。
    pub fn inode_count(&self) -> usize {
        self.table().map.len()
    }
}

fn entry_attr(e: &Entry) -> Attr {
    Attr {
        kind: kind_of(e.t),
        perm: e.mode.unwrap_or(0o555) & 0o7777,
        size: if e.t == node_type::SYMLINK {
            e.target.len() as u64
        } else {
            e.size
        },
        uid: e.uid.unwrap_or(0),
        gid: e.gid.unwrap_or(0),
        mtime_ns: e.mtime.unwrap_or(0),
        ctime_ns: match (e.ctime, e.mtime) {
            (Some(c), _) if c != 0 => c,
            (_, m) => m.unwrap_or(0),
        },
    }
}

fn kind_of(t: u8) -> Kind {
    match t {
        node_type::DIR => Kind::Dir,
        node_type::SYMLINK => Kind::Symlink,
        _ => Kind::File,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn dir_root(path: &str) -> (Root, Vec<Entry>) {
        (
            Root {
                path: path.as_bytes().to_vec(),
                t: node_type::DIR,
                tree: Some(MetaId::from_bytes([0xA5; 32])),
                entry: None,
            },
            Vec::new(),
        )
    }

    fn names(level: &[VEntry]) -> Vec<Vec<u8>> {
        level.iter().map(|v| v.name().to_vec()).collect()
    }

    #[test]
    fn absolute_paths_expand_into_synthetic_levels_and_share_them() {
        let root = VirtualRoot::build(vec![
            dir_root("/home/a/data"),
            dir_root("/home/b/data"),
            dir_root("s3://bucket/prefix"),
        ])
        .unwrap();
        assert_eq!(
            names(root.level(b"")),
            [b"bucket".to_vec(), b"home".to_vec()]
        );
        assert!(root.level(b"")[1].entry().is_none(), "home is synthetic");
        assert_eq!(names(root.level(b"/home")), [b"a".to_vec(), b"b".to_vec()]);
        let leaf = VirtualRoot::lookup(root.level(b"/home/a"), b"data")
            .unwrap()
            .entry()
            .unwrap();
        assert_eq!(leaf.tree, Some(MetaId::from_bytes([0xA5; 32])));
        assert_eq!(names(root.level(b"/bucket")), [b"prefix".to_vec()]);
    }

    #[test]
    fn a_file_root_is_its_entry_under_the_parent_segments() {
        let mut e = Entry::new(b"f.txt".to_vec(), node_type::FILE, meta_kind::GENERIC);
        e.size = 3;
        let root = VirtualRoot::build(vec![(
            Root {
                path: b"/x/f.txt".to_vec(),
                t: node_type::FILE,
                tree: None,
                entry: Some(e),
            },
            Vec::new(),
        )])
        .unwrap();
        assert_eq!(names(root.level(b"")), [b"x".to_vec()]);
        let f = VirtualRoot::lookup(root.level(b"/x"), b"f.txt")
            .unwrap()
            .entry()
            .unwrap();
        assert_eq!((f.t, f.size), (node_type::FILE, 3));
    }

    #[test]
    fn a_root_without_segments_flattens_and_dotdot_is_mapped() {
        let flat = vec![
            Entry::new(b"bin".to_vec(), node_type::FILE, meta_kind::GENERIC),
            Entry::new(b"etc".to_vec(), node_type::FILE, meta_kind::GENERIC),
        ];
        let mut slash = dir_root("/");
        slash.1 = flat;
        let root = VirtualRoot::build(vec![slash, dir_root("/a/../b")]).unwrap();
        assert_eq!(
            names(root.level(b"")),
            [b"a".to_vec(), b"bin".to_vec(), b"etc".to_vec()]
        );
        assert_eq!(names(root.level(b"/a")), [b"__parent__".to_vec()]);
        assert_eq!(names(root.level(b"/a/__parent__")), [b"b".to_vec()]);
    }

    #[test]
    fn a_locator_with_nul_is_rejected() {
        assert!(VirtualRoot::build(vec![dir_root("/a\0b/c")]).is_err());
    }
}
