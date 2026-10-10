//! v4 的 restore：backup → restore 的往返、§9 依 `Root.t` 的落點、§7 的 chunk 索引，以及從 v3 的
//! `restore_hardening.rs` 搬過來的走訪層強化（重複還原、擋路的東西、壞掉的 chunk、過深的目錄、唯讀目錄、
//! 硬連結、metadata 的順序；ADR 019）。檔案系統那一層的單元行為由 v3 的測試覆蓋（共用 `crate::restore`）。

#[path = "v4_common/mod.rs"]
mod v4_common;

use kist_core::v4::Repo;
use kist_core::{CoreError, RestoreOptions};
use kist_format::v4::ids::{ChunkId, ItemId, MetaId};
use kist_format::v4::keys::Key;
use kist_format::v4::meta::{
    content_type, meta_kind, node_type, ChunkIndex, ChunkList, ChunkPart, Entry, Item, Seg,
};
use kist_format::v4::snapshot::Root;
use v4_common::*;

/// 一個 item 的 meta ID（與 backup 寫的同一個算法）。
fn meta_id(repo: &Repo, item: &Item) -> MetaId {
    repo.keys().meta_id(&item.encode().unwrap())
}

/// 目錄 root：`entries`（排好）組成一段，放在定位 `path`。
fn dir_root(repo: &Repo, path: &[u8], mut entries: Vec<Entry>) -> (Root, Item) {
    entries.sort_by(|a, b| a.n.cmp(&b.n));
    let seg = Item::Seg(Seg::new(entries));
    let root = Root {
        path: path.to_vec(),
        t: node_type::DIR,
        tree: Some(meta_id(repo, &seg)),
        entry: None,
    };
    (root, seg)
}

/// 手工的單一目錄 root snapshot（client 0x11，時間在一小時後）。
async fn hand_dir(repo: &Repo, path: &[u8], entries: Vec<Entry>) -> Key {
    let (root, seg) = dir_root(repo, path, entries);
    hand_snapshot(repo, client(0x11), hours_from_now(1), vec![root], vec![seg]).await
}

/// 一個檔案 Entry：內容是 `chunks`（直接內容），generic metadata。
fn file(n: &[u8], chunks: &[ChunkId], size: u64) -> Entry {
    let mut e = entry(n, node_type::FILE, meta_kind::GENERIC);
    e.size = size;
    e.chunks = chunks.iter().map(|c| ItemId::from(*c)).collect();
    e
}

/// snapshot 的一個 root（只有一個 root 的 snapshot）的目錄內容。
async fn root_entries(repo: &Repo, snapshot: &str) -> Vec<Entry> {
    let reader = repo.reader(None).await.unwrap();
    let snap = repo.read_snapshot(&skey(snapshot)).await.unwrap();
    let roots = reader.roots(&snap.roots).await.unwrap();
    reader.entries(&roots[0].tree.unwrap()).await.unwrap()
}

/// 每個小於最小 chunk 的檔案是一個 chunk：備份它們，回傳 (內容, chunk ID)。
async fn small_chunks(t: &TestRepo, repo: &Repo, n: usize) -> Vec<(Vec<u8>, ChunkId)> {
    let src = t.dir.path().join("chunks");
    std::fs::create_dir_all(&src).unwrap();
    let mut out = Vec::new();
    for i in 0..n {
        let content = data(&format!("chunk {i}"), 1000 + i);
        std::fs::write(src.join(format!("c{i}")), &content).unwrap();
        let id = repo.keys().chunk_id(&content);
        out.push((content, id));
    }
    repo.backup(&[src], backup_options()).await.unwrap();
    out
}

/// ADR 019 A2：restore 先寫同目錄的隱藏暫存檔，成功才 rename；失敗時暫存檔要被刪掉。
fn assert_no_restore_temp(dir: &std::path::Path) {
    let left: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with(".kist-restore-"))
        .collect();
    assert!(left.is_empty(), "暫存檔沒清掉：{left:?}");
}

// --- 往返 ---

#[tokio::test]
async fn backup_then_restore_is_byte_identical() {
    let t = TestRepo::new().await;
    let src = t.dir.path().join("src");
    make_source(&src);
    let repo = t.open().await;
    let s = repo
        .backup(std::slice::from_ref(&src), backup_options())
        .await
        .unwrap();
    assert!(s.report.packs_new >= 2, "{:?}", s.report);
    let target = t.dir.path().join("out");
    let summary = repo
        .restore(&skey(&s.snapshot), &target, RestoreOptions::default())
        .await
        .unwrap();
    assert!(summary.errors.is_empty(), "{summary:?}");
    assert_eq!(summary.files, s.stats.files);
    assert_same_tree(&src, &landed(&target, &src));
}

/// 超過 256 個 chunk 的檔案以 ChunkList 記錄（§8 的 `ct=1`）。
#[tokio::test]
async fn large_file_uses_indirect_content_and_restores() {
    let t = TestRepo::new().await;
    let src = t.dir.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("big.bin"), random_bytes(7, 8 * 1024 * 1024)).unwrap();
    let repo = t.open().await;
    let s = repo
        .backup(std::slice::from_ref(&src), backup_options())
        .await
        .unwrap();
    assert!(s.report.chunks_new > 256, "{:?}", s.report);
    let entries = root_entries(&repo, &s.snapshot).await;
    assert_eq!(entries[0].ct, content_type::INDIRECT);
    let target = t.dir.path().join("out");
    repo.restore(&skey(&s.snapshot), &target, RestoreOptions::default())
        .await
        .unwrap();
    assert_same_tree(&src, &landed(&target, &src));
}

