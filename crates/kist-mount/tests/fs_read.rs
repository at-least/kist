//! FsCore 對真 repo 的讀取路徑：瀏覽、隨機讀、indirect content、symlink、
//! xattr。不掛載——直接驅動核心邏輯，需要真實的 chunk 抓取與解密。

mod common;

use common::{TestRepo, CLIENT};
use kist_mount::corefs::Kind;
use kist_mount::FsCore;

/// 準備一份備份：src/ 下有小檔、跨多 chunk 的大檔、subdir、symlink。
/// 回傳 big.bin 的原始 bytes。
async fn setup() -> (TestRepo, Vec<u8>) {
    let t = TestRepo::new().await;
    let src = t.dir.path().join("src");
    std::fs::create_dir_all(src.join("sub")).unwrap();
    std::fs::write(src.join("hello.txt"), b"hello mount").unwrap();
    // 多 chunk（min 4 KiB / avg 16 KiB）：1 MiB 資料 → 幾十顆 chunk（direct）
    let big: Vec<u8> = (0..1024 * 1024u32).map(|i| (i * 7 % 251) as u8).collect();
    std::fs::write(src.join("big.bin"), &big).unwrap();
    std::fs::write(src.join("sub").join("nested.txt"), b"nested").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink("hello.txt", src.join("link")).unwrap();
    t.backup(&src).await;
    (t, big)
}

/// 從 snapshot 根走到 `parts` 指定的子路徑，回傳最後一層的 ino。
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

/// 兩個 timestamp 目錄名（依時間排序）。測試都只備份一或兩次，直接列。
async fn timestamps(core: &FsCore) -> Vec<String> {
    let client_ino = core.lookup(1, CLIENT.as_bytes()).await.unwrap().ino;
    let fh = core.opendir(client_ino).await.unwrap();
    let page = core.readdir(fh, 0, 100).unwrap();
    core.releasedir(fh);
    page.into_iter()
        .map(|e| String::from_utf8(e.name).unwrap())
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn browse_and_read_small_file() {
    let (t, _big) = setup().await;
    let core = t.fs_core().await;

    // 根目錄列出 client
    let fh = core.opendir(1).await.unwrap();
    let page = core.readdir(fh, 0, 100).unwrap();
    assert_eq!(page.len(), 1);
    assert_eq!(page[0].name, CLIENT.as_bytes());
    assert_eq!(page[0].kind, Kind::Dir);
    core.releasedir(fh);

    let ts = timestamps(&core).await.remove(0);
    let tmpname = t
        .dir
        .path()
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let src_ino = walk(&core, &ts, &tmpname, &["src"]).await;

    let hello = core.lookup(src_ino, b"hello.txt").await.unwrap();
    assert_eq!(hello.attr.kind, Kind::File);
    let data = core.read_file(hello.ino, 0, 1024).await.unwrap();
    assert_eq!(data, b"hello mount");

    // 子目錄也在（分段 tree chain）
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
    let tmpname = t
        .dir
        .path()
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let big_ino = walk(&core, &ts, &tmpname, &["src", "big.bin"]).await;
    let (attr, _) = core.getattr(big_ino).unwrap();
    assert_eq!(attr.size, big.len() as u64, "size 來自樹，不必解密");

    // 全檔讀（分頁 100 KiB）與原始資料逐位元組一致
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

    // 跨 chunk 邊界的偏移讀也要位元組一致
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
    // 越過檔尾 = 空
    assert!(core
        .read_file(big_ino, big.len() as u64, 10)
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn indirect_content_resolves_via_chunk_list() {
    let t = TestRepo::new().await;
    // 8 MiB 隨機檔：> 256 chunks → indirect content（清單本身也是 chunk）
    let huge: Vec<u8> = (0..8 * 1024 * 1024u32)
        .map(|i| (i.wrapping_mul(31) % 253) as u8)
        .collect();
    let src = t.dir.path().join("huge");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("huge.bin"), &huge).unwrap();
    t.backup(&src).await;

    let core = t.fs_core().await;
    let ts = timestamps(&core).await.pop().unwrap();
    let tmpname = t
        .dir
        .path()
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let huge_ino = walk(&core, &ts, &tmpname, &["huge", "huge.bin"]).await;
    let got = core.read_file(huge_ino, 0, 8 * 1024 * 1024).await.unwrap();
    assert_eq!(got.len(), huge.len());
    assert_eq!(got, huge, "indirect content 的全檔讀必須一致");

    // 隨機偏移也對（indirect 的邊界表同樣來自 index raw_len）
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
    let tmpname = t
        .dir
        .path()
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let src_ino = walk(&core, &ts, &tmpname, &["src"]).await;

    // symlink：readlink 回相對目標
    let link = core.lookup(src_ino, b"link").await.unwrap();
    assert_eq!(link.attr.kind, Kind::Symlink);
    assert_eq!(core.readlink(link.ino).unwrap(), b"hello.txt");

    // 不存在的名字 → NotFound
    assert_eq!(
        core.lookup(src_ino, b"no-such-file").await.unwrap_err(),
        kist_mount::FsError::NotFound
    );
    // 檔案底下不能再有東西
    let hello = core.lookup(src_ino, b"hello.txt").await.unwrap();
    assert_eq!(
        core.lookup(hello.ino, b"anything").await.unwrap_err(),
        kist_mount::FsError::NotFound
    );
    // 爛 timestamp → NotFound（早退）
    let client_ino = core.lookup(1, CLIENT.as_bytes()).await.unwrap().ino;
    assert_eq!(
        core.lookup(client_ino, b"not-a-timestamp")
            .await
            .unwrap_err(),
        kist_mount::FsError::NotFound
    );
    // 壞 inode → NotFound
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
    let ts_list = timestamps(&core).await;
    let ts = ts_list.last().unwrap().clone(); // 第二次備份
    let tmpname = t
        .dir
        .path()
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let src_ino = walk(&core, &ts, &tmpname, &["src"]).await;
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
    // 沒有的 xattr → None
    assert_eq!(core.get_xattr(hello.ino, b"user.nope").unwrap(), None);
}

/// `kist backup /` 在 v2 會產生名為 "/" 的合成根，mount 把它的子樹攤平到
/// snapshot 頂層（與 restore 併入目標根的語意一致）。v3 沒有合成根：root 的
/// 內容樹直接掛在 `roots[].tree`（這裡以 public 的 `seal_tree` +
/// `write_snapshot` 手工組一個 path="/" 的 root，內容 bin、etc）。
/// v2 的攤平斷言原樣保留——v3 的 mount 對 locator 切不出組件的 root 是
/// 整個跳過（vpath），此測試釘住兩代語意的差異。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn slash_root_entry_flattens_to_top() {
    assert_root_flattens_to_top(b"/").await;
}

/// 只有 scheme、沒有組件的定位（`s3://`）與 `/` 同一規則：restore 落在
/// target 本身（fsmeta::locator_to_relative 回空路徑），mount 也要攤平——
/// 不能因為 corefs 與 vpath 切段規則不同，整個 root 在頂層消失。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scheme_only_root_entry_flattens_to_top() {
    assert_root_flattens_to_top(b"s3://").await;
}

