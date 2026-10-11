//! v4 測試共用：小參數的 repo、讀回 snapshot、比對來源目錄、扮演維護角色寫 head 與分片。

#![allow(dead_code)]

pub mod harness;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use kist_backend::Backend;
use kist_core::v4::read::Reader;
use kist_core::v4::{BackupOptions, InitOptions, Repo};
use kist_crypto::KdfCost;
use kist_format::v4::config::Chunker;
use kist_format::v4::ids::{ClientId, ObjectId};
use kist_format::v4::index::{Head, Record, Settings, Shard, DEFAULT_GRACE, SHARDS};
use kist_format::v4::keys::{Gen, Key};
use kist_format::v4::meta::{node_type, Entry};
use kist_format::v4::snapshot::split_locator;

pub const PASSWORD: &[u8] = b"test password";

/// 小 chunk、最小的 pack 與 mpack：小量資料就有多個 chunk、多個 pack。
pub fn settings(replicas: u8) -> Settings {
    Settings {
        grace: DEFAULT_GRACE,
        replicas,
        pack_target: 64 * 1024,
        mpack_target: 64 * 1024,
    }
}

pub fn init_options(replicas: u8) -> InitOptions {
    InitOptions {
        chunker: Chunker {
            min: 4 * 1024,
            avg: 16 * 1024,
            max: 64 * 1024,
        },
        kdf: KdfCost {
            m_cost_kib: 8,
            t_cost: 1,
            p_cost: 1,
        },
        settings: Some(settings(replicas)),
    }
}

pub fn client(b: u8) -> ClientId {
    ClientId::from_bytes([b; 16])
}

pub fn opts(c: u8) -> BackupOptions {
    let mut o = BackupOptions::new(client(c), "testhost", "tester");
    o.lone_head_wait = Duration::ZERO;
    o
}

pub struct TestRepo {
    pub dir: tempfile::TempDir,
    pub backend: Backend,
}

impl TestRepo {
    pub async fn new() -> Self {
        Self::with(init_options(0)).await
    }

    pub async fn with(opts: InitOptions) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let backend = Backend::local(&dir.path().join("repo")).unwrap();
        Repo::init(backend.clone(), PASSWORD, opts).await.unwrap();
        Self { dir, backend }
    }

    /// 以 `cache` 底下的本地快取打開（每台 client 一個快取目錄）。
    pub async fn open_as(&self, cache: &str) -> Repo {
        let root = self.dir.path().join("cache").join(cache);
        Repo::open(self.backend.clone(), PASSWORD, Some(&root))
            .await
            .unwrap()
    }

    pub async fn open(&self) -> Repo {
        self.open_as("default").await
    }

    pub async fn open_uncached(&self) -> Repo {
        Repo::open(self.backend.clone(), PASSWORD, None)
            .await
            .unwrap()
    }

    pub fn repo_path(&self) -> PathBuf {
        self.dir.path().join("repo")
    }

    /// 某個前綴底下的物件數（本機 repo 的檔案數）。
    pub fn count(&self, prefix: &str) -> usize {
        let p = self.repo_path().join(prefix);
        if !p.exists() {
            return 0;
        }
        walk_files(&p).into_iter().filter(|p| p.is_file()).count()
    }

    /// 某個前綴底下的 key，排好。
    pub async fn keys(&self, prefix: &str) -> Vec<String> {
        let mut k: Vec<String> = self
            .backend
            .list(prefix)
            .await
            .unwrap()
            .into_iter()
            .map(|o| o.key)
            .collect();
        k.sort();
        k
    }
}

/// 讀回來或來源目錄裡的一個節點。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Node {
    Dir,
    File(Vec<u8>),
    Symlink(Vec<u8>),
}

