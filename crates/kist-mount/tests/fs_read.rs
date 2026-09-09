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
            path: b"/".to_vec().into(),
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
        "\"/\": 子樹攤平到頂層"
    );

    // 子目錄真的可以走進去（空的）
    let bin = core.lookup(snap_ino, b"bin").await.unwrap();
    assert_eq!(bin.attr.kind, Kind::Dir);
    let fh = core.opendir(bin.ino).await.unwrap();
    assert!(core.readdir(fh, 0, 10).unwrap().is_empty());
    core.releasedir(fh);
}
