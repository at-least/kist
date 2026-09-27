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

/// ADR 019 A30：目標處本機的擋路物（使用者自己放的 symlink、目錄）不是 repo
/// 損壞：錯誤不能說「object … is corrupt」，要點名擋路的本機路徑。控制流程
/// 不變：檔案、子目錄、symlink 各記一筆節點錯誤，其餘照常還原。
#[cfg(unix)]
#[tokio::test]
async fn local_obstacles_in_the_tree_are_not_reported_as_corrupt() {
    let t = TestRepo::new().await;
    let src = t.dir.path().join("src");
    make_source(&src); // small.txt、sub/、link -> small.txt
    let repo = t.open().await;
    let s = repo
        .backup(std::slice::from_ref(&src), backup_options())
        .await
        .unwrap();

    let outside = t.dir.path().join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    let target = t.dir.path().join("out");
    let restored = target.join(src.strip_prefix("/").unwrap_or(&src));
    std::fs::create_dir_all(&restored).unwrap();
    let file_link = restored.join("small.txt");
    let dir_link = restored.join("sub");
    let dir_on_symlink = restored.join("link");
    std::os::unix::fs::symlink(outside.join("f"), &file_link).unwrap();
    std::os::unix::fs::symlink(&outside, &dir_link).unwrap();
    std::fs::create_dir(&dir_on_symlink).unwrap();

    let summary = repo
        .restore(&s.snapshot_key, &target, RestoreOptions::default())
        .await
        .unwrap();
    assert_eq!(summary.errors.len(), 3, "{summary:?}");
    for (path, what) in [
        (&file_link, "a symlink is in the way of a restored file"),
        (&dir_link, "a symlink is in the way of a restored directory"),
        (&dir_on_symlink, "a directory is in the way of a symlink"),
    ] {
        let line = summary
            .errors
            .iter()
            .find(|e| e.contains(what))
            .unwrap_or_else(|| panic!("少了「{what}」：{summary:?}"));
        assert!(!line.contains("corrupt"), "本機擋路被報成損壞：{line}");
        assert!(
            line.contains(&path.display().to_string()),
            "要點名擋路的路徑：{line}"
        );
    }
    // 其餘照常還原，擋路的 symlink 沒被跟隨。
    assert_eq!(
        std::fs::read(restored.join("empty.txt")).unwrap(),
        Vec::<u8>::new()
    );
    assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 0);
}

/// ADR 019 A30：root 定位上的某一層被使用者的 symlink 佔住：整個 restore 回錯
/// （與以前相同），錯誤是本機擋路、點名那個路徑，不說 repo 物件損壞。
#[cfg(unix)]
#[tokio::test]
async fn local_obstacle_on_the_root_path_is_not_reported_as_corrupt() {
    let t = TestRepo::new().await;
    let src = t.dir.path().join("src");
    make_source(&src);
    let repo = t.open().await;
    let s = repo
        .backup(std::slice::from_ref(&src), backup_options())
        .await
        .unwrap();

    let outside = t.dir.path().join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    let target = t.dir.path().join("out");
    let root_path = target.join(src.strip_prefix("/").unwrap_or(&src));
    std::fs::create_dir_all(root_path.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&outside, &root_path).unwrap();

    let err = repo
        .restore(&s.snapshot_key, &target, RestoreOptions::default())
        .await
        .expect_err("root 定位被 symlink 擋住，restore 整體回錯");
    let msg = err.to_string();
    assert!(!msg.contains("corrupt"), "本機擋路被報成損壞：{msg}");
    assert!(
        msg.contains(&root_path.display().to_string()),
        "要點名擋路的路徑：{msg}"
    );
    assert!(format!("{err:?}").starts_with("RestoreBlocked"), "{err:?}");
    assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 0);
}

/// 定位組件含 NUL 必須回錯：NUL 不是路徑元件，
/// Unix 上會在 syscall 層 EINVAL。
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
/// 排序、唯一；save 與 load 都跑）：解得開但結構不合法的 snapshot
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
/// process 墊進 stack overflow。上限 [`kist_core::MAX_TREE_DEPTH`] 文件寡在
/// docs/format.md §8.4。
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

