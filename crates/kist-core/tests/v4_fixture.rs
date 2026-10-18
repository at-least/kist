//! 凍結的 v4 fixture repo（`tests/fixtures/v4/repo`）：由 Rust 寫出一次、提交進 git，**之後永不重生**——
//! 只在格式升版時換（ADR 018 的做法；v4 在 ADR 026 實作第七段凍結）。golden 與向量會跟著程式一起重錄，
//! 這份不會：它抓的是「Rust 自己前後一致、但格式悄悄變了」。
//!
//! 每個斷言對準一類漂移：
//! - 開得起來：KDF、槽的名稱與 AAD（綁 config 的 bytes）、master 的子金鑰；
//! - `check --read-data` 乾淨：ID、AEAD、pack 與 mpack 的排版與 trailer、head 與分片、snapshot 的 AAD；
//! - 還原與來源相同（內容、mtime、symlink、空目錄、超過 256 個 chunk 的 ChunkList）；
//! - 解碼後重新編碼等於存下的明文：config、每一代的 head、snapshot、每一個 metadata item（CBOR 欄位名、
//!   欄位順序、最短整數、切段的位置）；
//! - 同內容再備份零新 chunk：切塊邊界與 chunk ID；
//! - parity 修得回（check 列在 `repairable`，v4 不寫回）、mpack 的 `.r1` 讀得回來、`check --repair` 補得回主體。
//!
//! 限制：它抓不到 fixture 寫出當天就已存在的 bug；它只保證「今天的程式讀得懂、而且會寫出同樣的 bytes」。
//!
//! 重生（只在格式升版、單獨一個 commit）：
//! `KIST_WRITE_FIXTURE=1 cargo test -p kist-core --test v4_fixture -- --ignored write_fixture`

#![cfg(unix)]

mod v4_common;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use kist_backend::Backend;
use kist_core::v4::check::CheckOptions;
use kist_core::v4::pack::{open_meta, trailer_of};
use kist_core::v4::{BackupOptions, InitOptions, Repo, SnapshotInfo};
use kist_crypto::KdfCost;
use kist_format::v4::config::{Chunker, Config};
use kist_format::v4::ids::{ClientId, ObjectId};
use kist_format::v4::index::{Head, Settings, DEFAULT_GRACE};
use kist_format::v4::keys::{Key, REPLICA};
use kist_format::v4::meta::{decode_item, Item};
use kist_format::v4::snapshot::{split_locator, Snapshot};
use v4_common::{assert_same_tree, random_bytes, walk_files};

const PASSWORD: &[u8] = b"kist fixture v4";
const CLIENT: ClientId = ClientId::from_bytes([0xF4; 16]);
const HOST: &str = "fixture-host";
/// 所有來源項目的 mtime（秒）；第二代的 readme 另加 60 秒。只要固定即可。
const MTIME: i64 = 1_790_000_000;

fn fixture_repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/v4/repo")
}

/// 最小的 chunker（64/256/1024）：96 KiB 的檔案就切出超過 256 個 chunk，走 ChunkList；pack 與 mpack 取下限
/// 64 KiB，資料分散在好幾個 pack；replicas 1，mpack 與 snapshot 都有 `.r1`。
fn init_options() -> InitOptions {
    InitOptions {
        chunker: Chunker {
            min: 64,
            avg: 256,
            max: 1024,
        },
        kdf: KdfCost {
            m_cost_kib: 8,
            t_cost: 1,
            p_cost: 1,
        },
        settings: Some(Settings {
            grace: DEFAULT_GRACE,
            replicas: 1,
            pack_target: 64 * 1024,
            mpack_target: 64 * 1024,
        }),
    }
}

fn backup_options(client: ClientId) -> BackupOptions {
    let mut o = BackupOptions::new(client, HOST, "fixture");
    o.parity = 2;
    o
}

/// 第 `generation` 代的來源內容（1 或 2；只有 `docs/readme.txt` 不同）。
fn build_source(root: &Path, generation: u8) {
    std::fs::create_dir_all(root.join("docs")).unwrap();
    std::fs::create_dir_all(root.join("data")).unwrap();
    std::fs::create_dir_all(root.join("empty-dir")).unwrap();
    std::fs::create_dir_all(root.join("nested/a/b/c")).unwrap();
    std::fs::write(root.join("docs/中文檔名.txt"), "內容").unwrap();
    std::fs::write(root.join("data/random.bin"), random_bytes(0xF4, 96 * 1024)).unwrap();
    std::fs::write(root.join("data/zeros.bin"), vec![0u8; 32 * 1024]).unwrap();
    std::fs::write(root.join("data/empty.txt"), b"").unwrap();
    std::fs::write(root.join("nested/a/b/c/deep.txt"), b"deep\n").unwrap();
    std::os::unix::fs::symlink("docs/readme.txt", root.join("link")).unwrap();
    write_readme(root, generation);
    set_metadata(root, generation);
}

