//! v4 的 FsCore 對真 repo 的讀取路徑（從 v3 的 fs_read.rs 搬過來，加上 v4 才有的）：瀏覽、隨機讀、chunk 清單、
//! symlink、xattr、§9 依 `Root.t` 的頂層、inode 去重、多段的目錄、看到新 snapshot 時重新載入。不掛載——直接
//! 驅動核心邏輯。

#[path = "v4_common/mod.rs"]
mod v4_common;

use kist_format::v4::ids::ItemId;
use kist_format::v4::meta::{meta_kind, node_type, Entry, Item, Seg};
use kist_format::v4::snapshot::Root;
use kist_mount::v4::FsCore;
use kist_mount::Kind;
use v4_common::*;

/// src/ 下有小檔、跨多 chunk 的大檔、子目錄、symlink。回傳 big.bin 的 bytes。
async fn setup() -> (TestRepo, Vec<u8>) {
    let t = TestRepo::new().await;
    let src = t.dir.path().join("src");
    std::fs::create_dir_all(src.join("sub")).unwrap();
    std::fs::write(src.join("hello.txt"), b"hello mount").unwrap();
    let big: Vec<u8> = (0..1024 * 1024u32).map(|i| (i * 7 % 251) as u8).collect();
    std::fs::write(src.join("big.bin"), &big).unwrap();
    std::fs::write(src.join("sub").join("nested.txt"), b"nested").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink("hello.txt", src.join("link")).unwrap();
    t.backup(&src).await;
    (t, big)
}

/// 從 snapshot 根走到 `parts`（本機來源的定位是 `/tmp/<tmpname>/...`）。
async fn walk(core: &FsCore, ts: &str, tmpname: &str, parts: &[&str]) -> u64 {
    let client_ino = core.lookup(1, CLIENT.as_bytes()).await.unwrap().ino;
    let snap_ino = core.lookup(client_ino, ts.as_bytes()).await.unwrap().ino;
    let mut cur = core.lookup(snap_ino, b"tmp").await.unwrap().ino;
    cur = core.lookup(cur, tmpname.as_bytes()).await.unwrap().ino;
    for p in parts {
        cur = core.lookup(cur, p.as_bytes()).await.unwrap().ino;
    }
    cur
}

async fn list(core: &FsCore, ino: u64) -> Vec<Vec<u8>> {
    let fh = core.opendir(ino).await.unwrap();
    let page = core.readdir(fh, 0, 100_000).unwrap();
    core.releasedir(fh);
    page.into_iter().map(|e| e.name).collect()
}