/// 間接內容的 chunk 清單同樣要比 `v`（format.md §16：≠3 拒絕；ADR 019 A10）。
/// 泛型 `cbor::decode` 不看版本：v=4 的清單以前會被 restore 照 v3 的讀法
/// 還原出內容、check 也看不出異狀。restore 要記節點錯誤、不寫檔；check
/// （不讀資料也會走 reach 解開清單）要回報。
#[tokio::test]
async fn chunk_list_with_unknown_version_is_rejected() {
    use kist_core::CheckOptions;
    use kist_format::cbor;
    use kist_format::keys;
    use kist_format::snapshot::{format_key_timestamp, Root};
    use kist_format::tree::{content_type, meta_kind, node_type, ChunkList, Entry, Tree};
    use serde_bytes::ByteBuf;

    let t = TestRepo::new().await;
    let repo = t.open().await;
    // 兩個小檔各自成一顆 chunk：一顆是資料，一顆的明文正好是 v=4 的清單。
    let payload = b"payload listed by a v4 chunk list".to_vec();
    let list = cbor::encode(&ChunkList {
        version: 4,
        chunks: vec![repo.keys().chunk_id(&payload)],
    })
    .unwrap();
    let src = t.dir.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("payload.bin"), &payload).unwrap();
    std::fs::write(src.join("list.cbor"), &list).unwrap();
    let s = repo
        .backup(std::slice::from_ref(&src), backup_options())
        .await
        .unwrap();

    // 手工組一棵 tree：一個間接內容的檔案，chunks 指向那顆清單 chunk。
    let tree = Tree::new(
        vec![Entry {
            name: b"file.bin".to_vec(),
            kind: node_type::FILE,
            meta_kind: meta_kind::GENERIC,
            size: payload.len() as u64,
            content: content_type::INDIRECT,
            chunks: vec![repo.keys().chunk_id(&list)],
            ..plain_entry()
        }],
        None,
    );
    let (tree_id, sealed_tree) = repo.seal_tree(tree).await.unwrap();
    repo.backend()
        .put(&keys::tree(&tree_id), sealed_tree)
        .await
        .unwrap();

    // 借真 snapshot 的形狀，換 root 與時間（key 的時間戳與 time_ns 讀取端核對）。
    let mut snap = repo.read_snapshot_by_key(&s.snapshot_key).await.unwrap();
    snap.roots = vec![Root {
        path: ByteBuf::from(b"/evil".to_vec()),
        tree: tree_id,
    }];
    let at = time::OffsetDateTime::now_utc() + time::Duration::hours(1);
    let ts = format_key_timestamp(at).unwrap();
    snap.time_ns = at.unix_timestamp_nanos() as i64; // i128 → i64：時間軸遠在範圍內
    let key_path = keys::snapshot(&backup_options().client_id, &ts);
    let sealed = repo
        .keys()
        .seal_snapshot(&key_path, &cbor::encode(&snap).unwrap())
        .unwrap();
    repo.backend().put(&key_path, sealed).await.unwrap();

    // check 走 reach → resolve_chunks：要指出清單的版本不對。
    let report = repo
        .check(CheckOptions {
            read_data: false,
            repair: false,
        })
        .await
        .unwrap();
    assert!(
        report
            .errors
            .iter()
            .any(|e| e.contains("chunk list") && e.contains("version 4")),
        "check 要回報 v=4 的 chunk 清單：{:?}",
        report.errors
    );

    // restore：這個檔記節點錯誤，不照 v3 的讀法寫出內容。
    let target = t.dir.path().join("out");
    let summary = repo
        .restore(&key_path, &target, RestoreOptions::default())
        .await
        .unwrap();
    assert!(
        summary.errors.iter().any(|e| e.contains("version 4")),
        "restore 要拒絕 v=4 的 chunk 清單：{:?}",
        summary.errors
    );
    assert!(
        !target.join("evil").join("file.bin").exists(),
        "v=4 的清單不能被還原成檔案"
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

/// ADR 019 A2：restore 先寫同目錄的隱藏暫存檔（`.kist-restore-*`），成功才
/// rename 成正式名；失敗時暫存檔要被刪掉，不能留在目錄裡。
fn assert_no_restore_temp(dir: &std::path::Path) {
    let left: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with(".kist-restore-"))
        .collect();
    assert!(left.is_empty(), "暫存檔沒清掉：{left:?}");
}