/// §7：多段的 chunk 清單由 ChunkIndex 串起來，依序讀；每一段的 `size` 必須等於它涵蓋的 chunk 明文長度，不符是
/// 那個檔案的錯誤。真的 backup 要上萬個 chunk 才會切段，這裡手工組。
#[tokio::test]
async fn a_chunk_index_is_followed_in_order_and_its_sizes_are_checked() {
    let t = TestRepo::new().await;
    let repo = t.open().await;
    let chunks = small_chunks(&t, &repo, 5).await;
    let ids: Vec<ChunkId> = chunks.iter().map(|(_, id)| *id).collect();
    let len =
        |r: std::ops::Range<usize>| -> u64 { chunks[r].iter().map(|(c, _)| c.len() as u64).sum() };
    let a = Item::ChunkList(ChunkList::new(ids[..2].to_vec()));
    let b = Item::ChunkList(ChunkList::new(ids[2..].to_vec()));
    let index = |size_b: u64| {
        Item::ChunkIndex(ChunkIndex::new(
            1,
            vec![
                ChunkPart {
                    size: len(0..2),
                    id: meta_id(&repo, &a),
                },
                ChunkPart {
                    size: size_b,
                    id: meta_id(&repo, &b),
                },
            ],
        ))
    };
    let good = index(len(2..5));
    let bad = index(len(2..5) - 1);
    let indirect = |n: &[u8], list: &Item, size: u64| {
        let mut e = entry(n, node_type::FILE, meta_kind::GENERIC);
        e.ct = content_type::INDIRECT;
        e.chunks = vec![ItemId::from(meta_id(&repo, list))];
        e.size = size;
        e
    };
    let total = len(0..5);
    let (root, seg) = dir_root(
        &repo,
        b"/hand",
        vec![
            indirect(b"good.bin", &good, total),
            indirect(b"bad.bin", &bad, total - 1),
        ],
    );
    let key = hand_snapshot(
        &repo,
        client(0x11),
        hours_from_now(1),
        vec![root],
        vec![a, b, good, bad, seg],
    )
    .await;
    let target = t.dir.path().join("out");
    let summary = repo
        .restore(&key, &target, RestoreOptions::default())
        .await
        .unwrap();
    let expected: Vec<u8> = chunks.iter().flat_map(|(c, _)| c.clone()).collect();
    assert_eq!(
        std::fs::read(target.join("hand/good.bin")).unwrap(),
        expected
    );
    assert_eq!(summary.errors.len(), 1, "{summary:?}");
    assert!(
        summary.errors[0].contains("bad.bin") && summary.errors[0].contains("part 1 covers"),
        "{summary:?}"
    );
    assert!(!target.join("hand/bad.bin").exists());
}

/// §7：多段的目錄（Dir 加 Seg）照樣整個還原。
#[tokio::test]
async fn a_segmented_directory_restores() {
    let t = TestRepo::new().await;
    let repo = t.open().await;
    let src = t.dir.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    for n in names_with_cuts(&repo, 3) {
        std::fs::write(src.join(&n), n.as_bytes()).unwrap();
    }
    let s = repo
        .backup(std::slice::from_ref(&src), backup_options())
        .await
        .unwrap();
    let target = t.dir.path().join("out");
    let summary = repo
        .restore(&skey(&s.snapshot), &target, RestoreOptions::default())
        .await
        .unwrap();
    assert!(summary.errors.is_empty(), "{summary:?}");
    assert_same_tree(&src, &landed(&target, &src));
}

#[tokio::test]
async fn backup_of_multiple_paths_and_single_file() {
    let t = TestRepo::new().await;
    let a = t.dir.path().join("a");
    let b = t.dir.path().join("b");
    std::fs::create_dir_all(&a).unwrap();
    std::fs::create_dir_all(&b).unwrap();
    std::fs::write(a.join("x"), b"x").unwrap();
    std::fs::write(b.join("y"), b"y").unwrap();
    let file = t.dir.path().join("single.txt");
    std::fs::write(&file, b"single").unwrap();
    let repo = t.open().await;
    let s = repo
        .backup(&[a.clone(), b.clone(), file.clone()], backup_options())
        .await
        .unwrap();
    assert_eq!(s.stats.files, 3);
    let target = t.dir.path().join("out");
    repo.restore(&skey(&s.snapshot), &target, RestoreOptions::default())
        .await
        .unwrap();
    assert_same_tree(&a, &landed(&target, &a));
    assert_same_tree(&b, &landed(&target, &b));
    assert_eq!(std::fs::read(landed(&target, &file)).unwrap(), b"single");
}

#[tokio::test]
async fn restore_unknown_snapshot_fails() {
    let t = TestRepo::new().await;
    let repo = t.open().await;
    let missing = Key::Snapshot {
        client: client(1),
        ts: kist_format::v4::keys::SnapTs::from_nanos(1).unwrap(),
        replica: false,
    };
    let err = repo
        .restore(
            &missing,
            &t.dir.path().join("out"),
            RestoreOptions::default(),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, CoreError::SnapshotNotFound(_)), "{err}");
}