async fn timestamps(core: &FsCore) -> Vec<String> {
    let client_ino = core.lookup(1, CLIENT.as_bytes()).await.unwrap().ino;
    list(core, client_ino)
        .await
        .into_iter()
        .map(|n| String::from_utf8(n).unwrap())
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn browse_and_read_small_file() {
    let (t, _big) = setup().await;
    let core = t.fs_core().await;
    let fh = core.opendir(1).await.unwrap();
    let page = core.readdir(fh, 0, 100).unwrap();
    core.releasedir(fh);
    assert_eq!(page.len(), 1);
    assert_eq!(page[0].name, CLIENT.as_bytes());
    assert_eq!(page[0].kind, Kind::Dir);

    let ts = timestamps(&core).await.remove(0);
    let src_ino = walk(&core, &ts, &t.tmpname(), &["src"]).await;
    let hello = core.lookup(src_ino, b"hello.txt").await.unwrap();
    assert_eq!(hello.attr.kind, Kind::File);
    assert_eq!(
        core.read_file(hello.ino, 0, 1024).await.unwrap(),
        b"hello mount"
    );
    let sub = core.lookup(src_ino, b"sub").await.unwrap();
    assert_eq!(sub.attr.kind, Kind::Dir);
    let nested = core.lookup(sub.ino, b"nested.txt").await.unwrap();
    assert_eq!(core.read_file(nested.ino, 0, 64).await.unwrap(), b"nested");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn random_reads_across_chunks_match_source() {
    let (t, big) = setup().await;
    let core = t.fs_core().await;
    let ts = timestamps(&core).await.remove(0);
    let big_ino = walk(&core, &ts, &t.tmpname(), &["src", "big.bin"]).await;
    let (attr, _) = core.getattr(big_ino).unwrap();
    assert_eq!(attr.size, big.len() as u64, "size 來自 entry，不必解密");
    let mut offset = 0u64;
    let mut collected = Vec::new();
    loop {
        let chunk = core.read_file(big_ino, offset, 100 * 1024).await.unwrap();
        if chunk.is_empty() {
            break;
        }
        offset += chunk.len() as u64;
        collected.extend_from_slice(&chunk);
    }
    assert_eq!(collected, big);
    for offset in [
        0u64,
        1,
        4095,
        4096,
        65535,
        65536,
        500_000,
        big.len() as u64 - 1,
    ] {
        let want_end = (offset + 1000).min(big.len() as u64);
        let got = core
            .read_file(big_ino, offset, (want_end - offset) as u32)
            .await
            .unwrap();
        assert_eq!(
            got,
            big[offset as usize..want_end as usize],
            "offset {offset}"
        );
    }
    assert!(core
        .read_file(big_ino, big.len() as u64, 10)
        .await
        .unwrap()
        .is_empty());
}

/// 超過 256 個 chunk：ChunkList（§8 的 `ct=1`），邊界表的長度來自持有者記錄的 `r`。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn indirect_content_resolves_via_chunk_list() {
    let t = TestRepo::new().await;
    let huge: Vec<u8> = (0..8 * 1024 * 1024u32)
        .map(|i| (i.wrapping_mul(31) % 253) as u8)
        .collect();
    let src = t.dir.path().join("huge");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("huge.bin"), &huge).unwrap();
    t.backup(&src).await;
    let core = t.fs_core().await;
    let ts = timestamps(&core).await.pop().unwrap();
    let huge_ino = walk(&core, &ts, &t.tmpname(), &["huge", "huge.bin"]).await;
    let got = core.read_file(huge_ino, 0, 8 * 1024 * 1024).await.unwrap();
    assert_eq!(got, huge);
    let mid = 4 * 1024 * 1024 + 7;
    let got = core.read_file(huge_ino, mid, 4096).await.unwrap();
    assert_eq!(got, huge[mid as usize..(mid + 4096) as usize]);
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn symlinks_missing_names_and_errors() {
    let (t, _big) = setup().await;
    let core = t.fs_core().await;
    let ts = timestamps(&core).await.remove(0);
    let src_ino = walk(&core, &ts, &t.tmpname(), &["src"]).await;
    let link = core.lookup(src_ino, b"link").await.unwrap();
    assert_eq!(link.attr.kind, Kind::Symlink);
    assert_eq!(core.readlink(link.ino).unwrap(), b"hello.txt");
    assert_eq!(
        core.lookup(src_ino, b"no-such-file").await.unwrap_err(),
        kist_mount::FsError::NotFound
    );
    let hello = core.lookup(src_ino, b"hello.txt").await.unwrap();
    assert_eq!(
        core.lookup(hello.ino, b"anything").await.unwrap_err(),
        kist_mount::FsError::NotFound
    );
    let client_ino = core.lookup(1, CLIENT.as_bytes()).await.unwrap().ino;
    assert_eq!(
        core.lookup(client_ino, b"not-a-timestamp")
            .await
            .unwrap_err(),
        kist_mount::FsError::NotFound
    );
    assert_eq!(
        core.lookup(1, b"not-a-client").await.unwrap_err(),
        kist_mount::FsError::NotFound
    );
    assert!(core.getattr(u64::MAX).is_err());
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn xattrs_are_exposed() {
    let (t, _big) = setup().await;
    let file = t.dir.path().join("src").join("hello.txt");
    xattr::set(&file, "user.kist-test", b"yes").unwrap();
    t.backup(&t.dir.path().join("src")).await;
    let core = t.fs_core().await;
    let ts = timestamps(&core).await.pop().unwrap();
    let src_ino = walk(&core, &ts, &t.tmpname(), &["src"]).await;
    let hello = core.lookup(src_ino, b"hello.txt").await.unwrap();
    assert_eq!(
        core.get_xattr(hello.ino, b"user.kist-test")
            .unwrap()
            .as_deref(),
        Some(b"yes".as_ref())
    );
    assert!(core
        .list_xattrs(hello.ino)
        .unwrap()
        .contains(&b"user.kist-test".to_vec()));
    assert_eq!(core.get_xattr(hello.ino, b"user.nope").unwrap(), None);
}

/// §9：沒有段的目錄 root（`/`、只有 scheme 的 `s3://`）的內容攤平到 snapshot 頂層，與 restore 直接放進目標一致。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_root_without_segments_flattens_to_the_top() {
    for locator in [&b"/"[..], b"s3://"] {
        let t = TestRepo::new().await;
        let repo = t.open().await;
        let empty = Item::Seg(Seg::new(Vec::new()));
        let empty_id = repo.keys().meta_id(&empty.encode().unwrap());
        let dir = |n: &[u8]| {
            let mut e = Entry::new(n.to_vec(), node_type::DIR, meta_kind::GENERIC);
            e.tree = Some(empty_id);
            e
        };
        let top = Item::Seg(Seg::new(vec![dir(b"bin"), dir(b"etc")]));
        let root = Root {
            path: locator.to_vec(),
            t: node_type::DIR,
            tree: Some(repo.keys().meta_id(&top.encode().unwrap())),
            entry: None,
        };
        let ts = hand_snapshot(&repo, 1_700_000_000, vec![root], vec![empty, top]).await;
        let core = t.fs_core().await;
        let client_ino = core.lookup(1, CLIENT.as_bytes()).await.unwrap().ino;
        let snap_ino = core.lookup(client_ino, ts.as_bytes()).await.unwrap().ino;
        assert_eq!(
            list(&core, snap_ino).await,
            [b"bin".to_vec(), b"etc".to_vec()],
            "{}",
            String::from_utf8_lossy(locator)
        );
        let bin = core.lookup(snap_ino, b"bin").await.unwrap();
        assert_eq!(bin.attr.kind, Kind::Dir);
        assert!(list(&core, bin.ino).await.is_empty());
    }
}

/// §9：檔案 root 是它自己的 entry、放在去掉最後一段的層級（v3 要讀內容猜，猜錯時退回成目錄）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_file_root_is_a_file_under_its_parent_segments() {
    let t = TestRepo::new().await;
    let src = t.dir.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("f.txt"), b"file source").unwrap();
    t.backup(&src.join("f.txt")).await;
    let core = t.fs_core().await;
    let ts = timestamps(&core).await.remove(0);
    let src_ino = walk(&core, &ts, &t.tmpname(), &["src"]).await;
    assert_eq!(list(&core, src_ino).await, [b"f.txt".to_vec()]);
    let f = core.lookup(src_ino, b"f.txt").await.unwrap();
    assert_eq!(f.attr.kind, Kind::File);
    assert_eq!(core.read_file(f.ino, 0, 64).await.unwrap(), b"file source");
}