/// 借真 snapshot `like` 的形狀，換成單一 root（`root_path` → 由 `entries` 組成
/// 的手工 tree）另存一份，回傳新 snapshot 的 key。時間選在 1 小時後（key 的
/// 時間戳與 time_ns 讀取端會核對），不與真 snapshot 撞 key。
async fn hand_snapshot(
    repo: &kist_core::Repository,
    like: &str,
    root_path: &[u8],
    entries: Vec<kist_format::tree::Entry>,
) -> String {
    use kist_format::cbor;
    use kist_format::keys;
    use kist_format::snapshot::{format_key_timestamp, Root};
    use kist_format::tree::Tree;
    use serde_bytes::ByteBuf;

    let (tree_id, sealed_tree) = repo.seal_tree(Tree::new(entries, None)).await.unwrap();
    repo.backend()
        .put(&keys::tree(&tree_id), sealed_tree)
        .await
        .unwrap();
    let mut snap = repo.read_snapshot_by_key(like).await.unwrap();
    snap.roots = vec![Root {
        path: ByteBuf::from(root_path.to_vec()),
        tree: tree_id,
    }];
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

/// ADR 019 A2：間接內容的 chunk 清單讀不到（pack 全刪）時，還原在開檔之前就
/// 失敗——目標處使用者原有的檔從頭到尾沒被碰過，不能因為「清掉寫到一半的
/// 檔」被刪掉。
#[tokio::test]
async fn failed_indirect_restore_keeps_the_existing_file() {
    use kist_format::tree::content_type;

    let t = TestRepo::new().await;
    let src = t.dir.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    // 超過 MAX_INLINE_CHUNKS（256）個 chunk 才是間接內容：測試 chunker 平均
    // 16 KiB，8 MiB 約 500 個。
    std::fs::write(src.join("big.bin"), random_bytes(3, 8 << 20)).unwrap();
    let repo = t.open().await;
    let s = repo
        .backup(std::slice::from_ref(&src), backup_options())
        .await
        .unwrap();
    let snap = repo.read_snapshot_by_key(&s.snapshot_key).await.unwrap();
    let entries = repo.read_tree_chain(&snap.roots[0].tree).await.unwrap();
    assert_eq!(
        entries[0].content,
        content_type::INDIRECT,
        "前提：big.bin 是間接內容"
    );
    for pack in walk_files(&t.repo_path().join("packs")) {
        if pack.is_file() {
            std::fs::remove_file(&pack).unwrap();
        }
    }

    let target = t.dir.path().join("out");
    let restored = target.join(src.strip_prefix("/").unwrap_or(&src));
    std::fs::create_dir_all(&restored).unwrap();
    std::fs::write(restored.join("big.bin"), b"PRECIOUS user data").unwrap();

    let summary = repo
        .restore(&s.snapshot_key, &target, RestoreOptions::default())
        .await
        .unwrap();
    assert_eq!(summary.errors.len(), 1, "{summary:?}");
    assert_eq!(
        std::fs::read(restored.join("big.bin")).ok().as_deref(),
        Some(&b"PRECIOUS user data"[..]),
        "還原失敗不能動到使用者原有的檔；{summary:?}"
    );
    assert_no_restore_temp(&restored);
}

/// ADR 019 A2：直接內容寫到一半才發現缺 chunk——正式檔名底下必須仍是原本
/// 的內容，不能是寫了一半的檔，也不能被刪掉。
#[tokio::test]
async fn failed_direct_restore_keeps_the_existing_file() {
    use kist_format::tree::{content_type, meta_kind, node_type, Entry};

    let t = TestRepo::new().await;
    let repo = t.open().await;
    // 一顆真的 chunk（先寫得進暫存檔）＋一顆 repo 裡沒有的。
    let payload = b"first chunk exists in the repo".to_vec();
    let src = t.dir.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("payload.bin"), &payload).unwrap();
    let s = repo
        .backup(std::slice::from_ref(&src), backup_options())
        .await
        .unwrap();
    let key = hand_snapshot(
        &repo,
        &s.snapshot_key,
        b"/hand",
        vec![Entry {
            name: b"file.bin".to_vec(),
            kind: node_type::FILE,
            meta_kind: meta_kind::GENERIC,
            size: payload.len() as u64 + 7,
            content: content_type::DIRECT,
            chunks: vec![
                repo.keys().chunk_id(&payload),
                repo.keys().chunk_id(b"missing"),
            ],
            ..plain_entry()
        }],
    )
    .await;

    let target = t.dir.path().join("out");
    let dir = target.join("hand");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("file.bin"), b"PRECIOUS user data").unwrap();

    let summary = repo
        .restore(&key, &target, RestoreOptions::default())
        .await
        .unwrap();
    assert_eq!(summary.errors.len(), 1, "{summary:?}");
    assert_eq!(
        std::fs::read(dir.join("file.bin")).ok().as_deref(),
        Some(&b"PRECIOUS user data"[..]),
        "缺 chunk 的還原不能毀掉原本的內容；{summary:?}"
    );
    assert_no_restore_temp(&dir);
}