/// 來源目錄 → 相對路徑（`/` 分隔）→ 節點。
pub fn source_tree(root: &Path) -> BTreeMap<String, Node> {
    fn walk(dir: &Path, prefix: &str, out: &mut BTreeMap<String, Node>) {
        for e in std::fs::read_dir(dir).unwrap() {
            let e = e.unwrap();
            let name = e.file_name().into_string().unwrap();
            let path = if prefix.is_empty() {
                name
            } else {
                format!("{prefix}/{name}")
            };
            let meta = std::fs::symlink_metadata(e.path()).unwrap();
            if meta.file_type().is_symlink() {
                let t = std::fs::read_link(e.path()).unwrap();
                out.insert(path, Node::Symlink(t.into_os_string().into_encoded_bytes()));
            } else if meta.is_dir() {
                out.insert(path.clone(), Node::Dir);
                walk(&e.path(), &path, out);
            } else {
                out.insert(path, Node::File(std::fs::read(e.path()).unwrap()));
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, "", &mut out);
    out
}

/// 讀回一個 snapshot：每個 root 放在它的定位切段之下（§9 的落點），內容依序讀 chunk。
pub async fn read_tree(repo: &Repo, snapshot: &str) -> BTreeMap<String, Node> {
    let reader = repo.reader(None).await.unwrap();
    let key = Key::parse(snapshot).unwrap();
    let snap = repo.read_snapshot(&key).await.unwrap();
    let mut out = BTreeMap::new();
    for root in reader.roots(&snap.roots).await.unwrap() {
        let segs: Vec<String> = split_locator(&root.path)
            .unwrap()
            .into_iter()
            .map(|s| String::from_utf8(s).unwrap())
            .collect();
        if root.t == node_type::DIR {
            walk_tree(&reader, root.tree.unwrap(), &segs.join("/"), &mut out).await;
        } else {
            let mut dir = segs;
            dir.pop();
            let e = root.entry.unwrap();
            let path = join(&dir.join("/"), &e.n);
            out.insert(path, node(&reader, &e).await);
        }
    }
    out
}

/// 一個 root 底下（不含 root 自己）：相對於 root 的路徑 → 節點。
pub async fn read_root_dir(repo: &Repo, snapshot: &str) -> BTreeMap<String, Node> {
    let reader = repo.reader(None).await.unwrap();
    let snap = repo
        .read_snapshot(&Key::parse(snapshot).unwrap())
        .await
        .unwrap();
    let roots = reader.roots(&snap.roots).await.unwrap();
    assert_eq!(roots.len(), 1);
    let mut out = BTreeMap::new();
    walk_tree(&reader, roots[0].tree.unwrap(), "", &mut out).await;
    out
}

fn join(prefix: &str, name: &[u8]) -> String {
    let name = String::from_utf8(name.to_vec()).unwrap();
    if prefix.is_empty() {
        name
    } else {
        format!("{prefix}/{name}")
    }
}

async fn node(reader: &Reader, e: &Entry) -> Node {
    match e.t {
        node_type::DIR => Node::Dir,
        node_type::SYMLINK => Node::Symlink(e.target.clone()),
        _ => {
            let mut content = Vec::new();
            for id in reader.file_chunks(e).await.unwrap() {
                content.extend(reader.chunk(&id).await.unwrap());
            }
            Node::File(content)
        }
    }
}

fn walk_tree<'a>(
    reader: &'a Reader,
    tree: kist_format::v4::ids::MetaId,
    prefix: &'a str,
    out: &'a mut BTreeMap<String, Node>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + 'a>> {
    Box::pin(async move {
        let entries = reader.dir(&tree).await.unwrap().rest(reader).await.unwrap();
        for e in entries {
            let path = join(prefix, &e.n);
            out.insert(path.clone(), node(reader, &e).await);
            if e.t == node_type::DIR {
                walk_tree(reader, e.tree.unwrap(), &path, out).await;
            }
        }
    })
}

/// 依名稱排好、保證切成至少 `segs` 段的檔名（§7：這一段已有 1024 項之後，名稱的 cut hash 成立就在它之後切）。
/// K_cut 由每個 repo 亂數的 master 推導，固定的一組名稱可能剛好不切，所以依這個 repo 的金鑰挑名稱。
pub fn names_with_cuts(repo: &Repo, segs: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut in_seg = 0usize;
    let mut cuts = 0usize;
    let mut i = 0u32;
    while cuts + 1 < segs || in_seg == 0 {
        let name = format!("f{i:06}");
        i += 1;
        let cut =
            in_seg >= kist_format::v4::meta::MIN_SEG_ITEMS && repo.keys().is_cut(name.as_bytes());
        out.push(name);
        in_seg += 1;
        if cut {
            cuts += 1;
            in_seg = 0;
        }
    }
    out
}

/// 確定性的測試資料（"kist/test/<seed>" 的 BLAKE3 XOF），壓不動。
pub fn data(seed: &str, n: usize) -> Vec<u8> {
    let mut h = blake3::Hasher::new();
    h.update(format!("kist/test/{seed}").as_bytes());
    let mut out = vec![0u8; n];
    h.finalize_xof().fill(&mut out);
    out
}

/// 一個有子目錄、空目錄、空檔案、符號連結、多 chunk 檔案的來源。
pub fn sample_source(root: &Path) {
    std::fs::create_dir_all(root.join("docs/deep/er")).unwrap();
    std::fs::create_dir_all(root.join("empty")).unwrap();
    std::fs::write(root.join("a.txt"), b"hello").unwrap();
    std::fs::write(root.join("empty.bin"), b"").unwrap();
    std::fs::write(root.join("docs/big.bin"), data("big", 300 * 1024)).unwrap();
    std::fs::write(root.join("docs/deep/er/note"), b"deep").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink("a.txt", root.join("link")).unwrap();
}

/// 扮演維護角色：在世代 `g` 之上寫 head `g+1`（主體與 `.r1`）。`records` 是 `Map(g+1)` 的全部記錄，依名稱
/// 分片；`lone` 是只寫主體、不寫 `.r1`（寫入者在兩次 PUT 之間）。回傳 head 的密封 bytes。
pub async fn write_generation(repo: &Repo, records: Vec<Record>, lone: bool) -> Gen {
    let listing = repo.list_heads(None).await.unwrap();
    let g = *listing.keys().next_back().unwrap();
    let base = repo.read_head(g).await.unwrap();
    let mut by_shard: BTreeMap<u8, Vec<Record>> = BTreeMap::new();
    for r in records {
        by_shard.entry(r.id.as_bytes()[0]).or_default().push(r);
    }
    let mut shards = vec![None; SHARDS];
    for (i, rs) in by_shard {
        let plain = Shard::new(i, rs).encode().unwrap();
        let sealed = repo.keys().seal_shard(&plain).unwrap();
        let id = ObjectId::of(&sealed);
        // §13：replicas=1 時分片也寫 .r1（先主體）
        for replica in [false, true] {
            if replica && base.head.settings.replicas == 0 {
                continue;
            }
            repo.backend()
                .put_if_absent(&Key::Shard { id, replica }.path(), sealed.clone())
                .await
                .unwrap();
        }
        shards[i as usize] = Some(id);
    }
    let head = Head {
        g: g + 1,
        w: i64::try_from(time::OffsetDateTime::now_utc().unix_timestamp_nanos()).unwrap(),
        parent: Some(base.hash),
        shards,
        ..base.head
    };
    let sealed = repo
        .keys()
        .seal_head(g + 1, &head.encode().unwrap())
        .unwrap();
    for replica in [false, true] {
        if replica && lone {
            continue;
        }
        repo.backend()
            .put_if_absent(&Key::Head { g: g + 1, replica }.path(), sealed.clone())
            .await
            .unwrap();
    }
    g + 1
}

/// Unix 奈秒（head 的 `w`）。
pub fn ns(t: time::OffsetDateTime) -> i64 {
    i64::try_from(t.unix_timestamp_nanos()).unwrap()
}

/// 扮演維護角色：在最大世代之上寫 head `g+1`（主體與 `.r1`，分片照舊、`w` 是真的時間），`edit` 改內容。
/// `lone` 是只寫主體。回傳新的世代。
pub async fn write_head_with(repo: &Repo, lone: bool, edit: impl FnOnce(&mut Head)) -> Gen {
    let listing = repo.list_heads(None).await.unwrap();
    let g = *listing.keys().next_back().unwrap();
    let base = repo.read_head(g).await.unwrap();
    let mut head = Head {
        g: g + 1,
        w: ns(time::OffsetDateTime::now_utc()),
        parent: Some(base.hash),
        ..base.head
    };
    edit(&mut head);
    let sealed = repo
        .keys()
        .seal_head(g + 1, &head.encode().unwrap())
        .unwrap();
    for replica in [false, true] {
        if replica && lone {
            continue;
        }
        repo.backend()
            .put_if_absent(&Key::Head { g: g + 1, replica }.path(), sealed.clone())
            .await
            .unwrap();
    }
    g + 1
}

/// 扮演 prune 的 P4：列表裡的每個 pack 與 mpack 讀 trailer，記成 live（`since` 是新世代）。`status` 改寫
/// 指定 pack 的狀態。
pub async fn index_everything(
    repo: &Repo,
    status: &BTreeMap<ObjectId, kist_format::v4::index::Status>,
) -> Vec<Record> {
    let next = repo
        .list_heads(None)
        .await
        .unwrap()
        .keys()
        .next_back()
        .unwrap()
        + 1;
    let reader = repo.reader(None).await.unwrap();
    let mut out = Vec::new();
    for prefix in ["packs", "mpacks"] {
        for o in repo.backend().list(prefix).await.unwrap() {
            let key = Key::parse(&o.key).unwrap();
            let (client, id, kind) = match &key {
                Key::Pack { client, id } => (*client, *id, kist_format::v4::pack::Kind::Data),
                Key::Mpack {
                    client,
                    id,
                    replica: false,
                } => (*client, *id, kist_format::v4::pack::Kind::Meta),
                _ => continue,
            };
            let t = reader.trailer(&key).await.unwrap();
            out.push(Record {
                id,
                client,
                kind,
                size: o.size,
                entries: t.entries,
                by_prune: client == ClientId::MAINT,
                since: next,
                status: status
                    .get(&id)
                    .copied()
                    .unwrap_or(kist_format::v4::index::Status::LIVE),
                held: false,
            });
        }
    }
    out
}

/// 包住 `InMemory` 的 store：數每一種請求、記下 PUT 的順序，也能讓指定的 key 在寫入成功之後回「已存在」
/// 一次（S3 回 5xx、client 重試、第一次其實已寫入，§12）。
pub mod hooked {
    use std::collections::{BTreeMap, HashMap};
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use bytes::Bytes;
    use futures::stream::BoxStream;
    use futures::StreamExt as _;
    use object_store::memory::InMemory;
    use object_store::path::Path as StorePath;
    use object_store::{
        GetOptions, GetResult, ListResult, ObjectMeta, ObjectStore, ObjectStoreExt as _,
        PutOptions, PutPayload, PutResult,
    };

    #[derive(Debug, Default)]
    pub struct Hooked {
        inner: Arc<InMemory>,
        counts: Mutex<BTreeMap<&'static str, usize>>,
        puts: Mutex<Vec<String>>,
        ranged: Mutex<Vec<String>>,
        /// 這些 key 第一次寫入成功之後回「已存在」。
        pub echo: Mutex<Vec<String>>,
        /// 這些 key 的下一次寫入失敗（沒有寫進去，回錯誤）。
        pub fail: Mutex<Vec<String>>,
        /// (A, B, bytes)：A 被讀（GET，不含 HEAD 與 range）之後建立 B——在一個命令的兩步之間插入別人的寫入。
        pub after_get: Mutex<Vec<(String, String, Vec<u8>)>>,
        /// 刪除過的 key，依順序。
        deleted: Arc<Mutex<Vec<String>>>,
        /// key 以它開頭的寫入，寫進去的 bytes 被改掉一個 byte（寫入路徑上的損壞；讀回驗證抓得到）。
        pub corrupt_puts_under: Mutex<Option<String>>,
        /// 這些 key 的下一次讀取（GET、HEAD、range）失敗（暫時的錯誤，例如逾時）。
        pub fail_get: Mutex<Vec<String>>,
        /// 這些 key 的下一次刪除被拒絕（Object Lock、權限）。
        pub fail_delete: Arc<Mutex<Vec<String>>>,
        /// (A, 秒)：GET 或 PUT 了 A 之後，虛擬時鐘前進這麼多（在一個命令的兩步之間經過時間）。
        pub advance_after_get: Mutex<Vec<(String, i64)>>,
        pub advance_after_put: Mutex<Vec<(String, i64)>>,
        /// 有虛擬時鐘時，修改時間是寫入當下的虛擬後端時間（[`super::VClock`]），不是真的時間。
        clock: Option<Arc<super::VClock>>,
        stamps: Mutex<HashMap<String, chrono::DateTime<chrono::Utc>>>,
    }

    impl std::fmt::Display for Hooked {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "Hooked")
        }
    }

    impl Hooked {
        fn bump(&self, what: &'static str) {
            *self.counts.lock().unwrap().entry(what).or_default() += 1;
        }

        /// 每一種請求的次數（`put`、`get`、`range`、`head`、`list`、`delete`）。
        pub fn counts(&self) -> BTreeMap<&'static str, usize> {
            self.counts.lock().unwrap().clone()
        }

        pub fn total(&self) -> usize {
            self.counts.lock().unwrap().values().sum()
        }

        pub fn reset(&self) {
            self.counts.lock().unwrap().clear();
            self.puts.lock().unwrap().clear();
            self.ranged.lock().unwrap().clear();
        }

        /// range read 過的 key，依順序。
        pub fn ranged(&self) -> Vec<String> {
            self.ranged.lock().unwrap().clone()
        }

        pub fn puts(&self) -> Vec<String> {
            self.puts.lock().unwrap().clone()
        }

        /// 刪除過的 key，依順序。
        pub fn deleted(&self) -> Vec<String> {
            self.deleted.lock().unwrap().clone()
        }

        /// `hooks` 裡有 `key` 就拿掉它、讓虛擬時鐘前進。
        fn maybe_advance(&self, hooks: &Mutex<Vec<(String, i64)>>, key: &str) {
            let secs = {
                let mut h = hooks.lock().unwrap();
                h.iter().position(|(k, _)| k == key).map(|i| h.remove(i).1)
            };
            if let (Some(secs), Some(c)) = (secs, &self.clock) {
                c.advance(secs);
            }
        }

        fn stamp(&self, mut meta: ObjectMeta) -> ObjectMeta {
            if let Some(t) = self.stamps.lock().unwrap().get(meta.location.as_ref()) {
                meta.last_modified = *t;
            }
            meta
        }
    }

    #[async_trait]
    impl ObjectStore for Hooked {
        async fn put_opts(
            &self,
            location: &StorePath,
            payload: PutPayload,
            opts: PutOptions,
        ) -> object_store::Result<PutResult> {
            self.bump("put");
            self.puts.lock().unwrap().push(location.to_string());
            {
                let mut fail = self.fail.lock().unwrap();
                if let Some(i) = fail.iter().position(|k| k == location.as_ref()) {
                    fail.remove(i);
                    return Err(object_store::Error::Generic {
                        store: "Hooked",
                        source: "simulated write failure".into(),
                    });
                }
            }
            let corrupt = self
                .corrupt_puts_under
                .lock()
                .unwrap()
                .as_ref()
                .is_some_and(|p| location.as_ref().starts_with(p.as_str()));
            let payload = if corrupt {
                let mut b: Vec<u8> = payload.into_iter().flat_map(|c| c.to_vec()).collect();
                let mid = b.len() / 2;
                b[mid] ^= 0x55;
                PutPayload::from(b)
            } else {
                payload
            };
            let r = self.inner.put_opts(location, payload, opts).await?;
            if let Some(c) = &self.clock {
                let t = c.backend_now();
                let t =
                    chrono::DateTime::from_timestamp(t.unix_timestamp(), t.nanosecond()).unwrap();
                self.stamps.lock().unwrap().insert(location.to_string(), t);
            }
            self.maybe_advance(&self.advance_after_put, location.as_ref());
            let mut echo = self.echo.lock().unwrap();
            if let Some(i) = echo.iter().position(|k| k == location.as_ref()) {
                echo.remove(i);
                return Err(object_store::Error::AlreadyExists {
                    path: location.to_string(),
                    source: "simulated retry after a 5xx".into(),
                });
            }
            Ok(r)
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
            self.bump(if options.head {
                "head"
            } else if options.range.is_some() {
                self.ranged.lock().unwrap().push(location.to_string());
                "range"
            } else {
                "get"
            });
            let plain = !options.head && options.range.is_none();
            {
                let mut fail = self.fail_get.lock().unwrap();
                if let Some(i) = fail.iter().position(|k| k == location.as_ref()) {
                    fail.remove(i);
                    return Err(object_store::Error::Generic {
                        store: "Hooked",
                        source: "simulated read failure (timeout)".into(),
                    });
                }
            }
            let mut r = self.inner.get_opts(location, options).await?;
            r.meta = self.stamp(r.meta);
            if plain {
                self.maybe_advance(&self.advance_after_get, location.as_ref());
                let hook = {
                    let mut hooks = self.after_get.lock().unwrap();
                    hooks
                        .iter()
                        .position(|(a, _, _)| a == location.as_ref())
                        .map(|i| hooks.remove(i))
                };
                if let Some((_, b, bytes)) = hook {
                    self.put_opts(
                        &StorePath::from(b.as_str()),
                        bytes.into(),
                        PutOptions::default(),
                    )
                    .await?;
                }
            }
            Ok(r)
        }

        async fn get_ranges(
            &self,
            location: &StorePath,
            ranges: &[std::ops::Range<u64>],
        ) -> object_store::Result<Vec<Bytes>> {
            self.bump("range");
            self.ranged.lock().unwrap().push(location.to_string());
            self.inner.get_ranges(location, ranges).await
        }

        fn delete_stream(
            &self,
            locations: BoxStream<'static, object_store::Result<StorePath>>,
        ) -> BoxStream<'static, object_store::Result<StorePath>> {
            self.bump("delete");
            let log = Arc::clone(&self.deleted);
            let fail = Arc::clone(&self.fail_delete);
            let inner = Arc::clone(&self.inner);
            locations
                .then(move |l| {
                    let (log, fail, inner) =
                        (Arc::clone(&log), Arc::clone(&fail), Arc::clone(&inner));
                    async move {
                        let p = l?;
                        let refused = {
                            let mut f = fail.lock().unwrap();
                            f.iter()
                                .position(|k| k == p.as_ref())
                                .map(|i| f.remove(i))
                                .is_some()
                        };
                        if refused {
                            return Err(object_store::Error::Generic {
                                store: "Hooked",
                                source: "simulated delete refusal (Object Lock)".into(),
                            });
                        }
                        log.lock().unwrap().push(p.to_string());
                        inner.delete(&p).await?;
                        Ok(p)
                    }
                })
                .boxed()
        }

        fn list(
            &self,
            prefix: Option<&StorePath>,
        ) -> BoxStream<'static, object_store::Result<ObjectMeta>> {
            self.bump("list");
            let stamps = self.stamps.lock().unwrap().clone();
            self.inner
                .list(prefix)
                .map(move |m| {
                    m.map(|mut m| {
                        if let Some(t) = stamps.get(m.location.as_ref()) {
                            m.last_modified = *t;
                        }
                        m
                    })
                })
                .boxed()
        }

        async fn list_with_delimiter(
            &self,
            prefix: Option<&StorePath>,
        ) -> object_store::Result<ListResult> {
            self.bump("list");
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

    /// 以它為底的 Backend（位置當成本機：只影響 replicas 的預設與快取目錄的名稱）。
    pub fn backend(name: &str) -> (Arc<Hooked>, kist_backend::Backend) {
        backend_with(name, None)
    }

    /// 同 [`backend`]，修改時間取自虛擬時鐘。
    pub fn backend_with(
        name: &str,
        clock: Option<Arc<super::VClock>>,
    ) -> (Arc<Hooked>, kist_backend::Backend) {
        let store = Arc::new(Hooked {
            clock,
            ..Hooked::default()
        });
        let b = kist_backend::Backend::from_store(
            store.clone(),
            kist_backend::RepoLocation::Local(format!("/hooked/{name}").into()),
        );
        (store, b)
    }
}