/// 同一個（父 ino, 名稱）再 lookup 回同一個 ino，inode 表不長大（ADR 019 A6）：client、snapshot、合成的中介目錄、
/// 真實條目四個配號點都要走到。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repeated_lookup_returns_same_inode() {
    let t = TestRepo::new().await;
    let src = t.dir.path().join("src");
    std::fs::create_dir_all(src.join("a")).unwrap();
    std::fs::create_dir_all(src.join("b")).unwrap();
    std::fs::write(src.join("a").join("x.txt"), b"in a").unwrap();
    std::fs::write(src.join("a").join("y.txt"), b"also in a").unwrap();
    std::fs::write(src.join("b").join("x.txt"), b"in b").unwrap();
    t.backup(&src).await;
    let core = t.fs_core().await;
    let ts = timestamps(&core).await.remove(0);
    let tmpname = t.tmpname();
    let client = core.lookup(1, CLIENT.as_bytes()).await.unwrap();
    let snap = core.lookup(client.ino, ts.as_bytes()).await.unwrap();
    let tmp = core.lookup(snap.ino, b"tmp").await.unwrap();
    let a = walk(&core, &ts, &tmpname, &["src", "a"]).await;
    let ax = core.lookup(a, b"x.txt").await.unwrap();
    let count = core.inode_count();

    let client_again = core.lookup(1, CLIENT.as_bytes()).await.unwrap();
    assert_eq!(client_again.ino, client.ino);
    assert_eq!(client_again.ttl, kist_mount::v4::VOLATILE_TTL);
    assert_eq!(
        core.lookup(client.ino, ts.as_bytes()).await.unwrap().ino,
        snap.ino
    );
    assert_eq!(core.lookup(snap.ino, b"tmp").await.unwrap().ino, tmp.ino);
    assert_eq!(walk(&core, &ts, &tmpname, &["src", "a"]).await, a);
    assert_eq!(core.lookup(a, b"x.txt").await.unwrap().ino, ax.ino);
    assert_eq!(core.inode_count(), count, "重複 lookup 不能配新 inode");
    assert_eq!(core.read_file(ax.ino, 0, 64).await.unwrap(), b"in a");

    let ay = core.lookup(a, b"y.txt").await.unwrap();
    assert_ne!(ay.ino, ax.ino);
    let b = walk(&core, &ts, &tmpname, &["src", "b"]).await;
    assert_ne!(b, a);
    let bx = core.lookup(b, b"x.txt").await.unwrap();
    assert_ne!(bx.ino, ax.ino);
    assert_eq!(core.read_file(bx.ino, 0, 64).await.unwrap(), b"in b");
}