/// ADR 019 A2 裁定：正式檔名處擋著 symlink 維持「拒絕」——回報節點錯誤，
/// symlink 本身留著、不跟隨，它指向的檔內容不變。
#[cfg(unix)]
#[tokio::test]
async fn refused_symlink_in_the_way_is_left_alone() {
    let t = TestRepo::new().await;
    let src = t.dir.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("small.txt"), b"hello kist\n").unwrap();
    let repo = t.open().await;
    let s = repo
        .backup(std::slice::from_ref(&src), backup_options())
        .await
        .unwrap();

    let outside = t.dir.path().join("outside.txt");
    std::fs::write(&outside, b"outside content").unwrap();
    let target = t.dir.path().join("out");
    let restored = target.join(src.strip_prefix("/").unwrap_or(&src));
    std::fs::create_dir_all(&restored).unwrap();
    let link = restored.join("small.txt");
    std::os::unix::fs::symlink(&outside, &link).unwrap();

    let summary = repo
        .restore(&s.snapshot_key, &target, RestoreOptions::default())
        .await
        .unwrap();
    assert_eq!(summary.errors.len(), 1, "{summary:?}");
    assert!(
        summary.errors[0].contains("symlink is in the way"),
        "{summary:?}"
    );
    let still_link = std::fs::symlink_metadata(&link)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false);
    assert!(still_link, "擋路的 symlink 被刪掉或換掉了；{summary:?}");
    assert_eq!(std::fs::read_link(&link).unwrap(), outside);
    assert_eq!(std::fs::read(&outside).unwrap(), b"outside content");
    assert_no_restore_temp(&restored);
}