/// 虛擬時鐘（§11.1 的兩個時鐘）：[`hooked`] 後端的修改時間與維護命令、backup 的牆鐘都從這裡讀。`advance`
/// 讓兩邊一起前進；兩邊各有自己的偏差（後端時鐘與寫入者的牆鐘可以各自出錯）。起點是建立時的真實時間。
#[derive(Debug)]
pub struct VClock {
    base: time::OffsetDateTime,
    state: std::sync::Mutex<VState>,
}

#[derive(Debug, Default, Clone, Copy)]
struct VState {
    /// 從起點經過的秒數（單調時鐘也走這麼多）。
    elapsed: i64,
    backend_skew: i64,
    writer_skew: i64,
}

impl VClock {
    pub fn new() -> std::sync::Arc<Self> {
        let now = time::OffsetDateTime::now_utc();
        std::sync::Arc::new(Self {
            base: now.replace_nanosecond(0).unwrap(),
            state: std::sync::Mutex::new(VState::default()),
        })
    }

    fn state(&self) -> VState {
        *self.state.lock().unwrap()
    }

    /// 兩個時鐘一起前進。
    pub fn advance(&self, secs: i64) {
        self.state.lock().unwrap().elapsed += secs;
    }

    pub fn advance_hours(&self, hours: i64) {
        self.advance(hours * 3600);
    }

