//! 凍結的 v3 fixture repo（`tests/fixtures/v3/repo`）：由 Rust 寫出一次、提交進 git，
//! **之後永不重生**——只在格式升版時換（見 ADR 018）。它接手已移除的 Go 參考實作
//! 唯一有價值的驗證角色：抓「Rust 自己前後一致、但格式悄悄變了」。golden 與向量
//! 會跟著程式一起重錄，這份不會。
//!
//! 每個斷言對準一類漂移：
//! - 開得起來：KDF、金鑰階層、wrapped master 與 Invariants；
//! - `check --read-data` 乾淨：keyed hash 命名（tree ID、ChunkId）、AEAD、pack 排版與 trailer；
//! - 還原與來源相同（內容、mtime、symlink、空目錄、超過 256 個 chunk 的間接清單）；
//! - 解碼後重新編碼等於存下的明文：CBOR 欄位名、欄位順序、最短整數；
//! - 同內容再備份零新 chunk：切塊邊界與 ChunkId；
//! - parity 修復、`.r1` 副本讀回：sidecar 與副本格式。
//!
//! 限制：它抓不到 fixture 寫出當天就已存在的 bug；它只保證「今天的程式讀得懂、
//! 而且會寫出同樣的 bytes」。
//!
//! 重生（只在格式升版、單獨一個 commit）：
//! `KIST_WRITE_FIXTURE=1 cargo test -p kist-core --test fixture_v3 -- --ignored write_fixture`

#![cfg(unix)]

mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use common::{assert_same_tree, random_bytes, walk_files};
use kist_backend::Backend;
use kist_core::{BackupOptions, CheckOptions, InitOptions, Repository, SnapshotInfo};
use kist_crypto::KdfCost;
use kist_format::config::{ChunkerParams, RepoConfig};
use kist_format::snapshot::Snapshot;
use kist_format::tree::{content_type, Tree};
use kist_format::{cbor, keys, TreeId};
use time::OffsetDateTime;

const PASSWORD: &str = "kist fixture v3";
const CLIENT: [u8; 16] = [0xF1; 16];
const HOST: &str = "fixture-host";
/// 所有來源項目的 mtime（秒）；第二代的 readme 另加 60 秒。只要固定即可。
const MTIME: i64 = 1_790_000_000;

fn fixture_repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/v3/repo")
}

/// 最小的 chunker（64/256/1024）：96 KiB 的檔案就切出超過 256 個 chunk，
/// 走間接清單；pack 取下限 64 KiB，資料分散在好幾個 pack。
fn init_options() -> InitOptions {
    InitOptions {
        chunker: ChunkerParams {
            min: 64,
            avg: 256,
            max: 1024,
        },
        pack_target_size: 64 * 1024,
        kdf_cost: KdfCost {
            m_cost_kib: 8,
            t_cost: 1,
            p_cost: 1,
        },
        replicas: Some(1),
    }
}

fn backup_options(client: [u8; 16], now: OffsetDateTime) -> BackupOptions {
    BackupOptions {
        client_id: client,
        hostname: HOST.to_owned(),
        username: "fixture".to_owned(),
        now: Some(now),
        gc_grace: kist_core::DEFAULT_GC_GRACE,
        parity: 2,
        progress: None,
        source: kist_core::SourceSpec::default(),
    }
}

/// 第 `generation` 代的來源內容（1 或 2；只有 `docs/readme.txt` 不同）。
fn build_source(root: &Path, generation: u8) {
    std::fs::create_dir_all(root.join("docs")).unwrap();
    std::fs::create_dir_all(root.join("data")).unwrap();
    std::fs::create_dir_all(root.join("empty-dir")).unwrap();
    std::fs::create_dir_all(root.join("nested/a/b/c")).unwrap();
    std::fs::write(root.join("docs/中文檔名.txt"), "內容").unwrap();
    std::fs::write(root.join("data/random.bin"), random_bytes(0xF1, 96 * 1024)).unwrap();
    std::fs::write(root.join("data/zeros.bin"), vec![0u8; 32 * 1024]).unwrap();
    std::fs::write(root.join("data/empty.txt"), b"").unwrap();
    std::fs::write(root.join("nested/a/b/c/deep.txt"), b"deep\n").unwrap();
    std::os::unix::fs::symlink("docs/readme.txt", root.join("link")).unwrap();
    write_readme(root, generation);
    set_metadata(root, generation);
}

