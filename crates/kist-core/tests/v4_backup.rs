//! v4 的 init、open 與 backup（format-v4.md §3、§9、§11.2）：寫出來的東西讀得回來、物件放在對的前綴、沒變的
//! 第二次 backup 不寫新東西、大目錄改一個檔只重寫一段、各種 root。GC 的規則（B1–B5 的拒絕）在
//! `v4_backup_gc.rs`，跨 client 的沿用與請求數在 `v4_backup_dedup.rs`。

#[path = "v4_common/mod.rs"]
mod v4_common;

use std::collections::BTreeMap;

use kist_core::v4::Repo;
use kist_core::CoreError;
use kist_format::v4::keys::Key;
use kist_format::v4::meta::{content_type, node_type, Item};
use v4_common::*;

#[tokio::test]
async fn init_writes_config_one_slot_and_head_zero_and_open_checks_the_password() {
    let t = TestRepo::new().await;
    assert_eq!(
        t.keys("config").await.len(),
        0,
        "config is a single object, not a prefix"
    );
    assert!(t.backend.exists("config").await.unwrap());
    assert_eq!(t.keys("keys").await.len(), 1);
    assert_eq!(
        t.keys("maint/gen").await,
        [
            "maint/gen/0000000000000000",
            "maint/gen/0000000000000000.r1"
        ]
    );
    let repo = t.open().await;
    let head = repo.read_head(0).await.unwrap();
    assert_eq!(head.head.settings, settings(0));
    assert!(
        head.head.shards.iter().all(Option::is_none),
        "head 0 has an empty map"
    );

    let wrong = Repo::open(t.backend.clone(), b"nope", None)
        .await
        .unwrap_err();
    assert!(
        matches!(
            wrong,
            CoreError::Crypto(kist_crypto::CryptoError::WrongPassword)
        ),
        "{wrong}"
    );
    let again = Repo::init(t.backend.clone(), PASSWORD, init_options(0))
        .await
        .unwrap_err();
    assert!(matches!(again, CoreError::RepoExists), "{again}");

    let empty = tempfile::tempdir().unwrap();
    let none = Repo::open(
        kist_backend::Backend::local(empty.path()).unwrap(),
        PASSWORD,
        None,
    )
    .await
    .unwrap_err();
    assert!(matches!(none, CoreError::NotARepository), "{none}");
}

/// v3 repo 的 `config`（移除的 v3 fixture repo，ADR 018，commit 5cf1197）：v4 只讀 `v` 就拒絕。
const V3_CONFIG_HEX: &str = concat!(
    "a8617603677265706f5f696450a4fd84bbf24208a29fb821cc3eacd07b67637265617465641b18d8f56cb347979f6763",
    "68756e6b6572a3636d696e184063617667190100636d61781904006b7061636b5f7461726765741a000100006a6d696e",
    "5f72656164657203687265706c696361730164736c6f74a5617603646e616d656764656661756c746763726561746564",
    "1b18d8f56cb347979f636b6466a563616c67686172676f6e326964617401616d086170016473616c7450e64093c7069c",
    "5ed3aa900aaa7bc4462f677772617070656458829cb3ec48ae84789568e0cf2834679a54ac23916a4e33ee8679b8d6d2",
    "87434d30fc7396af186e422fc8f9fa6267d7732f701cb6910a821b5261614c6019ef288fdd03274fec8994a28a34dfa7",
    "7ca7565de6e05ad1a47776e9f7469cc1a16fd4bdfbd3a3ea17e4475cef2df9846c1201577240c0a91e1cfc605406e949",
    "457d311865e0",
);

#[tokio::test]
async fn a_v3_repository_is_refused_with_its_version() {
    let dir = tempfile::tempdir().unwrap();
    let backend = kist_backend::Backend::local(dir.path()).unwrap();
    let config: Vec<u8> = (0..V3_CONFIG_HEX.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&V3_CONFIG_HEX[i..i + 2], 16).unwrap())
        .collect();
    backend.put_if_absent("config", config).await.unwrap();
    let err = Repo::open(backend, PASSWORD, None).await.unwrap_err();
    assert!(err.to_string().contains("format v3"), "{err}");
}