fn write_readme(root: &Path, generation: u8) {
    let text: &[u8] = match generation {
        1 => b"kist fixture v4\n",
        _ => b"kist fixture v4, second snapshot\n",
    };
    std::fs::write(root.join("docs/readme.txt"), text).unwrap();
}

/// 權限與 mtime 一律設成固定值。由深到淺設定：子項目先設，才不會又動到父目錄的 mtime。
fn set_metadata(root: &Path, generation: u8) {
    for p in walk_files(root).into_iter().rev() {
        let meta = std::fs::symlink_metadata(&p).unwrap();
        let secs = if generation == 2 && p.ends_with("docs/readme.txt") {
            MTIME + 60
        } else {
            MTIME
        };
        let t = filetime::FileTime::from_unix_time(secs, 0);
        if meta.file_type().is_symlink() {
            filetime::set_symlink_file_times(&p, t, t).unwrap();
            continue;
        }
        let mode = if meta.is_dir() { 0o755 } else { 0o644 };
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(mode)).unwrap();
        filetime::set_file_times(&p, t, t).unwrap();
    }
}

/// 把提交的 fixture 複製到暫存目錄；測試永遠不動 git 裡的那份。
fn copy_fixture() -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let dest = tmp.path().join("repo");
    let from = fixture_repo();
    assert!(
        from.join(Key::Config.path()).is_file(),
        "fixture 不存在：{}（見檔頭的重生指令）",
        from.display()
    );
    for p in walk_files(&from) {
        let rel = p.strip_prefix(&from).unwrap();
        let to = dest.join(rel);
        if p.is_dir() {
            std::fs::create_dir_all(&to).unwrap();
        } else {
            std::fs::create_dir_all(to.parent().unwrap()).unwrap();
            std::fs::copy(&p, &to).unwrap();
        }
    }
    (tmp, dest)
}

async fn open(repo_dir: &Path) -> Repo {
    Repo::open(Backend::local(repo_dir).unwrap(), PASSWORD, None)
        .await
        .unwrap()
}

/// 兩個 snapshot，依 key 排序（第 1 代、第 2 代）。
async fn snapshots(repo: &Repo) -> Vec<SnapshotInfo> {
    let mut list = repo.list_snapshots().await.unwrap();
    list.sort_by_key(|s| s.key.path());
    list
}

/// 還原某個 snapshot，回傳來源根目錄在 `out` 底下的位置（§9 的落點：目標 + 定位的所有段）。
async fn restore(repo: &Repo, snap: &SnapshotInfo, out: &Path) -> PathBuf {
    let report = repo
        .restore(&snap.key, out, Default::default())
        .await
        .unwrap();
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    let reader = repo.reader(None).await.unwrap();
    let roots = reader.roots(&snap.snapshot.roots).await.unwrap();
    assert_eq!(roots.len(), 1);
    let mut dir = out.to_path_buf();
    for seg in split_locator(&roots[0].path).unwrap() {
        dir.push(String::from_utf8(seg).unwrap());
    }
    dir
}

/// 某個前綴底下的主體（不含 `.r1`）的 key 與 bytes。
fn objects_under(dir: &Path, prefix: &str) -> Vec<(Key, Vec<u8>)> {
    walk_files(&dir.join(prefix))
        .into_iter()
        .filter(|p| p.is_file() && !p.to_string_lossy().ends_with(REPLICA))
        .map(|p| {
            let rel = p.strip_prefix(dir).unwrap().to_string_lossy().into_owned();
            (Key::parse(&rel).unwrap(), std::fs::read(&p).unwrap())
        })
        .collect()
}

fn replica_of(dir: &Path, key: &Key) -> Vec<u8> {
    std::fs::read(dir.join(format!("{}{REPLICA}", key.path()))).unwrap()
}

#[tokio::test]
async fn fixture_opens_checks_and_restores() {
    let (tmp, dir) = copy_fixture();
    let repo = open(&dir).await;

    let snaps = snapshots(&repo).await;
    assert_eq!(
        snaps.len(),
        2,
        "{:?}",
        snaps.iter().map(|s| s.key.path()).collect::<Vec<_>>()
    );
    for s in &snaps {
        assert_eq!(s.snapshot.client, CLIENT);
        assert_eq!(s.snapshot.host, HOST);
    }
    assert_eq!(
        snaps[1].snapshot.parent.as_deref(),
        Some(snaps[0].key.path().as_str()),
        "the second snapshot reuses the first as its parent"
    );

    let report = repo
        .check(CheckOptions {
            read_data: true,
            repair: false,
        })
        .await
        .unwrap();
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    assert!(report.warnings.is_empty(), "{:?}", report.warnings);
    assert_eq!(report.snapshots, 2, "{report:?}");
    assert_eq!(
        report.generation, 0,
        "the fixture has no prune: head 0 only"
    );

    for (i, snap) in snaps.iter().enumerate() {
        let generation = i as u8 + 1;
        let expected = tmp.path().join(format!("expected-{generation}"));
        build_source(&expected, generation);
        let out = tmp.path().join(format!("out-{generation}"));
        let restored = restore(&repo, snap, &out).await;
        assert_same_tree(&expected, &restored);
    }
}