/// ADR 019 A2：同一個 snapshot 還原兩次到同一個目標，snapshot 裡有唯讀檔
/// （0o444）與唯讀目錄（0o555，裡面也是 0o444 的檔）。第一次還原後它們就是
/// 唯讀的；第二次必須照樣成功、什麼都不刪，最後的 mode 等於記錄的值。
#[cfg(unix)]
#[tokio::test]
async fn restore_twice_over_read_only_files_and_dirs() {
    use std::os::unix::fs::PermissionsExt;
    let mode_of = |p: &std::path::Path| {
        std::fs::symlink_metadata(p)
            .map(|m| format!("{:o}", m.permissions().mode() & 0o7777))
            .unwrap_or_else(|e| format!("<{e}>"))
    };
    let chmod = |p: &std::path::Path, mode: u32| {
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode)).unwrap();
    };

    let t = TestRepo::new().await;
    let src = t.dir.path().join("src");
    std::fs::create_dir_all(src.join("rodir")).unwrap();
    std::fs::write(src.join("ro.txt"), b"read-only file").unwrap();
    std::fs::write(src.join("rodir").join("inner.txt"), b"inside read-only dir").unwrap();
    chmod(&src.join("ro.txt"), 0o444);
    chmod(&src.join("rodir").join("inner.txt"), 0o444);
    chmod(&src.join("rodir"), 0o555);
    let repo = t.open().await;
    let s = repo
        .backup(std::slice::from_ref(&src), backup_options())
        .await
        .unwrap();

    let target = t.dir.path().join("out");
    let restored = target.join(src.strip_prefix("/").unwrap_or(&src));
    let first = repo
        .restore(&s.snapshot_key, &target, RestoreOptions::default())
        .await
        .unwrap();
    let second = repo
        .restore(&s.snapshot_key, &target, RestoreOptions::default())
        .await
        .unwrap();

    // 先把觀察值收齊、把目錄改回可寫，再斷言：斷言失敗時 TempDir 才刪得掉
    // 0o555 目錄裡的檔（否則靜靜留在 /tmp）。
    let modes = [
        mode_of(&restored.join("ro.txt")),
        mode_of(&restored.join("rodir")),
        mode_of(&restored.join("rodir").join("inner.txt")),
    ];
    let contents = [
        std::fs::read(restored.join("ro.txt")).ok(),
        std::fs::read(restored.join("rodir").join("inner.txt")).ok(),
    ];
    chmod(&src.join("rodir"), 0o755);
    if restored.join("rodir").is_dir() {
        chmod(&restored.join("rodir"), 0o755);
    }

    assert_eq!(
        modes,
        ["444", "555", "444"],
        "(ro.txt, rodir, inner.txt)；第二次：{second:?}"
    );
    assert!(first.errors.is_empty(), "第一次：{first:?}");
    assert!(second.errors.is_empty(), "第二次：{second:?}");
    assert_eq!(
        contents,
        [
            Some(b"read-only file".to_vec()),
            Some(b"inside read-only dir".to_vec()),
        ]
    );
    assert_no_restore_temp(&restored);
    assert_no_restore_temp(&restored.join("rodir"));
}

/// 同一個 snapshot 還原兩次，硬連結關係要還在（ADR 019 A2）。第二次時正式名
/// 都已存在：第一個名字以暫存檔＋rename 換成新的 inode，後面的名字 link(2)
/// 回 EEXIST——改走複製的話兩個名字就各自獨立了；要以暫存名 link 再 rename。
#[cfg(unix)]
#[tokio::test]
async fn restore_twice_keeps_hard_links() {
    use std::os::unix::fs::MetadataExt;
    let t = TestRepo::new().await;
    let src = t.dir.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("a.bin"), b"shared").unwrap();
    std::fs::hard_link(src.join("a.bin"), src.join("b.bin")).unwrap();
    let repo = t.open().await;
    let s = repo
        .backup(std::slice::from_ref(&src), backup_options())
        .await
        .unwrap();
    let target = t.dir.path().join("out");
    let restored = target.join(src.strip_prefix("/").unwrap_or(&src));
    for round in 1..=2 {
        let summary = repo
            .restore(&s.snapshot_key, &target, RestoreOptions::default())
            .await
            .unwrap();
        let a = std::fs::symlink_metadata(restored.join("a.bin")).unwrap();
        let b = std::fs::symlink_metadata(restored.join("b.bin")).unwrap();
        assert!(summary.errors.is_empty(), "第 {round} 次：{summary:?}");
        assert_eq!(
            (a.ino(), a.nlink()),
            (b.ino(), 2),
            "第 {round} 次：a.bin 與 b.bin 要是同一個 inode、nlink 2"
        );
        assert_eq!(std::fs::read(restored.join("b.bin")).unwrap(), b"shared");
        assert_no_restore_temp(&restored);
    }
}