#[tokio::test]
async fn the_first_backup_writes_everything_under_its_prefix_and_reads_back() {
    let t = TestRepo::new().await;
    let src = t.dir.path().join("src");
    sample_source(&src);
    let repo = t.open().await;
    let s = repo
        .backup(std::slice::from_ref(&src), opts(1))
        .await
        .unwrap();
    assert!(s.parent.is_none());
    assert_eq!(s.generation, 0);
    assert!(
        s.report.packs_new >= 1 && s.report.mpacks_new >= 1,
        "{:?}",
        s.report
    );
    assert_eq!(s.report.deltas_new, 1);
    assert_eq!(s.report.errors, 0);
    assert_eq!(s.stats.files, 4);
    assert_eq!(
        s.stats.dirs, 4,
        "docs, deep, er, empty; the root is not counted"
    );
    assert_eq!(s.stats.symlinks, 1);
    assert_eq!(s.stats.bytes, 5 + 300 * 1024 + 4);

    let c = client(1).to_hex();
    for prefix in ["packs", "mpacks", "deltas", "snapshots"] {
        let keys = t.keys(prefix).await;
        assert!(!keys.is_empty(), "{prefix}");
        assert!(
            keys.iter()
                .all(|k| k.starts_with(&format!("{prefix}/{c}/"))),
            "{prefix}: {keys:?}"
        );
        assert!(
            keys.iter().all(|k| !k.ends_with(".r1")),
            "replicas=0: {keys:?}"
        );
    }
    assert_eq!(t.keys("snapshots").await, std::slice::from_ref(&s.snapshot));
    assert!(t.keys("parity").await.is_empty());
    assert!(
        t.keys("maint/map").await.is_empty(),
        "backup never writes the map"
    );

    // 讀回：一個目錄 root 放在它的定位切段之下
    let mut expected = BTreeMap::new();
    let segs: Vec<String> = kist_format::v4::snapshot::split_locator(
        src.canonicalize().unwrap().as_os_str().as_encoded_bytes(),
    )
    .unwrap()
    .into_iter()
    .map(|s| String::from_utf8(s).unwrap())
    .collect();
    for (p, n) in source_tree(&src) {
        expected.insert(format!("{}/{p}", segs.join("/")), n);
    }
    assert_eq!(read_tree(&repo, &s.snapshot).await, expected);

    // snapshot 的內容與 key 核對；`time` 是開始時間
    let key = Key::parse(&s.snapshot).unwrap();
    let snap = repo.read_snapshot(&key).await.unwrap();
    assert_eq!(snap.client, client(1));
    assert_eq!(snap.host, "testhost");
    assert_eq!(snap.stats, s.stats);
}

#[tokio::test]
async fn an_unchanged_second_backup_writes_no_pack_and_reuses_the_parent() {
    let t = TestRepo::new().await;
    let src = t.dir.path().join("src");
    sample_source(&src);
    let repo = t.open().await;
    let first = repo
        .backup(std::slice::from_ref(&src), opts(1))
        .await
        .unwrap();
    let before: Vec<String> = [
        t.keys("packs").await,
        t.keys("mpacks").await,
        t.keys("deltas").await,
    ]
    .concat();
    let second = repo
        .backup(std::slice::from_ref(&src), opts(1))
        .await
        .unwrap();
    assert_eq!(second.parent.as_deref(), Some(first.snapshot.as_str()));
    let r = second.report;
    assert_eq!(
        (
            r.packs_new,
            r.mpacks_new,
            r.deltas_new,
            r.chunks_new,
            r.items_new
        ),
        (0, 0, 0, 0, 0),
        "{r:?}"
    );
    assert!(r.subtrees >= 1, "{r:?}");
    assert_eq!(second.stats, first.stats);
    let after: Vec<String> = [
        t.keys("packs").await,
        t.keys("mpacks").await,
        t.keys("deltas").await,
    ]
    .concat();
    assert_eq!(before, after);
    assert_eq!(t.keys("snapshots").await.len(), 2);
    assert_eq!(
        read_tree(&repo, &second.snapshot).await,
        read_tree(&repo, &first.snapshot).await
    );
}