/// 手工做一個 snapshot：唯一的 root 定位是 `locator`、內容是 bin、etc 兩個
/// 空目錄；斷言 snapshot 頂層直接就是 bin、etc。
async fn assert_root_flattens_to_top(locator: &[u8]) {
    use kist_format::snapshot::{format_key_timestamp, Root, Snapshot};
    use kist_format::tree::{meta_kind, node_type, Tree};
    use kist_format::FORMAT_VERSION;
    use time::OffsetDateTime;

    let t = TestRepo::new().await;
    let repo = t.open().await;

    fn dir_entry(name: &[u8], subtree: kist_format::TreeId) -> kist_format::tree::Entry {
        kist_format::tree::Entry {
            name: name.to_vec(),
            kind: node_type::DIR,
            meta_kind: meta_kind::POSIX,
            size: 0,
            target: Vec::new(),
            chunks: Vec::new(),
            content: 0,
            subtree,
            mode: Some(0o40555),
            uid: Some(0),
            gid: Some(0),
            mtime_ns: Some(0),
            ctime_ns: None,
            dev: None,
            inode: None,
            nlink: None,
            xattrs: None,
            etag: None,
            vern: None,
        }
    }

    // 空目錄 = 一個空 tree（v3 的 dir entry 必須帶非零 subtree）
    let empty = Tree {
        version: FORMAT_VERSION,
        entries: Vec::new(),
        prev: None,
    };
    let (bin_id, bin_bytes) = repo.seal_tree(empty.clone()).await.unwrap();
    t.backend
        .put(&kist_format::keys::tree(&bin_id), bin_bytes)
        .await
        .unwrap();
    let (etc_id, etc_bytes) = repo.seal_tree(empty).await.unwrap();
    t.backend
        .put(&kist_format::keys::tree(&etc_id), etc_bytes)
        .await
        .unwrap();

    // "/" 的內容樹：bin、etc 兩個空目錄（dir entry 不需要任何 chunk）
    let root_tree = Tree {
        version: FORMAT_VERSION,
        entries: vec![dir_entry(b"bin", bin_id), dir_entry(b"etc", etc_id)],
        prev: None,
    };
    let (root_id, root_bytes) = repo.seal_tree(root_tree).await.unwrap();
    t.backend
        .put(&kist_format::keys::tree(&root_id), root_bytes)
        .await
        .unwrap();

    // snapshot：time_ns 必須與 key 的時間戳一致（讀取端核對）
    let t0 = OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap();
    let ts = format_key_timestamp(t0).unwrap();
    let key = kist_format::keys::snapshot(&[0x22; 16], &ts);
    let snapshot = Snapshot {
        version: FORMAT_VERSION,
        roots: vec![Root {
            path: locator.to_vec().into(),
            tree: root_id,
        }],
        time_ns: t0.unix_timestamp_nanos() as i64,
        host: "handcraft".to_owned(),
        user: String::new(),
        client_id: vec![0x22; 16],
        parent: None,
        stats: Default::default(),
    };
    repo.write_snapshot(&key, snapshot).await.unwrap();

    // 瀏覽：頂層應該直接是 bin、etc（"/" 被攤平），而不是一個名為 "/" 的目錄
    let core = t.fs_core().await;
    let client_ino = core.lookup(1, CLIENT.as_bytes()).await.unwrap().ino;
    let fh = core.opendir(client_ino).await.unwrap();
    let page = core.readdir(fh, 0, 100).unwrap();
    core.releasedir(fh);
    assert_eq!(page.len(), 1);
    let ts = String::from_utf8(page[0].name.clone()).unwrap();

    let snap_ino = core.lookup(client_ino, ts.as_bytes()).await.unwrap().ino;
    let fh = core.opendir(snap_ino).await.unwrap();
    let page = core.readdir(fh, 0, 100).unwrap();
    core.releasedir(fh);
    let names: Vec<Vec<u8>> = page.iter().map(|e| e.name.clone()).collect();
    assert_eq!(
        names,
        vec![b"bin".to_vec(), b"etc".to_vec()],
        "{}: 子樹攤平到頂層",
        String::from_utf8_lossy(locator)
    );

    // 子目錄真的可以走進去（空的）
    let bin = core.lookup(snap_ino, b"bin").await.unwrap();
    assert_eq!(bin.attr.kind, Kind::Dir);
    let fh = core.opendir(bin.ino).await.unwrap();
    assert!(core.readdir(fh, 0, 10).unwrap().is_empty());
    core.releasedir(fh);
}