    /// 後端時鐘的偏差（秒，正的是快）。
    pub fn set_backend_skew(&self, secs: i64) {
        self.state.lock().unwrap().backend_skew = secs;
    }

    /// 寫入者牆鐘的偏差（秒，正的是快）。單調時鐘不受影響。
    pub fn set_writer_skew(&self, secs: i64) {
        self.state.lock().unwrap().writer_skew = secs;
    }

    pub fn backend_now(&self) -> time::OffsetDateTime {
        let s = self.state();
        self.base + time::Duration::seconds(s.elapsed + s.backend_skew)
    }

    pub fn writer_now(&self) -> time::OffsetDateTime {
        let s = self.state();
        self.base + time::Duration::seconds(s.elapsed + s.writer_skew)
    }

    /// 給維護命令與 backup 的時鐘：牆鐘是寫入者的時鐘，單調時鐘是經過的秒數。
    pub fn clock(self: &std::sync::Arc<Self>) -> kist_core::v4::Clock {
        let c = std::sync::Arc::clone(self);
        kist_core::v4::Clock::new(move || kist_core::v4::Reading {
            wall: c.writer_now(),
            boot: Duration::from_secs(u64::try_from(c.state().elapsed).unwrap() + 1_000_000),
        })
    }

    /// 維護命令的選項，時鐘是這個。
    pub fn maint(self: &std::sync::Arc<Self>) -> kist_core::v4::MaintOptions {
        kist_core::v4::MaintOptions {
            clock: self.clock(),
        }
    }
}

