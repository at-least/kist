//! restore 在「不理想」情況下的行為：重複還原、互相包含的來源、壞掉的 chunk、惡意名稱。

mod common;

use common::*;
use kist_core::RestoreOptions;
/// 第二次 restore 到同一個目錄（裡面已有 symlink）必須成功，結果仍然正確。
#[cfg(unix)]
#[tokio::test]
async fn restore_twice_into_same_target_succeeds() {
    let t = TestRepo::new().await;
    let src = t.dir.path().join("src");
    make_source(&src); // 含 link -> small.txt
    let repo = t.open().await;
    let s = repo
        .backup(std::slice::from_ref(&src), backup_options())
        .await
        .unwrap();
    let target = t.dir.path().join("out");
    repo.restore(&s.snapshot_key, &target, RestoreOptions::default())
        .await
        .unwrap();
    repo.restore(&s.snapshot_key, &target, RestoreOptions::default())
        .await
        .unwrap();
    let restored = target.join(src.strip_prefix("/").unwrap_or(&src));
    assert_same_tree(&src, &restored);
}

/// 來源 `/a` 與 `/a/b` 互相包含：只保留外層，restore 不會因為重建同一條路徑而失敗。
#[tokio::test]
async fn nested_source_paths_are_deduplicated() {
    let t = TestRepo::new().await;
    let a = t.dir.path().join("a");
    let b = a.join("b");
    std::fs::create_dir_all(&b).unwrap();
    std::fs::write(a.join("x"), b"x").unwrap();
    std::fs::write(b.join("y"), b"y").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink("y", b.join("link")).unwrap();
    let repo = t.open().await;
    let s = repo
        .backup(&[b.clone(), a.clone()], backup_options())
        .await
        .unwrap();
    assert_eq!(s.stats.files, 2, "b 在 a 裡面，只該算一次：{:?}", s.stats);
    let target = t.dir.path().join("out");
    repo.restore(&s.snapshot_key, &target, RestoreOptions::default())
        .await
        .unwrap();
    assert_same_tree(&a, &target.join(a.strip_prefix("/").unwrap_or(&a)));
}

/// 一個 chunk 壞掉：那個檔案失敗、其餘照常還原，錯誤在 summary 裡而不是整個 restore 中止。
#[tokio::test]
async fn corrupt_chunk_fails_only_that_file() {
    let t = TestRepo::new().await;
    let src = t.dir.path().join("src");
    make_source(&src);
    let repo = t.open().await;
    let s = repo
        .backup(std::slice::from_ref(&src), backup_options())
        .await
        .unwrap();
    // 翻掉每個 pack 的一個 byte（entry 區域），保證有檔案受影響
    for pack in walk_files(&t.repo_path().join("packs")) {
        if pack.is_file() {
            let mut bytes = std::fs::read(&pack).unwrap();
            bytes[40] ^= 0xff;
            std::fs::write(&pack, bytes).unwrap();
        }
    }
    let target = t.dir.path().join("out");
    let summary = repo
        .restore(&s.snapshot_key, &target, RestoreOptions::default())
        .await
        .unwrap();
    assert!(!summary.errors.is_empty(), "{summary:?}");
    let restored = target.join(src.strip_prefix("/").unwrap_or(&src));
    // 目錄結構與空檔一定還原得了
    assert!(restored.join("sub/deeper").is_dir());
    assert!(restored.join("empty.txt").is_file());
    // 用 symlink_metadata：`is_file()` 會跟隨 symlink，把 link 也算成檔案
    let files_ok = walk_files(&restored)
        .iter()
        .filter(|p| std::fs::symlink_metadata(p).unwrap().is_file())
        .count() as u64;
    assert_eq!(
        files_ok + summary.errors.len() as u64,
        s.stats.files,
        "還原成功 + 失敗 = 全部檔案；{summary:?}"
    );
}

/// tree 裡的子節點名稱由 repo 內容決定；含分隔符或 `..` 的名稱不能讓 restore 逃出目標目錄。
#[test]
fn child_names_that_escape_are_rejected() {
    use kist_core::fsmeta::validate_child_name;
    for bad in [&b""[..], b".", b"..", b"a/b", b"a\0b", b"/etc/passwd"] {
        assert!(validate_child_name(bad).is_err(), "{bad:?} 應該被拒絕");
    }
    // 反斜線在 Unix 是合法檔名，在 Windows 是分隔符
    assert_eq!(validate_child_name(b"a\\b").is_err(), cfg!(windows));
    for good in [
        &b"a"[..],
        b"..a",
        b"a..",
        "中文.txt".as_bytes(),
        b"weird name with spaces",
    ] {
        assert!(validate_child_name(good).is_ok(), "{good:?} 應該可以");
    }
}