/// §7：大目錄依內容切段，改一個檔只重寫它那一段、目錄的 Dir 與往上的路徑（這裡是 roots）。名稱依這個 repo 的
/// K_cut 挑，保證至少兩段（固定的兩萬個名稱在亂數的金鑰下約 1% 完全不切）。
#[tokio::test]
async fn one_changed_file_in_a_large_directory_rewrites_one_segment() {
    let t = TestRepo::new().await;
    let repo = t.open().await;
    let src = t.dir.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    let mut names = names_with_cuts(&repo, 2);
    while names.len() < 20_000 {
        names.push(format!("f{:06}", names.len()));
    }
    for n in &names {
        std::fs::write(src.join(n), b"").unwrap();
    }
    let first = repo
        .backup(std::slice::from_ref(&src), opts(1))
        .await
        .unwrap();
    let reader = repo.reader(None).await.unwrap();
    let snap = repo
        .read_snapshot(&Key::parse(&first.snapshot).unwrap())
        .await
        .unwrap();
    let roots = reader.roots(&snap.roots).await.unwrap();
    let Item::Dir(dir) = reader.item(&roots[0].tree.unwrap()).await.unwrap() else {
        panic!(
            "{} entries chosen to cut should need more than one segment",
            names.len()
        );
    };
    assert!(dir.segs.len() >= 2, "{} segments", dir.segs.len());

    let changed = names[names.len() / 2].clone();
    let path = src.join(&changed);
    std::fs::write(&path, b"changed").unwrap();
    filetime::set_file_mtime(&path, filetime::FileTime::from_unix_time(1_000_000_000, 0)).unwrap();
    let second = repo
        .backup(std::slice::from_ref(&src), opts(1))
        .await
        .unwrap();
    let r = second.report;
    assert_eq!(r.chunks_new, 1, "{r:?}");
    // 那一段、目錄的 Dir、roots
    assert_eq!(r.items_new, 3, "{r:?}");
    assert_eq!(r.files_reused, names.len() as u64 - 1, "{r:?}");
    let back = read_root_dir(&repo, &second.snapshot).await;
    assert_eq!(back.len(), names.len());
    assert_eq!(back[&changed], Node::File(b"changed".to_vec()));
}

/// §9 的 `Root.t`：檔案 root 帶自己的 Entry、目錄 root 帶 tree。本機來源先 canonicalize（ADR 020：symlink
/// 已解開），所以指向 a.txt 的 symlink 當來源就是 a.txt 本身。
#[tokio::test]
async fn file_and_directory_roots_record_their_type() {
    let t = TestRepo::new().await;
    let base = t.dir.path().join("src");
    sample_source(&base);
    let repo = t.open().await;
    let paths = [base.join("docs"), base.join("a.txt"), base.join("link")];
    let s = repo.backup(&paths, opts(1)).await.unwrap();
    let reader = repo.reader(None).await.unwrap();
    let snap = repo
        .read_snapshot(&Key::parse(&s.snapshot).unwrap())
        .await
        .unwrap();
    let roots = reader.roots(&snap.roots).await.unwrap();
    let types: Vec<(String, u8)> = roots
        .iter()
        .map(|r| {
            let p = String::from_utf8(r.path.clone()).unwrap();
            (p.rsplit('/').next().unwrap().to_owned(), r.t)
        })
        .collect();
    // 依 path 的 bytes 升冪；link 解開成 a.txt，去重
    assert_eq!(
        types,
        [
            ("a.txt".to_owned(), node_type::FILE),
            ("docs".to_owned(), node_type::DIR),
        ]
    );
    for r in &roots {
        r.validate().unwrap();
    }
    let file = roots[0].entry.as_ref().unwrap();
    assert_eq!((file.n.as_slice(), file.size), (&b"a.txt"[..], 5));
    let back = read_tree(&repo, &s.snapshot).await;
    assert!(back.values().any(|n| *n == Node::File(b"hello".to_vec())));
    assert!(back.keys().any(|k| k.ends_with("docs/deep/er/note")));

    // 一樣的 roots 再備一次：parent 照路徑與種類找到
    let again = repo.backup(&paths, opts(1)).await.unwrap();
    assert_eq!(again.parent.as_deref(), Some(s.snapshot.as_str()));
    assert_eq!(again.report.packs_new + again.report.mpacks_new, 0);
}