fn write_readme(root: &Path, generation: u8) {
    let text: &[u8] = match generation {
        1 => b"kist fixture v3\n",
        _ => b"kist fixture v3, second snapshot\n",
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
        from.join(keys::CONFIG).is_file(),
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

async fn open(repo_dir: &Path) -> Repository {
    Repository::open(Backend::local(repo_dir).unwrap(), PASSWORD.as_bytes())
        .await
        .unwrap()
}

/// 兩個 snapshot，依時間排序（第 1 代、第 2 代）。
async fn snapshots(repo: &Repository) -> Vec<SnapshotInfo> {
    let mut list = repo.list_snapshots().await.unwrap();
    list.sort_by(|a, b| a.key.cmp(&b.key));
    list
}

/// 還原某個 snapshot，回傳來源根目錄在 `out` 底下的位置。
async fn restore(repo: &Repository, snap: &SnapshotInfo, out: &Path) -> PathBuf {
    let report = repo
        .restore(&snap.key, out, Default::default())
        .await
        .unwrap();
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    let locator = snap.snapshot.roots[0].path.as_slice();
    out.join(kist_core::fsmeta::locator_to_relative(locator).unwrap())
}

fn files_under(dir: &Path) -> Vec<PathBuf> {
    walk_files(dir)
        .into_iter()
        .filter(|p| p.is_file() && !p.to_string_lossy().ends_with(keys::REPLICA_SUFFIX))
        .collect()
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
        snaps.iter().map(|s| &s.key).collect::<Vec<_>>()
    );
    for s in &snaps {
        assert_eq!(s.snapshot.client_id, CLIENT);
        assert_eq!(s.snapshot.host, HOST);
    }
    assert_eq!(
        snaps[1].snapshot.parent.as_deref(),
        Some(snaps[0].key.as_str())
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
    let keys_ = repo.keys();

    let stored = std::fs::read(dir.join(keys::CONFIG)).unwrap();
    let config: RepoConfig = cbor::decode(&stored).unwrap();
    assert_eq!(cbor::encode(&config).unwrap(), stored, "config 的編碼變了");

    let trees = files_under(&dir.join(keys::TREES_PREFIX));
    assert!(trees.len() >= 6, "fixture 的 tree 太少：{}", trees.len());
    let mut indirect = 0;
    for p in &trees {
        let name = p.file_name().unwrap().to_string_lossy().into_owned();
        let id = TreeId::from_bytes(*keys::object_id_from_key(&name).unwrap().as_bytes());
        let bytes = std::fs::read(p).unwrap();
        let replica = std::fs::read(format!("{}{}", p.display(), keys::REPLICA_SUFFIX)).unwrap();
        assert_eq!(replica, bytes, "tree {name} 的 .r1 與主體不同");
        let plain = keys_.open_tree(&id, &bytes).unwrap();
        let tree: Tree = cbor::decode(&plain).unwrap();
        assert_eq!(
            cbor::encode(&tree).unwrap(),
            plain,
            "tree {name} 的編碼變了"
        );
        indirect += tree
            .entries
            .iter()
            .filter(|e| e.content == content_type::INDIRECT)
            .count();
    }
    assert!(indirect >= 1, "fixture 應該含有間接 chunk 清單");

    let snaps = files_under(&dir.join(keys::SNAPSHOTS_PREFIX));
    assert_eq!(snaps.len(), 2);
    for p in &snaps {
        let key = p.strip_prefix(&dir).unwrap().to_string_lossy().into_owned();
        let bytes = std::fs::read(p).unwrap();
        let replica = std::fs::read(format!("{}{}", p.display(), keys::REPLICA_SUFFIX)).unwrap();
        assert_eq!(replica, bytes, "snapshot {key} 的 .r1 與主體不同");
        let plain = keys_.open_snapshot(&key, &bytes).unwrap();
        let snap: Snapshot = cbor::decode(&plain).unwrap();
        assert_eq!(
            cbor::encode(&snap).unwrap(),
            plain,
            "snapshot {key} 的編碼變了"
        );
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
            backup_options([0xF2; 16], OffsetDateTime::now_utc()),
        )
        .await
        .unwrap();
    assert_eq!(
        summary.report.chunks_new, 0,
        "切塊或 ChunkId 變了：{:?}",
        summary.report
    );
}

#[tokio::test]
async fn fixture_parity_repairs_and_replicas_serve_reads() {
    let (tmp, dir) = copy_fixture();

    // parity：翻轉一個 pack 中間的一個 byte，check --repair 用 sidecar 修回來。
    let packs = files_under(&dir.join(keys::PACKS_PREFIX));
    assert!(packs.len() >= 2, "fixture 的 pack 太少：{}", packs.len());
    let victim = &packs[0];
    let mut bytes = std::fs::read(victim).unwrap();
    let mid = bytes.len() / 2;
    bytes[mid] ^= 0x01;
    std::fs::write(victim, &bytes).unwrap();
    let repo = open(&dir).await;
    let report = repo
        .check(CheckOptions {
            read_data: true,
            repair: true,
        })
        .await
        .unwrap();
    assert_eq!(report.repaired.len(), 1, "{report:?}");
    assert!(report.errors.is_empty(), "{report:?}");

    // .r1：刪掉第 2 代的根 tree 主體，還原改讀副本，結果不變。
    let snaps = snapshots(&repo).await;
    let root = snaps[1].snapshot.roots[0].tree;
    std::fs::remove_file(dir.join(keys::tree(&root))).unwrap();
    let expected = tmp.path().join("expected");
    build_source(&expected, 2);
    let out = tmp.path().join("out");
    let restored = restore(&open(&dir).await, &snaps[1], &out).await;
    assert_same_tree(&expected, &restored);
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
    let work = tempfile::tempdir().unwrap();
    let src = work.path().join("src");
    build_source(&src, 1);
    let repo = Repository::init(
        Backend::local(&dest).unwrap(),
        PASSWORD.as_bytes(),
        init_options(),
    )
    .await
    .unwrap();
    // snapshot 時間用生成當下：commit 的 BackupTooLong 閘門以真實時鐘比對開始時間。
    let t0 = OffsetDateTime::now_utc();
    repo.backup(std::slice::from_ref(&src), backup_options(CLIENT, t0))
        .await
        .unwrap();
    // 第 2 代原地改寫一個檔、只設它的 mtime：其他項目的 inode/ctime 不變，走 parent
    // 快速路徑、沿用子目錄的 tree（產生 touch 物件）。
    write_readme(&src, 2);
    let t = filetime::FileTime::from_unix_time(MTIME + 60, 0);
    filetime::set_file_times(src.join("docs/readme.txt"), t, t).unwrap();
    repo.backup(
        std::slice::from_ref(&src),
        backup_options(CLIENT, t0 + time::Duration::minutes(1)),
    )
    .await
    .unwrap();
}