/// 間接內容的 chunk 清單要比 `v`（format.md §16：≠3 拒絕；ADR 019 A10）：
/// v=4 的清單不能被 mount 照 v3 的讀法讀出內容。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chunk_list_with_unknown_version_is_rejected() {
    use kist_format::snapshot::{format_key_timestamp, Root, Snapshot};
    use kist_format::tree::{content_type, meta_kind, node_type, ChunkList, Entry, Tree};
    use kist_format::FORMAT_VERSION;
    use time::OffsetDateTime;

    let t = TestRepo::new().await;
    let repo = t.open().await;
    // 兩個小檔各自成一顆 chunk：一顆是資料，一顆的明文正好是 v=4 的清單。
    let payload = b"payload listed by a v4 chunk list".to_vec();
    let list = kist_format::cbor::encode(&ChunkList {
        version: 4,
        chunks: vec![repo.keys().chunk_id(&payload)],
    })
    .unwrap();
    let src = t.dir.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("payload.bin"), &payload).unwrap();
    std::fs::write(src.join("list.cbor"), &list).unwrap();
    t.backup(&src).await;

    // 手工組：root "/evil" 的內容樹只有一個間接內容的檔案，chunks 指向清單 chunk。
    let tree = Tree::new(
        vec![Entry {
            name: b"file.bin".to_vec(),
            kind: node_type::FILE,
            meta_kind: meta_kind::GENERIC,
            size: payload.len() as u64,
            target: Vec::new(),
            content: content_type::INDIRECT,
            chunks: vec![repo.keys().chunk_id(&list)],
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
        }],
        None,
    );
    let (tree_id, tree_bytes) = repo.seal_tree(tree).await.unwrap();
    t.backend
        .put(&kist_format::keys::tree(&tree_id), tree_bytes)
        .await
        .unwrap();
    // snapshot：time_ns 必須與 key 的時間戳一致（讀取端核對）
    let t0 = OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap();
    let ts = format_key_timestamp(t0).unwrap();
    let key = kist_format::keys::snapshot(&[0x22; 16], &ts);
    let snapshot = Snapshot {
        version: FORMAT_VERSION,
        roots: vec![Root {
            path: b"/evil".to_vec().into(),
            tree: tree_id,
        }],
        time_ns: t0.unix_timestamp_nanos() as i64,
        host: "handcraft".to_owned(),
        user: String::new(),
        client_id: vec![0x22; 16],
        parent: None,
        stats: Default::default(),
    };
    repo.write_snapshot(&key, snapshot).await.unwrap();

    let core = t.fs_core().await;
    let client_ino = core.lookup(1, CLIENT.as_bytes()).await.unwrap().ino;
    let snap_ino = core.lookup(client_ino, ts.as_bytes()).await.unwrap().ino;
    let evil_ino = core.lookup(snap_ino, b"evil").await.unwrap().ino;
    let file_ino = core.lookup(evil_ino, b"file.bin").await.unwrap().ino;
    assert_eq!(
        core.read_file(file_ino, 0, 4096).await,
        Err(kist_mount::FsError::Io),
        "v=4 的 chunk 清單不能被讀出內容"
    );
}