/// §7：超過 256 個 chunk 的檔案改用 chunk 清單（`ct=1`）。
#[tokio::test]
async fn a_large_file_uses_a_chunk_list() {
    let t = TestRepo::new().await;
    let src = t.dir.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    let content = data("large", 8 * 1024 * 1024);
    std::fs::write(src.join("large"), &content).unwrap();
    let repo = t.open().await;
    let s = repo
        .backup(std::slice::from_ref(&src), opts(1))
        .await
        .unwrap();
    let reader = repo.reader(None).await.unwrap();
    let snap = repo
        .read_snapshot(&Key::parse(&s.snapshot).unwrap())
        .await
        .unwrap();
    let roots = reader.roots(&snap.roots).await.unwrap();
    let entries = reader
        .dir(&roots[0].tree.unwrap())
        .await
        .unwrap()
        .rest(&reader)
        .await
        .unwrap();
    assert_eq!(entries[0].ct, content_type::INDIRECT);
    assert!(reader.file_chunks(&entries[0]).await.unwrap().len() > 256);
    assert_eq!(
        read_root_dir(&repo, &s.snapshot).await["large"],
        Node::File(content)
    );

    // 沒變：清單是 parent 的子樹，只判斷它的根
    let again = repo
        .backup(std::slice::from_ref(&src), opts(1))
        .await
        .unwrap();
    assert_eq!(again.report.files_reused, 1);
    assert_eq!(
        again.report.chunks_reused, 0,
        "no chunk is checked one by one"
    );
}

/// §13：replicas=1 時 mpack 與 snapshot 也寫 `.r1`，bytes 與主體相同；資料 pack 不寫。
#[tokio::test]
async fn replicas_one_writes_r1_copies_of_metadata_and_snapshots() {
    let t = TestRepo::with(init_options(1)).await;
    let src = t.dir.path().join("src");
    sample_source(&src);
    let repo = t.open().await;
    let s = repo
        .backup(std::slice::from_ref(&src), opts(1))
        .await
        .unwrap();
    let mpacks = t.keys("mpacks").await;
    let bodies: Vec<&String> = mpacks.iter().filter(|k| !k.ends_with(".r1")).collect();
    assert!(!bodies.is_empty());
    for b in &bodies {
        let r1 = format!("{b}.r1");
        assert!(mpacks.contains(&r1), "{r1}");
        assert_eq!(
            t.backend.get(b).await.unwrap(),
            t.backend.get(&r1).await.unwrap()
        );
    }
    assert!(t.keys("packs").await.iter().all(|k| !k.ends_with(".r1")));
    assert_eq!(
        t.keys("snapshots").await,
        [s.snapshot.clone(), format!("{}.r1", s.snapshot)]
    );
}

/// 沿用 parent 的子樹發生在每一層：root 變了（底下一個檔案改了），沒變的兄弟目錄照樣整棵沿用。
#[tokio::test]
async fn an_unchanged_sibling_directory_is_reused_as_one_subtree() {
    let t = TestRepo::new().await;
    let src = t.dir.path().join("src");
    sample_source(&src);
    let repo = t.open().await;
    repo.backup(std::slice::from_ref(&src), opts(1))
        .await
        .unwrap();
    let path = src.join("a.txt");
    std::fs::write(&path, b"changed").unwrap();
    filetime::set_file_mtime(&path, filetime::FileTime::from_unix_time(1_000_000_000, 0)).unwrap();
    let s = repo
        .backup(std::slice::from_ref(&src), opts(1))
        .await
        .unwrap();
    // 沒變的 docs/deep/er、docs/deep、docs、empty 由下往上各沿用一次（巢狀的也算）；root 與 roots 變了，
    // 重寫的只有 root 的 Seg 與 roots
    assert_eq!(s.report.subtrees, 4, "{:?}", s.report);
    assert_eq!(
        (s.report.chunks_new, s.report.items_new),
        (1, 2),
        "{:?}",
        s.report
    );
    let back = read_root_dir(&repo, &s.snapshot).await;
    assert_eq!(back["a.txt"], Node::File(b"changed".to_vec()));
    assert_eq!(back["docs/big.bin"], Node::File(data("big", 300 * 1024)));
}

/// 一層的記憶體 S3 來源：每個檔案帶一個 etag。
struct FlatS3 {
    locator: Vec<u8>,
    files: Vec<(Vec<u8>, Vec<u8>, Vec<u8>)>,
}

impl kist_backend::source::Source for FlatS3 {
    fn locator(&self) -> &[u8] {
        &self.locator
    }

    fn meta_kind(&self) -> u8 {
        kist_format::v4::meta::meta_kind::S3
    }

