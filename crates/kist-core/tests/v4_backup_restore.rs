//! v4 backup → restore 的端到端行為：改過的檔、FIFO、mtime 變了但內容沒變、同大小同 mtime 不同內容、跨段的
//! parent 快速路徑、stats 的口徑、深度上限。搬自 v3 的 `backup_restore.rs` 裡還沒有 v4 對應的 8 個（v4 的走訪
//! 是 v3 的複製；其餘 8 個在第五段搬進 v4_restore、v4_backup、v4_read）。

#[path = "v4_common/mod.rs"]
mod v4_common;

use kist_core::RestoreOptions;
use kist_format::v4::meta::Item;
use v4_common::*;

#[tokio::test]
async fn modified_file_is_picked_up_and_only_it_is_new() {
    let t = TestRepo::new().await;
    let src = t.dir.path().join("src");
    make_source(&src);
    let repo = t.open().await;
    repo.backup(std::slice::from_ref(&src), backup_options())
        .await
        .unwrap();

    // 改一個檔（內容 + mtime）、新增一個檔、刪一個檔
    std::fs::write(src.join("small.txt"), b"changed\n").unwrap();
    std::fs::write(src.join("sub/new.txt"), b"new").unwrap();
    std::fs::remove_file(src.join("zeros.bin")).unwrap();

    let second = repo
        .backup(std::slice::from_ref(&src), backup_options())
        .await
        .unwrap();
    assert!(
        second.report.chunks_new >= 2 && second.report.chunks_new <= 4,
        "{:?}",
        second.report
    );

    let target = t.dir.path().join("out");
    repo.restore(&skey(&second.snapshot), &target, RestoreOptions::default())
        .await
        .unwrap();
    let restored = target.join(src.strip_prefix("/").unwrap_or(&src));
    assert_same_tree(&src, &restored);
}

/// FIFO 這類非正規檔案必須跳過並記一筆略過；
/// 不能把 open（FIFO 無寫端會無限阻塞）帶進 backup——那會讓整個備份掛死。
#[cfg(unix)]
#[tokio::test]
async fn fifo_in_source_tree_is_skipped_not_read() {
    let t = TestRepo::new().await;
    let src = t.dir.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("f.txt"), b"hello").unwrap();
    let fifo = src.join("pipe");
    let ok = std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .expect("mkfifo")
        .success();
    assert!(ok, "mkfifo failed");

    let repo = t.open().await;
    let summary = repo
        .backup(std::slice::from_ref(&src), backup_options())
        .await
        .unwrap();
    assert_eq!(summary.report.errors, 1, "{:?}", summary.report);
    assert_eq!(summary.stats.files, 1, "{:?}", summary.stats);

    let target = t.dir.path().join("out");
    repo.restore(&skey(&summary.snapshot), &target, RestoreOptions::default())
        .await
        .unwrap();
    let restored = target.join(src.strip_prefix("/").unwrap_or(&src));
    assert_eq!(std::fs::read(restored.join("f.txt")).unwrap(), b"hello");
    assert!(!restored.join("pipe").exists(), "FIFO 不該進 snapshot");
}

/// 檔案 root 明確指到非正規檔：直接失敗並指名原因，不掛死。
#[cfg(unix)]
#[tokio::test]
async fn fifo_as_file_root_fails_loudly() {
    let t = TestRepo::new().await;
    let fifo = t.dir.path().join("pipe");
    let ok = std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .expect("mkfifo")
        .success();
    assert!(ok, "mkfifo failed");

    let repo = t.open().await;
    let err = repo
        .backup(std::slice::from_ref(&fifo), backup_options())
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("not a regular file"),
        "錯誤要指名非正規檔：{err}"
    );
}

#[tokio::test]
async fn touching_mtime_without_changing_content_does_not_write_chunks() {
    // parent 快速路徑失效（mtime 變了）也只是重讀檔案，chunk 去重仍然不寫新東西。
    let t = TestRepo::new().await;
    let src = t.dir.path().join("src");
    make_source(&src);
    let repo = t.open().await;
    repo.backup(std::slice::from_ref(&src), backup_options())
        .await
        .unwrap();

    let later = filetime::FileTime::from_unix_time(2_000_000_000, 0);
    filetime::set_file_mtime(src.join("random.bin"), later).unwrap();
    let second = repo
        .backup(std::slice::from_ref(&src), backup_options())
        .await
        .unwrap();
    assert_eq!(second.report.chunks_new, 0);
    assert_eq!(second.report.packs_new, 0);
}