/// §9：落點照 `Root.t`，不從內容猜。目錄 root 的內容放在定位的所有段之下（`/` 沒有段，直接放進目標；`s3://`
/// 去掉 scheme），檔案與符號連結放在去掉最後一段的目錄、名稱是 entry 的。
#[tokio::test]
async fn roots_land_by_their_recorded_type() {
    let t = TestRepo::new().await;
    let repo = t.open().await;
    let chunks = small_chunks(&t, &repo, 3).await;
    let f = |n: &[u8], i: usize| file(n, &[chunks[i].1], chunks[i].0.len() as u64);
    let (slash, slash_seg) = dir_root(&repo, b"/", vec![f(b"top.txt", 0)]);
    let (bucket, bucket_seg) = dir_root(&repo, b"s3://bucket/prefix", vec![f(b"obj", 1)]);
    let file_root = Root {
        path: b"/x/y/f".to_vec(),
        t: node_type::FILE,
        tree: None,
        entry: Some(f(b"f", 2)),
    };
    let mut link = entry(b"l", node_type::SYMLINK, meta_kind::GENERIC);
    link.target = b"f".to_vec();
    let link_root = Root {
        path: b"/x/y/l".to_vec(),
        t: node_type::SYMLINK,
        tree: None,
        entry: Some(link),
    };
    let key = hand_snapshot(
        &repo,
        client(0x11),
        hours_from_now(1),
        vec![slash, file_root, link_root, bucket],
        vec![slash_seg, bucket_seg],
    )
    .await;
    let target = t.dir.path().join("out");
    let summary = repo
        .restore(&key, &target, RestoreOptions::default())
        .await
        .unwrap();
    assert!(summary.errors.is_empty(), "{summary:?}");
    assert_eq!((summary.files, summary.symlinks), (3, 1));
    assert_eq!(std::fs::read(target.join("top.txt")).unwrap(), chunks[0].0);
    assert_eq!(
        std::fs::read(target.join("bucket/prefix/obj")).unwrap(),
        chunks[1].0
    );
    assert_eq!(std::fs::read(target.join("x/y/f")).unwrap(), chunks[2].0);
    #[cfg(unix)]
    assert_eq!(
        std::fs::read_link(target.join("x/y/l")).unwrap(),
        std::path::Path::new("f")
    );
    let mut planned = Vec::new();
    let plan = repo
        .plan_restore(&key, &target, RestoreOptions::default(), &mut |p, _| {
            planned.push(p.strip_prefix(&target).unwrap().to_path_buf())
        })
        .await
        .unwrap();
    assert_eq!((plan.files, plan.symlinks), (3, 1));
    planned.sort();
    let want: Vec<std::path::PathBuf> = ["bucket/prefix/obj", "top.txt", "x/y/f", "x/y/l"]
        .iter()
        .map(std::path::PathBuf::from)
        .collect();
    assert_eq!(planned, want);
}

/// xattr 的往返（unix）：檔案與目錄的 user.* 原樣回來；唯讀檔的 xattr 在 chmod 之前套；符號連結的不套、不算錯。
#[cfg(unix)]
#[tokio::test]
async fn xattrs_survive_backup_restore() {
    use std::ffi::OsStr;
    use std::os::unix::fs::PermissionsExt;

    let t = TestRepo::new().await;
    let src = t.dir.path().join("src");
    std::fs::create_dir_all(src.join("sub")).unwrap();
    std::fs::write(src.join("plain.txt"), b"plain").unwrap();
    xattr::set(src.join("plain.txt"), OsStr::new("user.tag"), b"file-value").unwrap();
    xattr::set(src.join("sub"), OsStr::new("user.dir-tag"), b"dir-value").unwrap();
    std::fs::write(src.join("ro.txt"), b"read only").unwrap();
    xattr::set(src.join("ro.txt"), OsStr::new("user.locked"), b"yes").unwrap();
    std::fs::set_permissions(src.join("ro.txt"), std::fs::Permissions::from_mode(0o444)).unwrap();
    std::os::unix::fs::symlink("plain.txt", src.join("link")).unwrap();
    let repo = t.open().await;
    let s = repo
        .backup(std::slice::from_ref(&src), backup_options())
        .await
        .unwrap();
    let target = t.dir.path().join("out");
    let restored = repo
        .restore(&skey(&s.snapshot), &target, RestoreOptions::default())
        .await
        .unwrap();
    let root = landed(&target, &src);
    let get = |p: &std::path::Path, name: &str| {
        xattr::get(p, OsStr::new(name))
            .unwrap()
            .unwrap_or_else(|| panic!("xattr {name} missing on {}", p.display()))
    };
    assert_eq!(get(&root.join("plain.txt"), "user.tag"), b"file-value");
    assert_eq!(get(&root.join("sub"), "user.dir-tag"), b"dir-value");
    let ro = root.join("ro.txt");
    assert_eq!(get(&ro, "user.locked"), b"yes");
    assert_eq!(
        std::fs::metadata(&ro).unwrap().permissions().mode() & 0o777,
        0o444
    );
    assert!(restored.errors.is_empty(), "{:?}", restored.errors);
}

// --- ADR 019：走訪層的強化（從 v3 的 restore_hardening.rs 搬過來） ---