/// ADR 019 A4：restore 途中，本機攻擊者（同 uid）反覆把目標之下的中間目錄
/// 換成指向目標之外的 symlink。以前逐段 lstat 檢查完就以完整路徑開檔，換上
/// symlink 之後的檔都經它寫到外面（ADR 實測：2000 個檔有 1999 個）。以目錄
/// handle 為錨逐層開檔之後，寫入一律落在已開的那個真目錄——它被搬到哪都
/// 一樣——外面永遠是空的。
///
/// 換的那一方等 `sub/` 裡出現第一個項目（restore 已經在寫它的子項目）才開始，
/// 之後反覆「真目錄搬到旁邊（仍在目標之內）、原位換上外指 symlink、稍等、
/// 換回來」。每一輪結束時真目錄都回到原位，最後的斷言與 TempDir 清理都是
/// 確定的。
#[cfg(unix)]
#[tokio::test]
async fn swapping_an_intermediate_dir_for_a_symlink_never_writes_outside() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    const FILES: usize = 2000;
    let t = TestRepo::new().await;
    let src = t.dir.path().join("src");
    std::fs::create_dir_all(src.join("sub")).unwrap();
    for i in 0..FILES {
        std::fs::write(
            src.join("sub").join(format!("f{i:04}")),
            format!("file {i}"),
        )
        .unwrap();
    }
    let repo = t.open().await;
    let s = repo
        .backup(std::slice::from_ref(&src), backup_options())
        .await
        .unwrap();

    // 「外面」：同一個 tempdir 裡、目標之外的目錄。
    let outside = t.dir.path().join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    let target = t.dir.path().join("out");
    let restored = target.join(src.strip_prefix("/").unwrap_or(&src));
    let victim = restored.join("sub");
    let moved = restored.join("sub.moved");

    let stop = Arc::new(AtomicBool::new(false));
    let swapper = {
        let stop = Arc::clone(&stop);
        let (victim, moved, outside) = (victim.clone(), moved.clone(), outside.clone());
        std::thread::spawn(move || {
            let started = |p: &std::path::Path| {
                std::fs::read_dir(p).is_ok_and(|mut entries| entries.next().is_some())
            };
            while !stop.load(Ordering::Relaxed) && !started(&victim) {
                std::thread::yield_now();
            }
            let mut swaps = 0u32;
            while !stop.load(Ordering::Relaxed) {
                std::fs::rename(&victim, &moved).unwrap();
                std::os::unix::fs::symlink(&outside, &victim).unwrap();
                swaps += 1;
                std::thread::sleep(std::time::Duration::from_micros(200));
                std::fs::remove_file(&victim).unwrap();
                std::fs::rename(&moved, &victim).unwrap();
                std::thread::sleep(std::time::Duration::from_micros(200));
            }
            swaps
        })
    };

    let summary = repo
        .restore(&s.snapshot_key, &target, RestoreOptions::default())
        .await
        .unwrap();
    stop.store(true, Ordering::Relaxed);
    let swaps = swapper.join().unwrap();

    let leaked: Vec<String> = std::fs::read_dir(&outside)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    let placed = walk_files(&restored)
        .into_iter()
        .filter(|p| std::fs::symlink_metadata(p).unwrap().is_file())
        .count();
    assert!(swaps >= 1, "restore 在換第一次之前就結束了，這次沒測到競態");
    assert!(
        leaked.is_empty(),
        "換了 {swaps} 次；{} 個檔寫到目標之外，例如 {:?}；summary：files {} errors {}",
        leaked.len(),
        &leaked[..leaked.len().min(3)],
        summary.files,
        summary.errors.len()
    );
    assert_eq!(
        placed as u64, summary.files,
        "算成還原成功的檔，都要真的在目標之內"
    );
    assert!(summary.errors.is_empty(), "換了 {swaps} 次：{summary:?}");
    assert_eq!(summary.files, FILES as u64);
}

