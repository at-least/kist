//! v4 測試共用：小參數的 repo、讀回 snapshot、比對來源目錄、扮演維護角色寫 head 與分片。

#![allow(dead_code)]

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
        repo.backend()
            .put_if_absent(&Key::Shard { id, replica: false }.path(), sealed)
            .await
            .unwrap();
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
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use bytes::Bytes;
    use futures::stream::BoxStream;
    use object_store::memory::InMemory;
    use object_store::path::Path as StorePath;
    use object_store::{
        GetOptions, GetResult, ListResult, ObjectMeta, ObjectStore, PutOptions, PutPayload,
        PutResult,
    };

    #[derive(Debug, Default)]
    pub struct Hooked {
        inner: InMemory,
        counts: Mutex<BTreeMap<&'static str, usize>>,
        puts: Mutex<Vec<String>>,
        /// 這些 key 第一次寫入成功之後回「已存在」。
        pub echo: Mutex<Vec<String>>,
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
        }

        pub fn puts(&self) -> Vec<String> {
            self.puts.lock().unwrap().clone()
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
            let r = self.inner.put_opts(location, payload, opts).await?;
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
                "range"
            } else {
                "get"
            });
            self.inner.get_opts(location, options).await
        }

        async fn get_ranges(
            &self,
            location: &StorePath,
            ranges: &[std::ops::Range<u64>],
        ) -> object_store::Result<Vec<Bytes>> {
            self.bump("range");
            self.inner.get_ranges(location, ranges).await
        }

        fn delete_stream(
            &self,
            locations: BoxStream<'static, object_store::Result<StorePath>>,
        ) -> BoxStream<'static, object_store::Result<StorePath>> {
            self.bump("delete");
            self.inner.delete_stream(locations)
        }

        fn list(
            &self,
            prefix: Option<&StorePath>,
        ) -> BoxStream<'static, object_store::Result<ObjectMeta>> {
            self.bump("list");
            self.inner.list(prefix)
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
        let store = Arc::new(Hooked::default());
        let b = kist_backend::Backend::from_store(
            store.clone(),
            kist_backend::RepoLocation::Local(format!("/hooked/{name}").into()),
        );
        (store, b)
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
        let dir = tempfile::tempdir().unwrap();
        let (store, backend) = hooked::backend(&dir.path().display().to_string());
        Repo::init(backend.clone(), PASSWORD, opts).await.unwrap();
        Self {
            dir,
            store,
            backend,
        }
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