/// 第二次 restore 到同一個目錄（裡面已有 symlink）必須成功，結果仍然正確。
#[cfg(unix)]
#[tokio::test]
async fn restore_twice_into_same_target_succeeds() {
    let t = TestRepo::new().await;
    let src = t.dir.path().join("src");
    make_source(&src);
    let repo = t.open().await;
    let s = repo
        .backup(std::slice::from_ref(&src), backup_options())
        .await
        .unwrap();
    let target = t.dir.path().join("out");
    for _ in 0..2 {
        let summary = repo
            .restore(&skey(&s.snapshot), &target, RestoreOptions::default())
            .await
            .unwrap();
        assert!(summary.errors.is_empty(), "{summary:?}");
    }
    assert_same_tree(&src, &landed(&target, &src));
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
    repo.restore(&skey(&s.snapshot), &target, RestoreOptions::default())
        .await
        .unwrap();
    assert_same_tree(&a, &landed(&target, &a));
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
    // 翻掉每個資料 pack 的一個 byte（第一項的密文裡），保證有檔案受影響
    for pack in walk_files(&t.repo_path().join("packs")) {
        if pack.is_file() {
            let mut bytes = std::fs::read(&pack).unwrap();
            bytes[40] ^= 0xff;
            std::fs::write(&pack, bytes).unwrap();
        }
    }
    let target = t.dir.path().join("out");
    let summary = repo
        .restore(&skey(&s.snapshot), &target, RestoreOptions::default())
        .await
        .unwrap();
    assert!(!summary.errors.is_empty(), "{summary:?}");
    let restored = landed(&target, &src);
    assert!(restored.join("sub/deeper").is_dir());
    assert!(restored.join("empty.txt").is_file());
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

/// snapshot 可以帶一個 symlink，另一個 root 的定位穿過它。restore 不得透過那個 symlink 寫到目標之外：目錄路徑
/// 遇到 symlink 回錯，與檔案路徑的防護一致。
#[cfg(unix)]
#[tokio::test]
async fn restore_refuses_to_write_through_a_planted_symlink() {
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

    // 第一個 root 是真的那一個；手工的第二個 root 定位在 symlink 之下，裡面只有 secret.txt
    let reader = repo.reader(None).await.unwrap();
    let snap = repo.read_snapshot(&skey(&s.snapshot)).await.unwrap();
    let real = reader.roots(&snap.roots).await.unwrap().remove(0);
    let mut secret = file(b"secret.txt", &[repo.keys().chunk_id(b"escaped")], 7);
    secret.mk = meta_kind::GENERIC;
    let hostile_path = src.join("link").join("data");
    let (hostile, hostile_seg) = dir_root(
        &repo,
        hostile_path.as_os_str().as_encoded_bytes(),
        vec![secret],
    );
    let key = hand_snapshot(
        &repo,
        client(0x11),
        hours_from_now(1),
        vec![real, hostile],
        vec![hostile_seg],
    )
    .await;

    std::fs::remove_dir_all(&victim).unwrap();
    std::fs::create_dir_all(&victim).unwrap();
    let target = base.join("out");
    repo.restore(&key, &target, RestoreOptions::default())
        .await
        .expect_err("root 定位穿過 snapshot 種的 symlink，restore 必須整體回錯");
    assert!(
        !victim.join("data").exists(),
        "restore 穿過 symlink 在目標之外建了目錄"
    );
    assert!(landed(&target, &src).join("harmless.txt").is_file());
}

/// 檔案路徑的 symlink-in-the-way 拒絕。
#[cfg(unix)]
#[tokio::test]
async fn symlink_in_the_way_of_a_file_is_refused() {
    let t = TestRepo::new().await;
    let src = t.dir.path().join("src");
    make_source(&src);
    let repo = t.open().await;
    let s = repo
        .backup(std::slice::from_ref(&src), backup_options())
        .await
        .unwrap();
    let target = t.dir.path().join("out");
    let restored_root = landed(&target, &src);
    std::fs::create_dir_all(&restored_root).unwrap();
    std::os::unix::fs::symlink("/etc/hostname", restored_root.join("small.txt")).unwrap();
    let summary = repo
        .restore(&skey(&s.snapshot), &target, RestoreOptions::default())
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

/// ADR 019 A30：目標處本機的擋路物不是 repo 損壞：錯誤點名擋路的本機路徑，其餘照常還原。
#[cfg(unix)]
#[tokio::test]
async fn local_obstacles_in_the_tree_are_not_reported_as_corrupt() {
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
    let restored = landed(&target, &src);
    std::fs::create_dir_all(&restored).unwrap();
    let file_link = restored.join("small.txt");
    let dir_link = restored.join("sub");
    let dir_on_symlink = restored.join("link");
    std::os::unix::fs::symlink(outside.join("f"), &file_link).unwrap();
    std::os::unix::fs::symlink(&outside, &dir_link).unwrap();
    std::fs::create_dir(&dir_on_symlink).unwrap();
    let summary = repo
        .restore(&skey(&s.snapshot), &target, RestoreOptions::default())
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
        assert!(line.contains(&path.display().to_string()), "{line}");
    }
    assert_eq!(std::fs::read(restored.join("empty.txt")).unwrap(), b"");
    assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 0);
}