/// ADR 019 A43：非 root 還原維持現狀——不 chown（還原出的東西屬於還原者本人，
/// 記錄的擁有者是別人也一樣、也不因此回錯），記錄的 setuid／setgid 照套
/// （setuid 指向自己不構成提權）。snapshot 裡的擁有者換成別人，另加一個有
/// mode、沒有 uid／gid 的 sftp 條目。以 root 跑就略過：root 會真的 chown，
/// 那部分這裡驗不了（UNVERIFIED，沒有 root）。
#[cfg(unix)]
#[tokio::test]
async fn non_root_restore_keeps_setid_bits_and_does_not_chown() {
    use kist_format::tree::{meta_kind, node_type};
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let euid = rustix::process::geteuid().as_raw();
    if euid == 0 {
        eprintln!("skipping: running as root");
        return;
    }
    let other = if euid == 4242 { 4243 } else { 4242 };

    let t = TestRepo::new().await;
    let src = t.dir.path().join("src");
    std::fs::create_dir_all(src.join("sgid")).unwrap();
    std::fs::write(src.join("suid.bin"), b"payload").unwrap();
    std::fs::write(src.join("sgid").join("inner.txt"), b"inner").unwrap();
    std::os::unix::fs::symlink("suid.bin", src.join("link")).unwrap();
    std::fs::set_permissions(src.join("suid.bin"), PermissionsExt::from_mode(0o4755)).unwrap();
    std::fs::set_permissions(src.join("sgid"), PermissionsExt::from_mode(0o2755)).unwrap();
    let repo = t.open().await;
    let s = repo
        .backup(std::slice::from_ref(&src), backup_options())
        .await
        .unwrap();

    // 同一份內容，擁有者全換成別人（sgid/ 裡的 inner.txt 在原本的子 tree，不動）。
    let snap = repo.read_snapshot_by_key(&s.snapshot_key).await.unwrap();
    let mut entries = repo.read_tree_chain(&snap.roots[0].tree).await.unwrap();
    for e in &mut entries {
        assert!(
            e.uid.is_some() && e.gid.is_some(),
            "前提：posix 條目記錄擁有者"
        );
        e.uid = Some(other);
        e.gid = Some(other);
    }
    let mut sftp = plain_entry();
    sftp.name = b"sftp.bin".to_vec();
    sftp.kind = node_type::FILE;
    sftp.meta_kind = meta_kind::SFTP;
    sftp.mode = Some(0o104755);
    sftp.mtime_ns = Some(1_000_000_000_000_000_000);
    entries.push(sftp);
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    let key = hand_snapshot(&repo, &s.snapshot_key, b"/foreign", entries).await;

    let target = t.dir.path().join("out");
    let summary = repo
        .restore(&key, &target, RestoreOptions::default())
        .await
        .unwrap();
    let restored = target.join("foreign");
    // restore 自己建的那層目錄：還原者新建的東西都該是這個 gid。
    let own_gid = std::fs::metadata(&restored).unwrap().gid();
    let stat = |name: &str| {
        let m = std::fs::symlink_metadata(restored.join(name)).unwrap();
        (format!("{:o}", m.mode() & 0o7777), m.uid(), m.gid())
    };

    assert!(summary.errors.is_empty(), "{summary:?}");
    assert_eq!((summary.files, summary.symlinks), (3, 1), "{summary:?}");
    assert_eq!(stat("suid.bin"), ("4755".to_owned(), euid, own_gid));
    assert_eq!(stat("sgid"), ("2755".to_owned(), euid, own_gid));
    assert_eq!(stat("sftp.bin"), ("4755".to_owned(), euid, own_gid));
    let link = std::fs::symlink_metadata(restored.join("link")).unwrap();
    assert!(link.file_type().is_symlink());
    assert_eq!((link.uid(), link.gid()), (euid, own_gid));
    assert_eq!(
        std::fs::read(restored.join("sgid").join("inner.txt")).unwrap(),
        b"inner"
    );
}