/// 同一個（父 ino, 名稱）再 lookup 要回同一個 ino，inode 表不長大（ADR 019 A6）。
/// 每次都配新號的話，頂層 client 目錄的 entry TTL（1 秒）一過期，kernel 重新
/// lookup 拿到新號，底下已快取的整棵子樹就作廢：真掛載 `cp -r` 兩萬個檔會漏檔，
/// mount 的記憶體也一路長。四個配號點都要走到：client、snapshot、合成的中介
/// 目錄、真實條目。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repeated_lookup_returns_same_inode() {
    let t = TestRepo::new().await;
    // 同名檔放在兩個不同目錄底下：a/x.txt、b/x.txt
    let src = t.dir.path().join("src");
    std::fs::create_dir_all(src.join("a")).unwrap();
    std::fs::create_dir_all(src.join("b")).unwrap();
    std::fs::write(src.join("a").join("x.txt"), b"in a").unwrap();
    std::fs::write(src.join("a").join("y.txt"), b"also in a").unwrap();
    std::fs::write(src.join("b").join("x.txt"), b"in b").unwrap();
    t.backup(&src).await;

    let core = t.fs_core().await;
    let ts = timestamps(&core).await.remove(0);
    let tmpname = t
        .dir
        .path()
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();

    // 第一次走完整條路徑，記下每一層的 ino
    let client = core.lookup(1, CLIENT.as_bytes()).await.unwrap();
    let snap = core.lookup(client.ino, ts.as_bytes()).await.unwrap();
    let tmp = core.lookup(snap.ino, b"tmp").await.unwrap();
    let a = walk(&core, &ts, &tmpname, &["src", "a"]).await;
    let ax = core.lookup(a, b"x.txt").await.unwrap();
    let count = core.inode_count();

    // 同一條路再走一次：每一層都回同一個 ino，表不長大
    let client_again = core.lookup(1, CLIENT.as_bytes()).await.unwrap();
    assert_eq!(
        client_again.ino, client.ino,
        "頂層 client（TTL 1 秒）過期重查要回同一個 ino"
    );
    assert_eq!(client_again.ttl, kist_mount::corefs::VOLATILE_TTL);
    assert_eq!(
        core.lookup(client.ino, ts.as_bytes()).await.unwrap().ino,
        snap.ino,
        "snapshot 目錄"
    );
    assert_eq!(
        core.lookup(snap.ino, b"tmp").await.unwrap().ino,
        tmp.ino,
        "合成的中介目錄"
    );
    assert_eq!(walk(&core, &ts, &tmpname, &["src", "a"]).await, a);
    assert_eq!(
        core.lookup(a, b"x.txt").await.unwrap().ino,
        ax.ino,
        "真實條目"
    );
    assert_eq!(core.inode_count(), count, "重複 lookup 不能配新 inode");

    // 同一個 ino 照樣讀得到內容
    assert_eq!(core.read_file(ax.ino, 0, 64).await.unwrap(), b"in a");

    // 不同名字、不同父目錄仍是不同的 inode
    let ay = core.lookup(a, b"y.txt").await.unwrap();
    assert_ne!(ay.ino, ax.ino, "同一個目錄底下的不同名字");
    let b = walk(&core, &ts, &tmpname, &["src", "b"]).await;
    assert_ne!(b, a);
    let bx = core.lookup(b, b"x.txt").await.unwrap();
    assert_ne!(bx.ino, ax.ino, "不同父目錄底下的同名條目");
    assert_eq!(core.read_file(bx.ino, 0, 64).await.unwrap(), b"in b");
}