    fn list(
        &self,
        _dir: &[u8],
    ) -> Result<Box<dyn kist_backend::source::SortedItems + Send>, kist_backend::BackendError> {
        struct Items(std::vec::IntoIter<kist_backend::source::SourceItem>);
        impl kist_backend::source::SortedItems for Items {
            fn next_item(
                &mut self,
            ) -> Option<Result<kist_backend::source::SourceItem, kist_backend::BackendError>>
            {
                self.0.next().map(Ok)
            }
        }
        let items: Vec<_> = self
            .files
            .iter()
            .map(|(name, content, etag)| kist_backend::source::SourceItem {
                name: name.clone(),
                kind: kist_backend::source::SourceItemKind::File {
                    size: content.len() as u64,
                    mtime_ns: 1_000_000_000_000_000_000,
                    etag: Some(etag.clone()),
                    vern: None,
                },
            })
            .collect();
        Ok(Box::new(Items(items.into_iter())))
    }

    fn read(
        &self,
        file: &[u8],
    ) -> Result<Box<dyn std::io::Read + Send>, kist_backend::BackendError> {
        let (_, content, _) = self.files.iter().find(|(n, ..)| n == file).unwrap();
        Ok(Box::new(std::io::Cursor::new(content.clone())))
    }
}

/// §7：放不進格式的項目（這裡是超過 1 KiB 的 etag）記成那個項目的錯誤、不寫入，目錄照樣寫。
#[tokio::test]
async fn an_entry_that_does_not_fit_is_an_error_of_that_entry_only() {
    let t = TestRepo::new().await;
    let repo = t.open().await;
    let source = FlatS3 {
        locator: b"s3://bucket/data".to_vec(),
        files: vec![
            (b"a".to_vec(), b"first".to_vec(), b"e1".to_vec()),
            (b"b".to_vec(), b"too long".to_vec(), vec![b'x'; 2000]),
            (b"c".to_vec(), b"third".to_vec(), b"e3".to_vec()),
        ],
    };
    let mut o = opts(1);
    o.source =
        kist_core::SourceSpec::Injected(std::sync::Arc::new(source), b"s3://bucket/data".to_vec());
    let s = repo.backup(&[], o).await.unwrap();
    assert_eq!(s.report.errors, 1, "{:?}", s.report);
    let back = read_root_dir(&repo, &s.snapshot).await;
    assert_eq!(
        back.keys().collect::<Vec<_>>(),
        ["a", "c"],
        "the directory is written without the entry"
    );
    assert_eq!(back["c"], Node::File(b"third".to_vec()));
}

/// §13：parity 選配，資料 pack 與 mpack 都有一份同名的 `parity/<c>/<hex>`（v3 的排版、`v` 是 4），修得回
/// 壞掉的 bytes。超過 8 片在寫任何東西之前就拒絕。
#[tokio::test]
async fn parity_covers_every_pack_and_mpack_and_repairs_them() {
    let t = TestRepo::new().await;
    let src = t.dir.path().join("src");
    sample_source(&src);
    let repo = t.open().await;
    let mut o = opts(1);
    o.parity = 9;
    let err = repo
        .backup(std::slice::from_ref(&src), o)
        .await
        .unwrap_err();
    assert!(matches!(err, CoreError::Usage(_)), "{err}");
    assert!(t.keys("packs").await.is_empty());

    let mut o = opts(1);
    o.parity = 2;
    repo.backup(std::slice::from_ref(&src), o).await.unwrap();
    let packs = [t.keys("packs").await, t.keys("mpacks").await].concat();
    let parity = t.keys("parity").await;
    assert_eq!(parity.len(), packs.len(), "{parity:?}");
    for k in packs {
        let (client, id) = match Key::parse(&k).unwrap() {
            Key::Pack { client, id } | Key::Mpack { client, id, .. } => (client, id),
            other => panic!("{other:?}"),
        };
        let pk = Key::Parity { client, id }.path();
        let obj = kist_format::v4::parity::parse(&t.backend.get(&pk).await.unwrap()).unwrap();
        assert_eq!((obj.v, obj.parity_shards()), (4, 2));
        let bytes = t.backend.get(&k).await.unwrap();
        assert_eq!(obj.repair(&id, &bytes).unwrap(), bytes);
        let mut bad = bytes.clone();
        let mid = bad.len() / 2;
        bad[mid] ^= 0xff;
        assert_eq!(obj.repair(&id, &bad).unwrap(), bytes, "{k}");
    }
}