/// snapshot 可以帶一個 symlink，另一個 root 的定位穿過它（`kist backup /
/// /data2/sub` 在 /data2 是 symlink 的機器上就是這個形狀；金鑰持有者也能
/// 手工寫出）。restore 不得透過那個 symlink 寫到目標之外——檔案路徑已有
/// 同樣防護（symlink-in-the-way 拒絕），目錄路徑跟進：create_dir_all 會
/// 跟隨 symlink，等於把寫入帶出使用者指名的目錄。
#[cfg(unix)]
#[tokio::test]
async fn restore_refuses_to_write_through_a_planted_symlink() {
    use kist_format::cbor;
    use kist_format::keys;
    use kist_format::snapshot::format_key_timestamp;
    use kist_format::snapshot::Root;
    use kist_format::tree::{content_type, meta_kind, node_type, Entry, Tree};
    use serde_bytes::ByteBuf;

    let t = TestRepo::new().await;
    let base = t.dir.path().to_path_buf();
    let src = base.join("src");
    let victim = base.join("victim");
    std::fs::create_dir_all(victim.join("data")).unwrap();
    std::fs::create_dir_all(&src).unwrap();
    std::os::unix::fs::symlink(&victim, src.join("link")).unwrap();
    std::fs::write(src.join("harmless.txt"), b"kept").unwrap();

    let repo = t.open().await;
    let s = repo
        .backup(std::slice::from_ref(&src), backup_options())
        .await
        .unwrap();

    // 手工打造第二個 root：一棵只含 secret.txt 的 tree，定位在 symlink 之下。
    let hostile_tree = Tree::new(
        vec![Entry {
            name: b"secret.txt".to_vec(),
            kind: node_type::FILE,
            meta_kind: meta_kind::POSIX,
            size: 7,
            content: content_type::DIRECT,
            chunks: vec![repo.keys().chunk_id(b"escaped")],
            mode: Some(0o644),
            uid: Some(1000),
            gid: Some(1000),
            mtime_ns: Some(1_750_000_000_000_000_000),
            ..zero_entry()
        }],
        None,
    );
    let (tree_id, sealed_tree) = repo.seal_tree(hostile_tree).await.unwrap();
    repo.backend()
        .put(&keys::tree(&tree_id), sealed_tree)
        .await
        .unwrap();

    let mut snap = repo.read_snapshot_by_key(&s.snapshot_key).await.unwrap();
    snap.roots.push(Root {
        path: ByteBuf::from(
            src.join("link")
                .join("data")
                .as_os_str()
                .as_encoded_bytes()
                .to_vec(),
        ),
        tree: tree_id,
    });
    // key 的時間戳與 time_ns 是讀取端核對的同一瞬間：一起選在 1 小時後。
    let at = time::OffsetDateTime::now_utc() + time::Duration::hours(1);
    let ts = format_key_timestamp(at).unwrap();
    snap.time_ns = at.unix_timestamp_nanos() as i64; // i128 → i64：時間軸遠在範圍內
    let key_path = keys::snapshot(&backup_options().client_id, &ts);
    let sealed = repo
        .keys()
        .seal_snapshot(&key_path, &cbor::encode(&snap).unwrap())
        .unwrap();
    repo.backend().put(&key_path, sealed).await.unwrap();

    // victim 清空但保留目錄：restore 之後底下再出現的任何東西都是穿過
    // symlink 寫進去的。
    std::fs::remove_dir_all(&victim).unwrap();
    std::fs::create_dir_all(&victim).unwrap();

    let target = base.join("out");
    let outcome = repo
        .restore(&key_path, &target, RestoreOptions::default())
        .await;
    outcome.expect_err("root 定位穿過 snapshot 種的 symlink，restore 必須整體回錯");
    assert!(
        !victim.join("data").exists(),
        "restore 穿過 symlink 在目標之外建了目錄"
    );
    // 第一個 root 沒有問題：它的檔案已經還原。
    let restored = target.join(src.strip_prefix("/").unwrap_or(&src));
    assert!(restored.join("harmless.txt").is_file());
}

/// 補齊 Entry 其餘欄位的零值，讓測試只寫它在乎的欄位。
#[cfg(unix)]
fn zero_entry() -> kist_format::tree::Entry {
    use kist_format::tree::Entry;
    Entry {
        name: Vec::new(),
        kind: 0,
        meta_kind: 0,
        size: 0,
        target: Vec::new(),
        content: 0,
        chunks: Vec::new(),
        subtree: kist_format::TreeId::ZERO,
        mode: None,
        uid: None,
        gid: None,
        mtime_ns: None,
        ctime_ns: None,
        dev: None,
        inode: None,
        nlink: None,
        xattrs: None,
        etag: None,
        vern: None,
    }
}