/// 去重只管身分，不管存在：snapshot 被刪掉之後，同一個（父, 名稱）再 lookup
/// 仍要回 NotFound，不能因為表裡記過就回舊的 ino。client 底下一個 snapshot
/// 都不剩時，client 本身也一樣。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn vanished_snapshot_is_not_found_after_lookup() {
    let (t, _big) = setup().await;
    let core = t.fs_core().await;
    let ts = timestamps(&core).await.remove(0);

    let client_ino = core.lookup(1, CLIENT.as_bytes()).await.unwrap().ino;
    core.lookup(client_ino, ts.as_bytes()).await.unwrap();
    let count = core.inode_count();

    // 測試 repo 不寫 `.r1` 副本，刪主體就是整個 snapshot 不見
    let key = format!("{}/{}/{}", kist_format::keys::SNAPSHOTS_PREFIX, CLIENT, ts);
    t.backend.delete(&key).await.unwrap();

    assert_eq!(
        core.lookup(client_ino, ts.as_bytes()).await.unwrap_err(),
        kist_mount::FsError::NotFound,
        "被刪掉的 snapshot 要回 NotFound"
    );
    assert_eq!(
        core.lookup(1, CLIENT.as_bytes()).await.unwrap_err(),
        kist_mount::FsError::NotFound,
        "沒有 snapshot 的 client 要回 NotFound"
    );
    assert_eq!(core.inode_count(), count);
}

/// snapshot lookup 時讀 root tree 失敗（暫時性），corefs 退回目錄形態：檔案來源
/// 顯示成目錄、`/` 攤平顯示成空的。這個 view 不能記進去重表——否則同一個
/// （client, ts）整個 session 都回這個錯的形態，錯誤消失了也好不了、只能重新掛載
/// （ADR 019 A6 複核）。退回形態的 lookup 用 1 秒 TTL，kernel 很快就會重查。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn degraded_snapshot_view_is_not_pinned() {
    let t = TestRepo::new().await;
    let src = t.dir.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("f.txt"), b"file source").unwrap();
    // 檔案來源：正常情況下葉子就是 f.txt 本身（檔案）
    t.backup(&src.join("f.txt")).await;
    let tmpname = t
        .dir
        .path()
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();

    let core = t.fs_core().await;
    let ts = timestamps(&core).await.remove(0);
    let client_ino = core.lookup(1, CLIENT.as_bytes()).await.unwrap().ino;

    // snapshot 目錄 → tmp → <tmpname> → src → f.txt
    async fn leaf(core: &FsCore, snap_ino: u64, tmpname: &str) -> kist_mount::corefs::Lookup {
        let mut cur = snap_ino;
        for p in ["tmp", tmpname, "src"] {
            cur = core.lookup(cur, p.as_bytes()).await.unwrap().ino;
        }
        core.lookup(cur, b"f.txt").await.unwrap()
    }

    // tree 物件暫時讀不到（模擬後端的暫時性錯誤）時 lookup snapshot
    let repo = t.dir.path().join("repo");
    std::fs::rename(repo.join("trees"), repo.join("trees.away")).unwrap();
    let first = core.lookup(client_ino, ts.as_bytes()).await;
    std::fs::rename(repo.join("trees.away"), repo.join("trees")).unwrap();
    let first = first.unwrap();
    // 確認真的走到退回形態（不然這個測試什麼都沒驗）
    assert_eq!(leaf(&core, first.ino, &tmpname).await.attr.kind, Kind::Dir);

    // 錯誤消失後重查：要拿到真正的形態——f.txt 是檔案、讀得到內容
    let second = core.lookup(client_ino, ts.as_bytes()).await.unwrap();
    let f = leaf(&core, second.ino, &tmpname).await;
    assert_eq!(f.attr.kind, Kind::File, "錯誤消失後重查要回真正的形態");
    assert_eq!(core.read_file(f.ino, 0, 64).await.unwrap(), b"file source");
    assert_ne!(second.ino, first.ino, "退回形態的 view 不能被去重表記住");

    // 退回形態：entry 與 attr 都是 1 秒 TTL；正常的是 immutable
    assert_eq!(first.ttl, kist_mount::corefs::VOLATILE_TTL);
    assert_eq!(
        core.getattr(first.ino).unwrap().1,
        kist_mount::corefs::VOLATILE_TTL
    );
    assert_eq!(second.ttl, kist_mount::corefs::IMMUTABLE_TTL);

    // 正常的 view 照樣去重：再查回同一個 ino、表不長大
    let count = core.inode_count();
    assert_eq!(
        core.lookup(client_ino, ts.as_bytes()).await.unwrap().ino,
        second.ino
    );
    assert_eq!(core.inode_count(), count);
}