/// `cp -p` / `rsync -a` 會保留 mtime；內容不同但大小相同時，只比 size + mtime 會漏掉。
/// ctime 是 kernel 在寫入時更新、使用者改不了的，所以能抓到。
#[cfg(unix)]
#[tokio::test]
async fn same_size_and_mtime_but_different_content_is_detected() {
    let t = TestRepo::new().await;
    let src = t.dir.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    let file = src.join("data.bin");
    std::fs::write(&file, random_bytes(11, 100 * 1024)).unwrap();
    let mtime = filetime::FileTime::from_unix_time(1_700_000_000, 0);
    filetime::set_file_mtime(&file, mtime).unwrap();

    let repo = t.open().await;
    repo.backup(std::slice::from_ref(&src), backup_options())
        .await
        .unwrap();

    // 換掉內容（同大小），再把 mtime 設回去
    std::fs::write(&file, random_bytes(12, 100 * 1024)).unwrap();
    filetime::set_file_mtime(&file, mtime).unwrap();

    let second = repo
        .backup(std::slice::from_ref(&src), backup_options())
        .await
        .unwrap();
    assert!(
        second.report.chunks_new > 0,
        "內容變了卻沒有新 chunk：快速路徑誤判 {:?}",
        second.report
    );
    let target = t.dir.path().join("out");
    repo.restore(&skey(&second.snapshot), &target, RestoreOptions::default())
        .await
        .unwrap();
    let restored = target.join(src.strip_prefix("/").unwrap_or(&src));
    assert_same_tree(&src, &restored);
}

/// 大目錄切成多段（§7：依 keyed hash 切段）。第二次 backup 的 parent 快速路徑必須**跨段**還能用：沒改的檔案
/// 直接沿用、改過的重讀、restore 仍逐 byte 相同。名稱依這個 repo 的 K_cut 挑，保證至少三段（v3 用固定的
/// 10,050 個名稱；v4 的金鑰是亂數，固定的名稱可能不切）。
#[tokio::test]
async fn parent_reuse_across_tree_segments_in_one_dir() {
    let t = TestRepo::new().await;
    let src = t.dir.path().join("big");
    std::fs::create_dir_all(&src).unwrap();
    let repo = t.open().await;
    let mut names = names_with_cuts(&repo, 3);
    while names.len() < 10_050 {
        names.push(format!("f{:06}", names.len()));
    }
    for (i, n) in names.iter().enumerate() {
        std::fs::write(src.join(n), format!("content {i}\n")).unwrap();
    }
    let first = repo
        .backup(std::slice::from_ref(&src), backup_options())
        .await
        .unwrap();
    let reader = repo.reader(None).await.unwrap();
    let snap = repo.read_snapshot(&skey(&first.snapshot)).await.unwrap();
    let roots = reader.roots(&snap.roots).await.unwrap();
    let Item::Dir(dir) = reader.item(&roots[0].tree.unwrap()).await.unwrap() else {
        panic!(
            "{} names chosen to cut should need more than one segment",
            names.len()
        );
    };
    assert!(dir.segs.len() >= 3, "{} segments", dir.segs.len());

    // 改掉一批跨段界的檔案（每段的頭、尾、中間都有）；fs::write 會更新
    // mtime，parent 快速路徑因此失效，必須重讀。
    let mut changed = 0u64;
    for (i, n) in names.iter().enumerate().step_by(97) {
        std::fs::write(src.join(n), format!("CHANGED {i}\n")).unwrap();
        changed += 1;
    }

    let second = repo
        .backup(std::slice::from_ref(&src), backup_options())
        .await
        .unwrap();
    assert_ne!(first.snapshot, second.snapshot);
    assert_eq!(second.parent.as_deref(), Some(first.snapshot.as_str()));
    assert_eq!(
        second.report.files_reused,
        names.len() as u64 - changed,
        "跨段界之外沒改的檔案都該走 parent 快速路徑"
    );
    assert_eq!(
        second.report.chunks_new, changed,
        "改過的檔案各有一個新內容 chunk"
    );

    let target = t.dir.path().join("out2");
    repo.restore(&skey(&second.snapshot), &target, RestoreOptions::default())
        .await
        .unwrap();
    let restored = target.join(src.strip_prefix("/").unwrap_or(&src));
    assert_same_tree(&src, &restored);
}