/// 檔案路徑的 symlink-in-the-way 拒絕（防護的可行為釘死；開檔瞬間的
/// 原子拒絕——O_NOFOLLOW——無法以確定性測試重現競態本身）。
#[cfg(unix)]
#[tokio::test]
async fn symlink_in_the_way_of_a_file_is_refused() {
    let t = TestRepo::new().await;
    let src = t.dir.path().join("src");
    make_source(&src); // small.txt、link -> small.txt
    let repo = t.open().await;
    let s = repo
        .backup(std::slice::from_ref(&src), backup_options())
        .await
        .unwrap();

    let target = t.dir.path().join("out");
    let restored_root = target.join(src.strip_prefix("/").unwrap_or(&src));
    // 先放一個指向目標外的 symlink 在 small.txt 的位置：restore 不得
    // 跟隨它寫到目標之外。
    std::fs::create_dir_all(&restored_root).unwrap();
    std::os::unix::fs::symlink("/etc/hostname", restored_root.join("small.txt")).unwrap();

    let summary = repo
        .restore(&s.snapshot_key, &target, RestoreOptions::default())
        .await
        .unwrap();
    assert!(
        summary
            .errors
            .iter()
            .any(|e| e.contains("symlink is in the way")),
        "應拒絕而非跟隨：{summary:?}"
    );
}

/// 定位組件含 NUL 必須回錯（與 Go 端 restore.go 的 locatorComponents
/// 同判斷）：NUL 不是路徑元件，Unix 上會在 syscall 層 EINVAL。
#[test]
fn locator_with_nul_component_is_rejected() {
    use kist_core::fsmeta::locator_to_relative;
    assert!(
        locator_to_relative(b"/a\0b/c").is_err(),
        "含 NUL 的定位組件要回錯"
    );
    assert!(locator_to_relative(b"/a/b").is_ok());
}

/// 讀取端對 snapshot 內容也要跑結構驗證（`Snapshot::validate`：roots 非空、
/// 排序、唯一；Go 在 save 與 load 都跑）：解得開但結構不合法的 snapshot
/// 要以 Corrupt 拒絕，不能被 restore/mount 當正常資料。
#[tokio::test]
async fn snapshot_with_invalid_structure_is_rejected_on_read() {
    use kist_core::CoreError;
    use kist_format::cbor;

    let t = TestRepo::new().await;
    let src = t.dir.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("f.txt"), b"data").unwrap();
    let repo = t.open().await;
    let s = repo
        .backup(std::slice::from_ref(&src), backup_options())
        .await
        .unwrap();

    let mut snap = repo.read_snapshot_by_key(&s.snapshot_key).await.unwrap();
    snap.roots.clear(); // 解密與反序列化都會過、結構不合法
    let sealed = repo
        .keys()
        .seal_snapshot(&s.snapshot_key, &cbor::encode(&snap).unwrap())
        .unwrap();
    repo.backend().put(&s.snapshot_key, sealed).await.unwrap();

    let err = repo
        .read_snapshot_by_key(&s.snapshot_key)
        .await
        .expect_err("roots 為空的 snapshot 必須被讀取端拒絕");
    assert!(matches!(err, CoreError::Corrupt { .. }), "{err}");
}

/// 惡意 repo 可以是一條任意深的 DIR chain（每層一棵 tree、一個 entry 指向
/// 下一層；金鑰持有者寫得出，見 threat model「owns the repository」）。
/// restore 與 check 的走訪是有界堆疊上的遞迴：超過深度上限必須**乾淨回錯**
/// （restore 記進 summary.errors、check 記進 report.errors），不是把整個
/// process 墊進 stack overflow。上限 [`kist_core::MAX_TREE_DEPTH`] 與 Go
/// 實作的 `maxTreeDepth` 是同一個數，文件寡在 docs/format.md §8.4。
/// 測試本體在明確指定大小的執行緒上跑：debug build 的遞迴 frame 是 release
/// 的好幾倍大（實測 256 層深就要 >2 MiB，release 下的 2 MiB tokio worker
/// 綽綽有餘），libtest 預設執行緒的堆疊承載不了深鏈的 debug frame。
#[test]
fn overly_deep_tree_chain_is_reported_not_crashed() {
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(overly_deep_tree_chain_body())
        })
        .unwrap()
        .join()
        .unwrap();
}