#[tokio::test]
async fn fixture_objects_reencode_byte_for_byte() {
    let (_tmp, dir) = copy_fixture();
    let repo = open(&dir).await;
    let keys = repo.keys();

    // config：明文 CBOR
    let stored = std::fs::read(dir.join(Key::Config.path())).unwrap();
    let config = Config::decode(&stored).unwrap();
    assert_eq!(config.encode().unwrap(), stored, "config 的編碼變了");
    assert_eq!(config, *repo.config());

    // head 0：主體與 .r1 相同，解開後重新編碼相同
    let heads = objects_under(&dir, "maint/gen");
    assert_eq!(heads.len(), 1, "the fixture has head 0 only");
    for (key, bytes) in &heads {
        let Key::Head { g, .. } = key else {
            panic!("{}", key.path());
        };
        assert_eq!(
            replica_of(&dir, key),
            *bytes,
            "{}: .r1 與主體不同",
            key.path()
        );
        let plain = keys.open_head(*g, bytes).unwrap();
        let head = Head::decode(&plain, *g).unwrap();
        assert_eq!(head.encode().unwrap(), plain, "{}: 編碼變了", key.path());
    }

    // snapshot：AAD 是 key 路徑；.r1 與主體相同
    let snaps = objects_under(&dir, "snapshots");
    assert_eq!(snaps.len(), 2);
    for (key, bytes) in &snaps {
        let Key::Snapshot { client, ts, .. } = key else {
            panic!("{}", key.path());
        };
        assert_eq!(
            replica_of(&dir, key),
            *bytes,
            "{}: .r1 與主體不同",
            key.path()
        );
        let plain = keys.open_snapshot(client, ts, bytes).unwrap();
        let snap = Snapshot::decode(&plain).unwrap();
        assert_eq!(snap.encode().unwrap(), plain, "{}: 編碼變了", key.path());
    }

    // 每一個 mpack 的每一項：trailer 讀得出來，item 解得開、重新編碼相同；.r1 與主體相同
    let mpacks = objects_under(&dir, "mpacks");
    assert!(mpacks.len() >= 2, "fixture 的 mpack 太少：{}", mpacks.len());
    let chunk_max = repo.config().chunker.max;
    let (mut items, mut chunk_lists) = (0usize, 0usize);
    for (key, bytes) in &mpacks {
        assert_eq!(
            replica_of(&dir, key),
            *bytes,
            "{}: .r1 與主體不同",
            key.path()
        );
        assert_eq!(
            ObjectId::of(bytes),
            match key {
                Key::Mpack { id, .. } => *id,
                _ => unreachable!(),
            },
            "{}: 名稱與內容不符",
            key.path()
        );
        let trailer = trailer_of(keys, key, bytes, chunk_max).unwrap();
        for e in &trailer.entries {
            let sealed = &bytes[e.o as usize..(e.o + e.l) as usize];
            let cbor = open_meta(keys, &e.i.to_meta(), sealed, e.r).unwrap();
            let item = decode_item(&cbor).unwrap();
            assert_eq!(
                item.encode().unwrap(),
                cbor,
                "{}: item {} 的編碼變了",
                key.path(),
                e.i
            );
            items += 1;
            if matches!(item, Item::ChunkList(_)) {
                chunk_lists += 1;
            }
        }
    }
    assert!(items >= 10, "fixture 的 metadata item 太少：{items}");
    assert!(
        chunk_lists >= 1,
        "fixture 應該含有超過 256 個 chunk 的檔案（ChunkList）"
    );

    // 資料 pack：名稱與內容相符
    let packs = objects_under(&dir, "packs");
    assert!(packs.len() >= 2, "fixture 的 pack 太少：{}", packs.len());
    for (key, bytes) in &packs {
        let Key::Pack { id, .. } = key else {
            panic!("{}", key.path());
        };
        assert_eq!(ObjectId::of(bytes), *id, "{}: 名稱與內容不符", key.path());
    }
}