/// 以 [`hooked`] 為底的 repo。
pub struct HookedRepo {
    pub dir: tempfile::TempDir,
    pub store: std::sync::Arc<hooked::Hooked>,
    pub backend: Backend,
}

impl HookedRepo {
    pub async fn new(opts: InitOptions) -> Self {
        Self::with_clock(opts, None).await
    }

    /// 修改時間取自虛擬時鐘的 repo（init 寫的 head 0 也是）。
    pub async fn with_clock(opts: InitOptions, clock: Option<std::sync::Arc<VClock>>) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let (store, backend) = hooked::backend_with(&dir.path().display().to_string(), clock);
        Repo::init(backend.clone(), PASSWORD, opts).await.unwrap();
        Self {
            dir,
            store,
            backend,
        }
    }

    /// 不用本地快取打開。
    pub async fn open_uncached(&self) -> Repo {
        Repo::open(self.backend.clone(), PASSWORD, None)
            .await
            .unwrap()
    }

    pub async fn open_as(&self, cache: &str) -> Repo {
        let root = self.dir.path().join("cache").join(cache);
        Repo::open(self.backend.clone(), PASSWORD, Some(&root))
            .await
            .unwrap()
    }

    pub async fn keys(&self, prefix: &str) -> Vec<String> {
        let mut k: Vec<String> = self
            .backend
            .list(prefix)
            .await
            .unwrap()
            .into_iter()
            .map(|o| o.key)
            .collect();
        k.sort();
        k
    }
}