/// 去重只管身分，不管存在：snapshot 被刪掉之後同一個名字要回 NotFound；沒有 snapshot 的 client 也是。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn vanished_snapshot_is_not_found_after_lookup() {
    let (t, _big) = setup().await;
    let core = t.fs_core().await;
    let ts = timestamps(&core).await.remove(0);
    let client_ino = core.lookup(1, CLIENT.as_bytes()).await.unwrap().ino;
    core.lookup(client_ino, ts.as_bytes()).await.unwrap();
    let count = core.inode_count();
    t.backend
        .delete(&format!("snapshots/{CLIENT}/{ts}"))
        .await
        .unwrap();
    assert_eq!(
        core.lookup(client_ino, ts.as_bytes()).await.unwrap_err(),
        kist_mount::FsError::NotFound
    );
    assert_eq!(
        core.lookup(1, CLIENT.as_bytes()).await.unwrap_err(),
        kist_mount::FsError::NotFound
    );
    assert_eq!(core.inode_count(), count);
}

/// snapshot 的 roots 讀不到（mpack 暫時不見）是那一次 lookup 的 I/O 錯誤，不記進去重表：恢復之後再查就是真的內容。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_snapshot_whose_roots_cannot_be_read_is_an_io_error_and_not_pinned() {
    let (t, _big) = setup().await;
    let core = t.fs_core().await;
    let ts = timestamps(&core).await.remove(0);
    let client_ino = core.lookup(1, CLIENT.as_bytes()).await.unwrap().ino;
    let mpacks = t.dir.path().join("repo").join("mpacks");
    let away = t.dir.path().join("repo").join("mpacks.away");
    std::fs::rename(&mpacks, &away).unwrap();
    let first = core.lookup(client_ino, ts.as_bytes()).await;
    std::fs::rename(&away, &mpacks).unwrap();
    assert_eq!(first.unwrap_err(), kist_mount::FsError::Io);
    let count = core.inode_count();
    let src_ino = walk(&core, &ts, &t.tmpname(), &["src"]).await;
    assert!(core.inode_count() > count);
    let hello = core.lookup(src_ino, b"hello.txt").await.unwrap();
    assert_eq!(
        core.read_file(hello.ino, 0, 64).await.unwrap(),
        b"hello mount"
    );
}