async fn overly_deep_tree_chain_body() {
    use kist_core::Repository;
    use kist_format::cbor;
    use kist_format::keys;
    use kist_format::snapshot::format_key_timestamp;
    use kist_format::snapshot::Root;
    use kist_format::tree::{content_type, meta_kind, node_type, Entry, Tree};
    use kist_format::TreeId;
    use serde_bytes::ByteBuf;

    let t = TestRepo::new().await;
    let repo = t.open().await;

    // 最深處：一棵只含一個空檔的 tree；往上串 DIR entry，層數由呼叫端給。
    async fn leaf(repo: &Repository) -> TreeId {
        let entry = Entry {
            name: b"f".to_vec(),
            kind: node_type::FILE,
            meta_kind: meta_kind::GENERIC,
            content: content_type::DIRECT,
            ..plain_entry()
        };
        let (id, sealed) = repo.seal_tree(Tree::new(vec![entry], None)).await.unwrap();
        repo.backend().put(&keys::tree(&id), sealed).await.unwrap();
        id
    }
    async fn chain(repo: &Repository, depth: usize) -> TreeId {
        let mut child = leaf(repo).await;
        for _ in 0..depth {
            let entry = Entry {
                name: b"d".to_vec(),
                kind: node_type::DIR,
                meta_kind: meta_kind::GENERIC,
                subtree: child,
                ..plain_entry()
            };
            let (id, sealed) = repo.seal_tree(Tree::new(vec![entry], None)).await.unwrap();
            repo.backend().put(&keys::tree(&id), sealed).await.unwrap();
            child = id;
        }
        child
    }
    async fn write_snapshot(
        t: &common::TestRepo,
        repo: &Repository,
        root: TreeId,
        path: &[u8],
    ) -> String {
        let mut snap = {
            // 任何一個真 snapshot 都好：借它的形狀，換 root 與時間。
            let src = t.dir.path().join("src");
            std::fs::create_dir_all(&src).unwrap();
            let s = repo
                .backup(std::slice::from_ref(&src), backup_options())
                .await
                .unwrap();
            let mut snap = repo.read_snapshot_by_key(&s.snapshot_key).await.unwrap();
            snap.roots.clear();
            snap
        };
        snap.roots.push(Root {
            path: ByteBuf::from(path.to_vec()),
            tree: root,
        });
        let at = time::OffsetDateTime::now_utc() + time::Duration::hours(1);
        let ts = format_key_timestamp(at).unwrap();
        snap.time_ns = at.unix_timestamp_nanos() as i64; // i128 → i64：時間軸遠在範圍內
        let key_path = keys::snapshot(&backup_options().client_id, &ts);
        let sealed = repo
            .keys()
            .seal_snapshot(&key_path, &cbor::encode(&snap).unwrap())
            .unwrap();
        repo.backend().put(&key_path, sealed).await.unwrap();
        key_path
    }

    // MAX_TREE_DEPTH - 1 層是承諾可以走的深度：check 不可以有任何錯。
    let ok_root = chain(&repo, kist_core::MAX_TREE_DEPTH - 1).await;
    let _ok_key = write_snapshot(&t, &repo, ok_root, b"/ok").await;
    let ok_report = repo
        .check(kist_core::CheckOptions {
            read_data: false,
            repair: false,
        })
        .await
        .unwrap();
    assert!(
        ok_report.errors.is_empty(),
        "上限前一層是誠實深度，不該有錯：{:?}",
        ok_report.errors
    );

    // 超過上限的 chain：check／prune 的走訪（沒有路徑長度自然封頂）必須乾淨回報。
    let hostile_root = chain(&repo, 4_200).await;
    let key_path = write_snapshot(&t, &repo, hostile_root, b"/deep").await;

    let msg = format!("nesting deeper than {}", kist_core::MAX_TREE_DEPTH);
    let report = repo
        .check(kist_core::CheckOptions {
            read_data: false,
            repair: false,
        })
        .await
        .unwrap();
    assert!(
        report.errors.iter().any(|e| e.contains(&msg)),
        "check 要回報深度上限，而不是別的錯／不是 crash：{:?}",
        report.errors
    );

    // restore 同樣：記錄錯誤並完成，不是整個 process 陣亡。
    let target = t.dir.path().join("out");
    let summary = repo
        .restore(&key_path, &target, RestoreOptions::default())
        .await
        .unwrap();
    assert!(
        summary.errors.iter().any(|e| e.contains(&msg)),
        "restore 要在上限處回錯：{:?}",
        summary.errors.first()
    );
}

/// 補齊 Entry 其餘欄位的零值（跨平台版：不綁 symlink 測試的 unix cfg）。
fn plain_entry() -> kist_format::tree::Entry {
    use kist_format::tree::Entry;
    Entry {
        name: Vec::new(),
        kind: 0,
        meta_kind: 0,
        size: 0,
        target: Vec::new(),
        content: 0,
        chunks: Vec::new(),
        subtree: kist_format::TreeId::ZERO,
        mode: None,
        uid: None,
        gid: None,
        mtime_ns: None,
        ctime_ns: None,
        dev: None,
        inode: None,
        nlink: None,
        xattrs: None,
        etag: None,
        vern: None,
    }
}