/// v3 測試的 client（`[0x11; 16]`）：從 v3 搬過來的測試照用。
pub fn backup_options() -> BackupOptions {
    opts(0x11)
}

/// snapshot 的 key（測試裡 backup 回傳的是字串）。
pub fn skey(s: &str) -> Key {
    Key::parse(s).unwrap()
}

pub fn random_bytes(seed: u64, len: usize) -> Vec<u8> {
    use rand::{RngExt, SeedableRng};
    let mut rng = rand::rngs::StdRng::seed_from_u64(seed);
    let mut v = vec![0u8; len];
    rng.fill(&mut v[..]);
    v
}

/// 一組有代表性的測試資料（同 v3 的 tests/common）：空檔、小檔、多 chunk 檔、可壓縮檔、子目錄、空目錄。
pub fn make_source(root: &Path) {
    std::fs::create_dir_all(root.join("sub/deeper")).unwrap();
    std::fs::create_dir_all(root.join("empty-dir")).unwrap();
    std::fs::write(root.join("empty.txt"), b"").unwrap();
    std::fs::write(root.join("small.txt"), b"hello kist\n").unwrap();
    std::fs::write(root.join("random.bin"), random_bytes(1, 300 * 1024)).unwrap();
    std::fs::write(root.join("zeros.bin"), vec![0u8; 200 * 1024]).unwrap();
    std::fs::write(root.join("sub/a.txt"), b"a").unwrap();
    std::fs::write(root.join("sub/deeper/b.bin"), random_bytes(2, 70 * 1024)).unwrap();
    std::fs::write(root.join("sub/中文檔名.txt"), "內容").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink("small.txt", root.join("link")).unwrap();
}