/// ADR 019 A30：root 定位上的某一層被使用者的 symlink 佔住：整個 restore 回錯，錯誤是本機擋路。
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
    let root_path = landed(&target, &src);
    std::fs::create_dir_all(root_path.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&outside, &root_path).unwrap();
    let err = repo
        .restore(&skey(&s.snapshot), &target, RestoreOptions::default())
        .await
        .expect_err("root 定位被 symlink 擋住，restore 整體回錯");
    let msg = err.to_string();
    assert!(!msg.contains("corrupt"), "{msg}");
    assert!(msg.contains(&root_path.display().to_string()), "{msg}");
    assert!(format!("{err:?}").starts_with("RestoreBlocked"), "{err:?}");
    assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 0);
}

/// 惡意 repo 可以是一條任意深的目錄鏈。restore 的走訪是遞迴：超過深度上限必須乾淨回錯（記進 errors），不是把
/// process 墊進 stack overflow；dry run 的計數與真的還原一致。在大堆疊的執行緒上跑（debug 的 frame 很大）。
#[test]
fn overly_deep_directory_chain_is_reported_not_crashed() {
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(overly_deep_body())
        })
        .unwrap()
        .join()
        .unwrap();
}

async fn overly_deep_body() {
    let t = TestRepo::new().await;
    let repo = t.open().await;
    // 最深處是一段只有一個空檔的目錄；往上每一層一段、一個名為 d 的目錄
    let mut items = vec![Item::Seg(Seg::new(vec![file(b"f", &[], 0)]))];
    for _ in 0..4_200 {
        let mut d = entry(b"d", node_type::DIR, meta_kind::GENERIC);
        d.tree = Some(meta_id(&repo, items.last().unwrap()));
        items.push(Item::Seg(Seg::new(vec![d])));
    }
    let root = Root {
        path: b"/deep".to_vec(),
        t: node_type::DIR,
        tree: Some(meta_id(&repo, items.last().unwrap())),
        entry: None,
    };
    let key = hand_snapshot(&repo, client(0x11), hours_from_now(1), vec![root], items).await;
    let msg = format!("nesting deeper than {}", kist_core::MAX_TREE_DEPTH);
    let summary = repo
        .restore(&key, &t.dir.path().join("out"), RestoreOptions::default())
        .await
        .unwrap();
    assert!(
        summary.errors.iter().any(|e| e.contains(&msg)),
        "restore 要在上限處回錯：{:?}",
        summary.errors.first()
    );
    // 上限處的節點被選到、它的上層沒有：真的還原為了寫它先建好上層（都計數），dry run 也要算。
    let deepest = format!("/deep{}", "/d".repeat(kist_core::MAX_TREE_DEPTH));
    let opts = RestoreOptions {
        include: kist_core::PathFilter::new(&[&deepest]).unwrap(),
        ..RestoreOptions::default()
    };
    let real = repo
        .restore(&key, &t.dir.path().join("out-deepest"), opts.clone())
        .await
        .unwrap();
    let plan = repo
        .plan_restore(&key, &t.dir.path().join("plan"), opts, &mut |_, _| {})
        .await
        .unwrap();
    assert_eq!(
        real.dirs,
        kist_core::MAX_TREE_DEPTH as u64 - 1,
        "{:?}",
        real.errors
    );
    assert_eq!(
        (plan.dirs, plan.errors.len(), &plan.unmatched_includes),
        (real.dirs, real.errors.len(), &real.unmatched_includes)
    );
}

/// ADR 019 A2：間接內容的 chunk 都讀不到（資料 pack 全刪）時，目標處使用者原有的檔從頭到尾沒被碰過。
#[tokio::test]
async fn failed_indirect_restore_keeps_the_existing_file() {
    let t = TestRepo::new().await;
    let src = t.dir.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("big.bin"), random_bytes(3, 8 << 20)).unwrap();
    let repo = t.open().await;
    let s = repo
        .backup(std::slice::from_ref(&src), backup_options())
        .await
        .unwrap();
    assert_eq!(
        root_entries(&repo, &s.snapshot).await[0].ct,
        content_type::INDIRECT
    );
    for pack in walk_files(&t.repo_path().join("packs")) {
        if pack.is_file() {
            std::fs::remove_file(&pack).unwrap();
        }
    }
    let target = t.dir.path().join("out");
    let restored = landed(&target, &src);
    std::fs::create_dir_all(&restored).unwrap();
    std::fs::write(restored.join("big.bin"), b"PRECIOUS user data").unwrap();
    let summary = repo
        .restore(&skey(&s.snapshot), &target, RestoreOptions::default())
        .await
        .unwrap();
    assert_eq!(summary.errors.len(), 1, "{summary:?}");
    assert_eq!(
        std::fs::read(restored.join("big.bin")).unwrap(),
        b"PRECIOUS user data"
    );
    assert_no_restore_temp(&restored);
}

/// ADR 019 A2：缺 chunk 的直接內容——正式檔名底下仍是原本的內容，不是寫了一半的檔，也沒被刪。
#[tokio::test]
async fn failed_direct_restore_keeps_the_existing_file() {
    let t = TestRepo::new().await;
    let repo = t.open().await;
    let chunks = small_chunks(&t, &repo, 1).await;
    let (payload, id) = &chunks[0];
    let key = hand_dir(
        &repo,
        b"/hand",
        vec![file(
            b"file.bin",
            &[*id, repo.keys().chunk_id(b"missing")],
            payload.len() as u64 + 7,
        )],
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
        std::fs::read(dir.join("file.bin")).unwrap(),
        b"PRECIOUS user data"
    );
    assert_no_restore_temp(&dir);
}