#[tokio::test]
async fn same_content_backs_up_with_zero_new_chunks() {
    let (tmp, dir) = copy_fixture();
    let repo = open(&dir).await;
    let src = tmp.path().join("src");
    build_source(&src, 2);
    let summary = repo
        .backup(
            std::slice::from_ref(&src),
            backup_options(ClientId::from_bytes([0xF5; 16])),
        )
        .await
        .unwrap();
    assert_eq!(
        summary.report.chunks_new, 0,
        "切塊或 chunk ID 變了：{:?}",
        summary.report
    );
    assert!(summary.report.chunks_reused > 0, "{:?}", summary.report);
}

#[tokio::test]
async fn fixture_parity_is_repairable_and_replicas_serve_reads() {
    let (tmp, dir) = copy_fixture();

    // parity：翻轉一個 pack 中間的一個 byte，check --read-data 證明 parity 修得回（v4 不寫回）。
    let packs = objects_under(&dir, "packs");
    let (victim, bytes) = &packs[0];
    let mut damaged = bytes.clone();
    let mid = damaged.len() / 2;
    damaged[mid] ^= 0x01;
    std::fs::write(dir.join(victim.path()), &damaged).unwrap();
    let repo = open(&dir).await;
    let report = repo
        .check(CheckOptions {
            read_data: true,
            repair: false,
        })
        .await
        .unwrap();
    assert_eq!(report.repairable.len(), 1, "{report:?}");
    assert!(
        report.repairable[0].starts_with(&victim.path()),
        "{report:?}"
    );
    assert!(report.unrepairable.is_empty(), "{report:?}");
    std::fs::write(dir.join(victim.path()), bytes).unwrap();

    // .r1：刪掉第 2 代根目錄所在的 mpack 主體，還原改讀副本，結果不變；check --repair 補回主體。
    let snaps = snapshots(&repo).await;
    let reader = repo.reader(None).await.unwrap();
    let roots = reader.roots(&snaps[1].snapshot.roots).await.unwrap();
    let tree = roots[0].tree.unwrap();
    let holder = reader
        .state
        .read_candidates(&tree.into(), kist_format::v4::pack::Kind::Meta)
        .into_iter()
        .next()
        .unwrap();
    let body = Key::Mpack {
        client: holder.client,
        id: holder.pack,
        replica: false,
    };
    std::fs::remove_file(dir.join(body.path())).unwrap();
    let expected = tmp.path().join("expected");
    build_source(&expected, 2);
    let out = tmp.path().join("out");
    let restored = restore(&open(&dir).await, &snaps[1], &out).await;
    assert_same_tree(&expected, &restored);

    let report = open(&dir)
        .await
        .check(CheckOptions {
            read_data: true,
            repair: true,
        })
        .await
        .unwrap();
    assert_eq!(report.repaired.len(), 1, "{report:?}");
    assert!(report.repaired[0].starts_with(&body.path()), "{report:?}");
    assert!(dir.join(body.path()).is_file());
    let report = open(&dir)
        .await
        .check(CheckOptions {
            read_data: true,
            repair: false,
        })
        .await
        .unwrap();
    assert!(
        report.errors.is_empty() && report.warnings.is_empty(),
        "{report:?}"
    );
}

/// 寫出（覆蓋）提交的 fixture。只在格式升版時跑，並在單獨一個 commit 提交。
#[tokio::test]
#[ignore = "只在格式升版時重生 fixture；見檔頭"]
async fn write_fixture() {
    assert!(
        std::env::var_os("KIST_WRITE_FIXTURE").is_some(),
        "設 KIST_WRITE_FIXTURE=1 才會覆蓋提交的 fixture"
    );
    let dest = fixture_repo();
    let _ = std::fs::remove_dir_all(&dest);
    // 來源放在固定的路徑：它會成為 snapshot 裡的 root 定位（restore 的落點）。
    let src = std::env::temp_dir().join("kist-fixture-v4-src");
    let _ = std::fs::remove_dir_all(&src);
    build_source(&src, 1);
    let repo = Repo::init(Backend::local(&dest).unwrap(), PASSWORD, init_options())
        .await
        .unwrap();
    repo.backup(std::slice::from_ref(&src), backup_options(CLIENT))
        .await
        .unwrap();
    // 第 2 代原地改寫一個檔、只設它的 mtime：其他項目的 inode/ctime 不變，走快速路徑、沿用 parent 的子樹。
    write_readme(&src, 2);
    let t = filetime::FileTime::from_unix_time(MTIME + 60, 0);
    filetime::set_file_times(src.join("docs/readme.txt"), t, t).unwrap();
    let second = repo
        .backup(std::slice::from_ref(&src), backup_options(CLIENT))
        .await
        .unwrap();
    assert!(second.parent.is_some());
    assert!(second.report.subtrees > 0, "{:?}", second.report);
    // backup 留下的本地快取不屬於 fixture（Repo::open 沒給快取目錄，所以沒有）；刪掉來源
    let _ = std::fs::remove_dir_all(&src);
}