/// 遞迴列出所有項目（相對路徑 → 內容），symlink 記錄目標，目錄記錄 `<dir>`。
pub fn snapshot_dir(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut out = BTreeMap::new();
    for p in walk_files(root) {
        let rel = p.strip_prefix(root).unwrap().to_path_buf();
        let meta = std::fs::symlink_metadata(&p).unwrap();
        let content = if meta.file_type().is_symlink() {
            let target = std::fs::read_link(&p).unwrap();
            format!("symlink -> {}", target.display()).into_bytes()
        } else if meta.is_dir() {
            b"<dir>".to_vec()
        } else {
            std::fs::read(&p).unwrap()
        };
        out.insert(rel, content);
    }
    out
}

/// 所有檔案、目錄、symlink 的路徑（含空目錄），已排序。
pub fn walk_files(root: &Path) -> Vec<PathBuf> {
    fn rec(dir: &Path, out: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let p = entry.unwrap().path();
            let meta = std::fs::symlink_metadata(&p).unwrap();
            out.push(p.clone());
            if meta.is_dir() {
                rec(&p, out);
            }
        }
    }
    let mut out = Vec::new();
    rec(root, &mut out);
    out.sort();
    out
}

/// 比對兩個目錄：清單、內容、mtime。
pub fn assert_same_tree(a: &Path, b: &Path) {
    let sa = snapshot_dir(a);
    let sb = snapshot_dir(b);
    let ka: Vec<_> = sa.keys().collect();
    let kb: Vec<_> = sb.keys().collect();
    assert_eq!(ka, kb, "檔案清單不同");
    for (k, va) in &sa {
        assert_eq!(va, &sb[k], "內容不同：{}", k.display());
        let ma = std::fs::symlink_metadata(a.join(k)).unwrap();
        let mb = std::fs::symlink_metadata(b.join(k)).unwrap();
        if !ma.file_type().is_symlink() {
            assert_eq!(
                filetime::FileTime::from_last_modification_time(&ma),
                filetime::FileTime::from_last_modification_time(&mb),
                "mtime 不同：{}",
                k.display()
            );
        }
    }
}