/// ADR 019 A2：正式檔名處擋著 symlink 維持拒絕——symlink 本身留著、不跟隨。
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
    let restored = landed(&target, &src);
    std::fs::create_dir_all(&restored).unwrap();
    let link = restored.join("small.txt");
    std::os::unix::fs::symlink(&outside, &link).unwrap();
    let summary = repo
        .restore(&skey(&s.snapshot), &target, RestoreOptions::default())
        .await
        .unwrap();
    assert_eq!(summary.errors.len(), 1, "{summary:?}");
    assert!(
        summary.errors[0].contains("symlink is in the way"),
        "{summary:?}"
    );
    assert!(std::fs::symlink_metadata(&link)
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(std::fs::read_link(&link).unwrap(), outside);
    assert_eq!(std::fs::read(&outside).unwrap(), b"outside content");
    assert_no_restore_temp(&restored);
}

/// ADR 019 A2：同一個 snapshot 還原兩次，裡面有唯讀檔與唯讀目錄：第二次照樣成功，mode 等於記錄的值。
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
    let restored = landed(&target, &src);
    let first = repo
        .restore(&skey(&s.snapshot), &target, RestoreOptions::default())
        .await
        .unwrap();
    let second = repo
        .restore(&skey(&s.snapshot), &target, RestoreOptions::default())
        .await
        .unwrap();
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
    assert_eq!(modes, ["444", "555", "444"], "第二次：{second:?}");
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

/// 既有的唯讀目錄裡一個子項目都沒寫（空目錄，或子項目被排除）時，套 metadata 之前也要暫時加寫入權。
#[cfg(unix)]
#[tokio::test]
async fn restore_twice_over_read_only_dirs_nothing_is_written_into() {
    use std::ffi::OsStr;
    use std::os::unix::fs::PermissionsExt;
    let chmod = |p: &std::path::Path, mode: u32| {
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode)).unwrap();
    };
    let dirs = ["empty", "full"];
    let t = TestRepo::new().await;
    let src = t.dir.path().join("src");
    std::fs::create_dir_all(src.join("empty")).unwrap();
    std::fs::create_dir_all(src.join("full")).unwrap();
    std::fs::write(src.join("full").join("f"), b"f").unwrap();
    for d in dirs {
        xattr::set(src.join(d), OsStr::new("user.k"), b"1").unwrap();
        chmod(&src.join(d), 0o555);
    }
    let repo = t.open().await;
    let s = repo
        .backup(std::slice::from_ref(&src), backup_options())
        .await
        .unwrap();
    let abs = std::fs::canonicalize(&src).unwrap();
    let target = t.dir.path().join("out");
    let restored = landed(&target, &abs);
    let first = repo
        .restore(&skey(&s.snapshot), &target, RestoreOptions::default())
        .await
        .unwrap();
    let old = filetime::FileTime::from_unix_time(946_684_800, 0);
    for d in dirs {
        filetime::set_file_mtime(restored.join(d), old).unwrap();
    }
    let opts = RestoreOptions {
        exclude: kist_core::PathFilter::new(&[&format!("{}/full/f", abs.display())]).unwrap(),
        ..RestoreOptions::default()
    };
    let second = repo
        .restore(&skey(&s.snapshot), &target, opts)
        .await
        .unwrap();
    let observe = |p: &std::path::Path| {
        let meta = std::fs::metadata(p).unwrap();
        (
            format!("{:o}", meta.permissions().mode() & 0o7777),
            filetime::FileTime::from_last_modification_time(&meta),
            xattr::get(p, "user.k").unwrap(),
        )
    };
    let seen: Vec<_> = dirs.iter().map(|d| observe(&restored.join(d))).collect();
    let want: Vec<_> = dirs
        .iter()
        .map(|d| {
            (
                "555".to_owned(),
                observe(&src.join(d)).1,
                Some(b"1".to_vec()),
            )
        })
        .collect();
    for d in dirs {
        chmod(&src.join(d), 0o755);
        chmod(&restored.join(d), 0o755);
    }
    assert!(first.errors.is_empty(), "第一次：{first:?}");
    assert!(second.errors.is_empty(), "第二次：{second:?}");
    assert_eq!(seen, want, "(mode, mtime, user.k) of {dirs:?}");
}

/// 同一個 snapshot 還原兩次，硬連結關係要還在（第二次時後面的名字 link 回 EEXIST，要以暫存名 link 再 rename）。
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
    let restored = landed(&target, &src);
    for round in 1..=2 {
        let summary = repo
            .restore(&skey(&s.snapshot), &target, RestoreOptions::default())
            .await
            .unwrap();
        let a = std::fs::symlink_metadata(restored.join("a.bin")).unwrap();
        let b = std::fs::symlink_metadata(restored.join("b.bin")).unwrap();
        assert!(summary.errors.is_empty(), "第 {round} 次：{summary:?}");
        assert_eq!((a.ino(), a.nlink()), (b.ino(), 2), "第 {round} 次");
        assert_eq!(std::fs::read(restored.join("b.bin")).unwrap(), b"shared");
        assert_no_restore_temp(&restored);
    }
}