/// §7：多段的目錄。lookup 沿 Dir 的索引找到每一個名稱，opendir 列出全部。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_segmented_directory_is_listed_and_looked_up() {
    let t = TestRepo::new().await;
    let repo = t.open().await;
    let src = t.dir.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    let mut names = Vec::new();
    let mut in_seg = 0;
    let mut cuts = 0;
    let mut i = 0u32;
    while cuts < 2 || in_seg == 0 {
        let n = format!("f{i:06}");
        i += 1;
        let cut = in_seg >= 1024 && repo.keys().is_cut(n.as_bytes());
        names.push(n);
        in_seg += 1;
        if cut {
            cuts += 1;
            in_seg = 0;
        }
    }
    for n in &names {
        std::fs::write(src.join(n), n.as_bytes()).unwrap();
    }
    t.backup(&src).await;
    let core = t.fs_core().await;
    let ts = timestamps(&core).await.remove(0);
    let src_ino = walk(&core, &ts, &t.tmpname(), &["src"]).await;
    let listed: Vec<String> = list(&core, src_ino)
        .await
        .into_iter()
        .map(|n| String::from_utf8(n).unwrap())
        .collect();
    assert_eq!(listed, names);
    for n in [&names[0], &names[names.len() / 2], names.last().unwrap()] {
        let f = core.lookup(src_ino, n.as_bytes()).await.unwrap();
        assert_eq!(core.read_file(f.ino, 0, 64).await.unwrap(), n.as_bytes());
    }
    assert_eq!(
        core.lookup(src_ino, b"f000000x").await.unwrap_err(),
        kist_mount::FsError::NotFound
    );
}

/// mount 之後才寫的 snapshot：看到沒見過的 key 就重新載入世代，新 pack 裡的內容讀得到。持有者全部失敗時的
/// 重新載入限流 60 秒，所以第二個新 snapshot 靠的是這一條（它自己限流 1 秒）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn snapshots_written_after_mounting_are_readable() {
    let (t, _big) = setup().await;
    let core = t.fs_core().await;
    let mut known = timestamps(&core).await;
    for round in 0..2 {
        tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
        let src = t.dir.path().join(format!("new{round}"));
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(
            src.join("fresh.txt"),
            format!("written after mount {round}"),
        )
        .unwrap();
        t.backup(&src).await;
        let now = timestamps(&core).await;
        let ts = now.iter().find(|x| !known.contains(x)).unwrap().clone();
        known = now;
        let dir = walk(&core, &ts, &t.tmpname(), &[&format!("new{round}")]).await;
        let f = core.lookup(dir, b"fresh.txt").await.unwrap();
        assert_eq!(
            core.read_file(f.ino, 0, 64).await.unwrap(),
            format!("written after mount {round}").as_bytes(),
            "round {round}"
        );
    }
}

/// §7 的大小檢查：手工的檔案 entry 說的 size 與 chunk 合計不符 → 讀它是 I/O 錯誤，不讀出內容。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_file_whose_size_does_not_match_its_chunks_is_an_io_error() {
    let t = TestRepo::new().await;
    let src = t.dir.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    let payload = b"one small chunk".to_vec();
    std::fs::write(src.join("payload.bin"), &payload).unwrap();
    t.backup(&src).await;
    let repo = t.open().await;
    let mut e = Entry::new(b"liar.bin".to_vec(), node_type::FILE, meta_kind::GENERIC);
    e.size = payload.len() as u64 + 1;
    e.chunks = vec![ItemId::from(repo.keys().chunk_id(&payload))];
    let seg = Item::Seg(Seg::new(vec![e]));
    let root = Root {
        path: b"/evil".to_vec(),
        t: node_type::DIR,
        tree: Some(repo.keys().meta_id(&seg.encode().unwrap())),
        entry: None,
    };
    let ts = hand_snapshot(&repo, 1_800_000_000, vec![root], vec![seg]).await;
    let core = t.fs_core().await;
    let client_ino = core.lookup(1, CLIENT.as_bytes()).await.unwrap().ino;
    let snap_ino = core.lookup(client_ino, ts.as_bytes()).await.unwrap().ino;
    let evil = core.lookup(snap_ino, b"evil").await.unwrap().ino;
    let liar = core.lookup(evil, b"liar.bin").await.unwrap();
    assert_eq!(liar.attr.size, payload.len() as u64 + 1);
    assert_eq!(
        core.read_file(liar.ino, 0, 4096).await,
        Err(kist_mount::FsError::Io)
    );
}