/// `src` 在 restore 目標底下的位置（本機來源：目標 + 絕對路徑，§9）。
pub fn landed(target: &Path, src: &Path) -> PathBuf {
    target.join(src.strip_prefix("/").unwrap_or(src))
}

/// 一個 Entry 的零值（只有名稱、類型與 metadata 種類）：測試只寫它在乎的欄位。
pub fn entry(n: &[u8], t: u8, mk: u8) -> Entry {
    Entry::new(n.to_vec(), t, mk)
}

/// 手工寫一個 snapshot（測試敵意或腐壞的 repo）：`items` 與 roots 的 RootSeg 寫進 client `c` 前綴下的一個
/// mpack，記進一份 delta（讀取端從 delta 找到持有者），再以時間 `at`（奈秒，key 的時間戳與內容的 time 一致）
/// 密封 snapshot。回傳 key。
pub async fn hand_snapshot(
    repo: &Repo,
    c: ClientId,
    at: i64,
    roots: Vec<kist_format::v4::snapshot::Root>,
    items: Vec<kist_format::v4::meta::Item>,
) -> Key {
    let raw = items.iter().map(|i| i.encode().unwrap()).collect();
    hand_snapshot_raw(repo, c, at, roots, raw).await
}

/// [`hand_snapshot`]，item 是已經編碼的明文（測 `Item::encode` 不肯寫的東西，例如版本不認得的 item）。
pub async fn hand_snapshot_raw(
    repo: &Repo,
    c: ClientId,
    at: i64,
    roots: Vec<kist_format::v4::snapshot::Root>,
    items: Vec<Vec<u8>>,
) -> Key {
    use kist_core::v4::pack::PackWriter;
    use kist_format::v4::index::{Delta, DeltaPack};
    use kist_format::v4::keys::SnapTs;
    use kist_format::v4::meta::Item;
    use kist_format::v4::pack::Kind;
    use kist_format::v4::snapshot::{RootSeg, Snapshot};

    let keys = std::sync::Arc::clone(repo.keys());
    let mut w = PackWriter::new(
        std::sync::Arc::clone(&keys),
        Kind::Meta,
        u64::MAX,
        64 * 1024,
    );
    let mut seen = std::collections::HashSet::new();
    let mut add = |cbor: &[u8]| {
        let id = keys.meta_id(cbor);
        if seen.insert(id) {
            w.add(id.into(), cbor).unwrap();
        }
        id
    };
    for item in &items {
        add(item);
    }
    let roots_id = add(&Item::RootSeg(RootSeg::new(roots)).encode().unwrap());
    let p = w.finish().unwrap().unwrap();
    repo.backend()
        .put_if_absent(
            &Key::Mpack {
                client: c,
                id: p.id,
                replica: false,
            }
            .path(),
            p.bytes.clone(),
        )
        .await
        .unwrap();
    let delta = Delta::new(
        c,
        vec![DeltaPack {
            id: p.id,
            kind: Kind::Meta,
            size: p.bytes.len() as u64,
            entries: p.entries,
        }],
    )
    .encode()
    .unwrap();
    let sealed = keys.seal_delta(&c, &delta).unwrap();
    repo.backend()
        .put_if_absent(
            &Key::Delta {
                client: c,
                id: ObjectId::of(&sealed),
            }
            .path(),
            sealed,
        )
        .await
        .unwrap();
    let ts = SnapTs::from_nanos(at).unwrap();
    let snap = Snapshot {
        v: kist_format::v4::VERSION,
        roots: roots_id,
        time: at,
        host: "hand".to_owned(),
        user: String::new(),
        client: c,
        parent: None,
        stats: Default::default(),
    };
    let sealed = keys
        .seal_snapshot(&c, &ts, &snap.encode().unwrap())
        .unwrap();
    let key = Key::Snapshot {
        client: c,
        ts,
        replica: false,
    };
    repo.backend()
        .put_if_absent(&key.path(), sealed)
        .await
        .unwrap();
    key
}

/// 現在之後 `hours` 小時的奈秒時間（手工 snapshot 不與真的撞 key）。
pub fn hours_from_now(hours: i64) -> i64 {
    i64::try_from(time::OffsetDateTime::now_utc().unix_timestamp_nanos()).unwrap()
        + hours * 3_600_000_000_000
}