/// ADR 019 A4：snapshot 裡有擁有者只能進入、不能讀的目錄（0311）：兩次還原都保住硬連結、子項目照寫；第二次只有
/// 那個目錄本身的 metadata 回 Permission denied。以 root 跑就略過。
#[cfg(target_os = "linux")]
#[tokio::test]
async fn unreadable_dir_in_the_snapshot_keeps_hard_links_and_children() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    if rustix::process::geteuid().is_root() {
        eprintln!("skipping: running as root");
        return;
    }
    let t = TestRepo::new().await;
    let src = t.dir.path().join("src");
    std::fs::create_dir_all(src.join("d")).unwrap();
    std::fs::create_dir_all(src.join("e")).unwrap();
    std::fs::write(src.join("d").join("f1"), b"linked").unwrap();
    std::fs::write(src.join("d").join("f2"), b"plain").unwrap();
    std::fs::hard_link(src.join("d").join("f1"), src.join("e").join("g")).unwrap();
    let repo = t.open().await;
    let s = repo
        .backup(std::slice::from_ref(&src), backup_options())
        .await
        .unwrap();
    let mut entries = root_entries(&repo, &s.snapshot).await;
    let d = entries.iter_mut().find(|e| e.n == b"d").unwrap();
    d.mode = d.mode.map(|m| (m & !0o7777) | 0o311);
    let key = hand_dir(&repo, b"/hand", entries).await;

    let target = t.dir.path().join("out");
    let restored = target.join("hand");
    let d = restored.join("d");
    let mut rounds = Vec::new();
    for _ in 1..=2 {
        let summary = repo
            .restore(&key, &target, RestoreOptions::default())
            .await
            .unwrap();
        let f1 = std::fs::symlink_metadata(d.join("f1")).unwrap();
        let g = std::fs::symlink_metadata(restored.join("e").join("g")).unwrap();
        let mode = std::fs::symlink_metadata(&d).unwrap().mode() & 0o7777;
        rounds.push((
            summary,
            (f1.ino() == g.ino(), g.nlink()),
            format!("{mode:o}"),
            std::fs::read(restored.join("e").join("g")).unwrap(),
        ));
    }
    std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o755)).unwrap();
    let (first, first_link, first_mode, first_g) = &rounds[0];
    assert!(first.errors.is_empty(), "第一次：{first:?}");
    assert_eq!((first.files, first.dirs), (3, 2), "第一次：{first:?}");
    assert_eq!(*first_link, (true, 2));
    assert_eq!(first_mode, "311");
    assert_eq!(first_g, b"linked");
    let (second, second_link, second_mode, second_g) = &rounds[1];
    assert_eq!((second.files, second.dirs), (3, 2), "第二次：{second:?}");
    assert_eq!(*second_link, (true, 2), "第二次：{second:?}");
    assert_eq!(second_mode, "311");
    assert_eq!(second_g, b"linked");
    assert_eq!(second.errors.len(), 1, "第二次：{second:?}");
    assert!(second.errors[0].contains("Permission denied"), "{second:?}");
    assert_no_restore_temp(&restored.join("d"));
    assert_no_restore_temp(&restored.join("e"));
}

/// ADR 019 A4：還原目標本身是只能進入、不能讀的目錄（0311），或指向它的 symlink：照常還原。以 root 跑就略過。
#[cfg(target_os = "linux")]
#[tokio::test]
async fn unreadable_target_is_restored_into() {
    use std::os::unix::fs::PermissionsExt;
    if rustix::process::geteuid().is_root() {
        eprintln!("skipping: running as root");
        return;
    }
    let t = TestRepo::new().await;
    let src = t.dir.path().join("src");
    std::fs::create_dir_all(src.join("sub")).unwrap();
    std::fs::write(src.join("sub").join("f.txt"), b"data").unwrap();
    let repo = t.open().await;
    let s = repo
        .backup(std::slice::from_ref(&src), backup_options())
        .await
        .unwrap();
    let direct = t.dir.path().join("direct");
    let behind_link = t.dir.path().join("behind-link");
    let link = t.dir.path().join("link");
    std::os::unix::fs::symlink(&behind_link, &link).unwrap();
    for (real, target) in [(&direct, &direct), (&behind_link, &link)] {
        std::fs::create_dir(real).unwrap();
        std::fs::set_permissions(real, std::fs::Permissions::from_mode(0o311)).unwrap();
        let summary = repo
            .restore(&skey(&s.snapshot), target, RestoreOptions::default())
            .await;
        std::fs::set_permissions(real, std::fs::Permissions::from_mode(0o755)).unwrap();
        let summary = summary.unwrap();
        assert!(
            summary.errors.is_empty(),
            "{}: {summary:?}",
            target.display()
        );
        assert_eq!(
            std::fs::read(landed(real, &src).join("sub").join("f.txt")).unwrap(),
            b"data"
        );
    }
}