/// stats 的口徑（format-v4.md §9）：`dirs` 是目錄 entry 數（root 本身是 path、不是 entry）；`files` 依名字計
/// （hard link 各算一個）；`bytes` 同一份 hard link 內容只算一次。
#[cfg(unix)]
#[tokio::test]
async fn stats_count_dirs_without_root_tree_and_hard_link_bytes_once() {
    let t = TestRepo::new().await;
    let src = t.dir.path().join("src");
    std::fs::create_dir_all(src.join("sub")).unwrap();
    std::fs::create_dir_all(src.join("empty")).unwrap();
    let payload = random_bytes(7, 300 * 1024);
    std::fs::write(src.join("a.bin"), &payload).unwrap();
    std::fs::hard_link(src.join("a.bin"), src.join("b.bin")).unwrap();
    std::fs::write(src.join("sub").join("c.txt"), b"c").unwrap();
    std::os::unix::fs::symlink("a.bin", src.join("link")).unwrap();

    let repo = t.open().await;
    let s = repo
        .backup(std::slice::from_ref(&src), backup_options())
        .await
        .unwrap();
    assert_eq!(
        s.stats.dirs, 2,
        "sub, empty（root 本身不算）：{:?}",
        s.stats
    );
    assert_eq!(
        s.stats.files, 3,
        "a.bin、b.bin（hard link）、sub/c.txt：{:?}",
        s.stats
    );
    assert_eq!(s.stats.symlinks, 1, "{:?}", s.stats);
    assert_eq!(
        s.stats.bytes,
        payload.len() as u64 + 1,
        "hard link 內容只算一次：{:?}",
        s.stats
    );
}

/// 寫入端與讀取端同一把深度尺（`MAX_TREE_DEPTH`）：讀取端把超過
/// `MAX_TREE_DEPTH` 的樹當敵意 repo 拒收，backup 就不能把「合法但過深」
/// 的來源目錄寫進去——寫得出、還原不回＝自造損壞。過深的子目錄跳過並
/// 記帳（與讀不到的項目同一套帳），backup 的走訪遞迴也因此有界。
#[tokio::test]
async fn backup_refuses_depth_beyond_the_read_side_limit() {
    // 修復前這個測試在 260 層就 abort（Box::pin 遞迴每一層的 debug frame
    // 很肥，2 MiB 的測試執行緒裝不下）：深走訪要在自己的大堆疊執行緒上
    // 跑，斷言的對象是語意（截斷在讀取端上限），不是堆疊極限本身。
    std::thread::Builder::new()
        .stack_size(64 << 20)
        .spawn(run_depth_limit_body)
        .unwrap()
        .join()
        .unwrap();
}

fn run_depth_limit_body() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let t = TestRepo::new().await;
        let src = t.dir.path().join("src");
        let mut deep = src.clone();
        std::fs::create_dir_all(&deep).unwrap();
        // 260 層巢狀目錄：legal on ext4（PATH_MAX 內），但超出讀取端的 256。
        for i in 0..260 {
            deep.push(format!("d{i}"));
            std::fs::create_dir_all(&deep).unwrap();
        }
        std::fs::write(deep.join("leaf.txt"), b"deep").unwrap();
        std::fs::write(src.join("top.txt"), b"top").unwrap();

        let repo = t.open().await;
        let summary = repo
            .backup(std::slice::from_ref(&src), backup_options())
            .await
            .unwrap();
        assert!(
            summary.report.errors >= 1,
            "過深的目錄必須記帳跳過：{:?}",
            summary.report
        );

        // repo 裡寫出的深度必須在讀取端上限內：restore 整顆成功。
        let target = t.dir.path().join("out");
        repo.restore(&skey(&summary.snapshot), &target, RestoreOptions::default())
            .await
            .unwrap();
        // restore 會在 target 底下重建完整的絕對路徑。
        let restored = target.join(src.strip_prefix("/").unwrap_or(&src));
        assert!(restored.join("top.txt").is_file(), "淺層內容照常還原");
        let mut limit_ok = restored.clone();
        for i in 0..255 {
            limit_ok.push(format!("d{i}")); // d0..d254 = 第 2..=256 層
        }
        assert!(
            limit_ok.is_dir(),
            "上限內的深度照常還原：{}",
            limit_ok.display()
        );
        assert!(
            !limit_ok.join("d255").exists(),
            "第 257 層起必須被跳過，不能寫出讀取端拒收的樹"
        );
    })
}