/// 排除規則、記號檔、大小上限與硬連結（走訪的骨架照 v3，ADR 020、021）：被排除的不進 snapshot、計進
/// `excluded`；硬連結的內容只算一次 bytes。
#[tokio::test]
async fn exclusions_and_hardlinks_follow_the_v3_walk() {
    let t = TestRepo::new().await;
    let src = t.dir.path().join("src");
    std::fs::create_dir_all(src.join("cache")).unwrap();
    std::fs::create_dir_all(src.join("keep")).unwrap();
    std::fs::write(src.join("keep/a.txt"), b"keep me").unwrap();
    std::fs::write(src.join("keep/b.tmp"), b"skip me").unwrap();
    std::fs::write(src.join("big.bin"), vec![1u8; 5000]).unwrap();
    std::fs::write(
        src.join("cache/CACHEDIR.TAG"),
        b"Signature: 8a477f597d28d172789f06886806bc55",
    )
    .unwrap();
    std::fs::write(src.join("cache/data"), b"cached").unwrap();
    std::fs::write(src.join("hard1"), b"shared content").unwrap();
    std::fs::hard_link(src.join("hard1"), src.join("hard2")).unwrap();
    let repo = t.open().await;
    let mut o = opts(1);
    o.exclude = kist_core::Exclusions::new(
        kist_core::PathFilter::new(&["*.tmp"]).unwrap(),
        &[],
        true,
        Some(1000),
        false,
    )
    .unwrap();
    let s = repo.backup(std::slice::from_ref(&src), o).await.unwrap();
    // b.tmp、big.bin、cache/data
    assert_eq!(s.report.excluded, 3, "{:?}", s.report);
    let back = read_root_dir(&repo, &s.snapshot).await;
    assert_eq!(
        back.keys().map(String::as_str).collect::<Vec<_>>(),
        [
            "cache",
            "cache/CACHEDIR.TAG",
            "hard1",
            "hard2",
            "keep",
            "keep/a.txt"
        ]
    );
    assert_eq!(back["hard2"], Node::File(b"shared content".to_vec()));
    // bytes：a.txt、CACHEDIR.TAG、硬連結的內容一次
    assert_eq!(s.stats.bytes, 7 + 43 + 14);
    assert_eq!(s.stats.files, 4);
    // 沒變的第二次走快速路徑：硬連結的兩個名字照樣只算一次
    let again = repo
        .backup(std::slice::from_ref(&src), {
            let mut o = opts(1);
            o.exclude = kist_core::Exclusions::new(
                kist_core::PathFilter::new(&["*.tmp"]).unwrap(),
                &[],
                true,
                Some(1000),
                false,
            )
            .unwrap();
            o
        })
        .await
        .unwrap();
    assert_eq!(again.report.files_reused, 4, "{:?}", again.report);
    assert_eq!(again.stats, s.stats);
}

/// 進度回報（kist-app 的網頁介面用）：每處理完一個項目一次（帶路徑），最後依序是 flush、verify、commit。
#[tokio::test]
async fn progress_is_reported_per_item_then_flush_verify_commit() {
    let t = TestRepo::new().await;
    let repo = t.open().await;
    let src = t.dir.path().join("src");
    std::fs::create_dir_all(src.join("sub")).unwrap();
    std::fs::write(src.join("a"), b"a").unwrap();
    std::fs::write(src.join("sub/b"), b"b").unwrap();
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let log = std::sync::Arc::clone(&seen);
    let mut o = opts(1);
    o.progress = Some(kist_core::v4::ProgressCallback(std::sync::Arc::new(
        move |p: &kist_core::v4::BackupProgress| {
            log.lock()
                .unwrap()
                .push((p.phase, p.current.clone(), p.stats.files));
        },
    )));
    repo.backup(std::slice::from_ref(&src), o).await.unwrap();
    let seen = seen.lock().unwrap().clone();
    let files: Vec<String> = seen
        .iter()
        .filter(|(phase, _, _)| *phase == "files")
        .map(|(_, current, _)| current.clone().unwrap())
        .collect();
    for name in ["a", "sub", "b"] {
        assert!(
            files.iter().any(|f| f.ends_with(name)),
            "{name} in {files:?}"
        );
    }
    let tail: Vec<&str> = seen.iter().rev().take(3).map(|(p, _, _)| *p).collect();
    assert_eq!(tail, ["commit", "verify", "flush"]);
    assert_eq!(seen.last().unwrap().2, 2, "stats travel with the events");
}