/// ADR 019 A2：覆寫使用者原有的 0o600 檔、而 metadata 套不上（xattr 名稱過長）時，內容照舊 rename、記一個錯，
/// 檔案不能比記錄的 mode 寬鬆。
#[cfg(unix)]
#[tokio::test]
async fn overwriting_a_private_file_stays_private_when_metadata_fails() {
    use serde_bytes::ByteBuf;
    use std::collections::BTreeMap;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let t = TestRepo::new().await;
    let repo = t.open().await;
    let chunks = small_chunks(&t, &repo, 1).await;
    let (payload, id) = &chunks[0];
    let me = std::fs::metadata(t.dir.path()).unwrap();
    let mut too_long = b"user.".to_vec();
    too_long.extend(vec![b'x'; 300]);
    let mut secret = entry(b"secret.txt", node_type::FILE, meta_kind::POSIX);
    secret.size = payload.len() as u64;
    secret.chunks = vec![ItemId::from(*id)];
    secret.mode = Some(0o100600);
    secret.uid = Some(me.uid());
    secret.gid = Some(me.gid());
    secret.mtime = Some(1_700_000_000_000_000_000);
    secret.xattrs = Some(BTreeMap::from([(
        ByteBuf::from(too_long),
        ByteBuf::from(b"v".to_vec()),
    )]));
    let key = hand_dir(&repo, b"/hand", vec![secret]).await;
    let target = t.dir.path().join("out");
    let dir = target.join("hand");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("secret.txt"), b"old secret").unwrap();
    std::fs::set_permissions(dir.join("secret.txt"), PermissionsExt::from_mode(0o600)).unwrap();
    let summary = repo
        .restore(&key, &target, RestoreOptions::default())
        .await
        .unwrap();
    let mode = std::fs::symlink_metadata(dir.join("secret.txt"))
        .unwrap()
        .mode()
        & 0o7777;
    assert_eq!(summary.errors.len(), 1, "{summary:?}");
    assert!(summary.errors[0].contains("setting xattr"), "{summary:?}");
    assert_eq!(std::fs::read(dir.join("secret.txt")).unwrap(), *payload);
    assert_eq!(format!("{mode:o}"), "600", "{summary:?}");
    assert_no_restore_temp(&dir);
}

/// ADR 019 A2：沒有記錄 mode 的條目（generic 來源）不套 mode：覆寫的 0o600 檔維持 0o600，新建的檔是 umask 預設。
#[cfg(unix)]
#[tokio::test]
async fn entries_without_a_recorded_mode_keep_the_existing_mode() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let t = TestRepo::new().await;
    let repo = t.open().await;
    let chunks = small_chunks(&t, &repo, 1).await;
    let (payload, id) = &chunks[0];
    let generic = |n: &[u8]| file(n, &[*id], payload.len() as u64);
    let key = hand_dir(
        &repo,
        b"/hand",
        vec![generic(b"fresh.txt"), generic(b"private.txt")],
    )
    .await;
    let target = t.dir.path().join("out");
    let dir = target.join("hand");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("private.txt"), b"old secret").unwrap();
    std::fs::set_permissions(dir.join("private.txt"), PermissionsExt::from_mode(0o600)).unwrap();
    std::fs::write(dir.join("reference"), b"").unwrap();
    let summary = repo
        .restore(&key, &target, RestoreOptions::default())
        .await
        .unwrap();
    let mode_of = |name: &str| {
        let m = std::fs::symlink_metadata(dir.join(name)).unwrap();
        format!("{:o}", m.mode() & 0o7777)
    };
    assert!(summary.errors.is_empty(), "{summary:?}");
    assert_eq!(std::fs::read(dir.join("private.txt")).unwrap(), *payload);
    assert_eq!(std::fs::read(dir.join("fresh.txt")).unwrap(), *payload);
    assert_eq!(
        [mode_of("private.txt"), mode_of("fresh.txt")],
        ["600".to_owned(), mode_of("reference")]
    );
    assert_no_restore_temp(&dir);
}

/// ADR 019 A4：restore 途中反覆把目標之下的中間目錄換成指向目標之外的 symlink：寫入一律落在已開的真目錄。
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
    let outside = t.dir.path().join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    let target = t.dir.path().join("out");
    let restored = landed(&target, &src);
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
        .restore(&skey(&s.snapshot), &target, RestoreOptions::default())
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
        "換了 {swaps} 次；{} 個檔寫到目標之外",
        leaked.len()
    );
    assert_eq!(placed as u64, summary.files);
    assert!(summary.errors.is_empty(), "換了 {swaps} 次：{summary:?}");
    assert_eq!(summary.files, FILES as u64);
}

/// ADR 019 A43：非 root 還原不 chown（擁有者是別人也一樣、也不回錯），記錄的 setuid／setgid 照套。以 root 跑就略過
/// （UNVERIFIED，沒有 root）。
#[cfg(unix)]
#[tokio::test]
async fn non_root_restore_keeps_setid_bits_and_does_not_chown() {
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
    let mut entries = root_entries(&repo, &s.snapshot).await;
    for e in &mut entries {
        assert!(
            e.uid.is_some() && e.gid.is_some(),
            "前提：posix 條目記錄擁有者"
        );
        e.uid = Some(other);
        e.gid = Some(other);
    }
    let mut sftp = entry(b"sftp.bin", node_type::FILE, meta_kind::SFTP);
    sftp.mode = Some(0o104755);
    sftp.mtime = Some(1_000_000_000_000_000_000);
    entries.push(sftp);
    let key = hand_dir(&repo, b"/foreign", entries).await;
    let target = t.dir.path().join("out");
    let summary = repo
        .restore(&key, &target, RestoreOptions::default())
        .await
        .unwrap();
    let restored = target.join("foreign");
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
