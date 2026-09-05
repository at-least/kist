//! check 的行為：正常 repo 無錯；人為破壞要被抓到。

mod common;

use common::*;
use kist_core::CheckOptions;

async fn repo_with_data() -> (TestRepo, kist_core::Repository) {
    let t = TestRepo::new().await;
    let src = t.dir.path().join("src");
    make_source(&src);
    let repo = t.open().await;
    repo.backup(&[src], backup_options()).await.unwrap();
    (t, repo)
}

fn some_pack(t: &TestRepo) -> std::path::PathBuf {
    let mut packs = walk_files(&t.repo_path().join("packs"));
    packs.sort();
    packs.remove(0)
}

#[tokio::test]
async fn clean_repo_passes_both_modes() {
    let (_t, repo) = repo_with_data().await;
    let quick = repo.check(CheckOptions { read_data: false, repair: false }).await.unwrap();
    assert!(quick.errors.is_empty(), "{:?}", quick.errors);
    assert!(quick.packs > 0 && quick.snapshots == 1 && quick.chunks > 0);
    let full = repo.check(CheckOptions { read_data: true, repair: false }).await.unwrap();
    assert!(full.errors.is_empty(), "{:?}", full.errors);
}

#[tokio::test]
async fn flipped_byte_in_pack_is_detected_by_read_data() {
    let (t, repo) = repo_with_data().await;
    let pack = some_pack(&t);
    let mut bytes = std::fs::read(&pack).unwrap();
    bytes[100] ^= 0x01;
    std::fs::write(&pack, bytes).unwrap();

    let report = repo.check(CheckOptions { read_data: true, repair: false }).await.unwrap();
    let name = pack.file_name().unwrap().to_str().unwrap();
    assert!(
        report.errors.iter().any(|e| e.contains(name)),
        "錯誤訊息應指名被破壞的 pack：{:?}",
        report.errors
    );
}

#[tokio::test]
async fn truncated_pack_is_detected_without_reading_data() {
    let (t, repo) = repo_with_data().await;
    let pack = some_pack(&t);
    let bytes = std::fs::read(&pack).unwrap();
    std::fs::write(&pack, &bytes[..bytes.len() - 10]).unwrap();

    let report = repo.check(CheckOptions { read_data: false, repair: false }).await.unwrap();
    let name = pack.file_name().unwrap().to_str().unwrap();
    assert!(
        report.errors.iter().any(|e| e.contains(name)),
        "{:?}",
        report.errors
    );
}

#[tokio::test]
async fn missing_pack_is_detected() {
    let (t, repo) = repo_with_data().await;
    let pack = some_pack(&t);
    std::fs::remove_file(&pack).unwrap();
    let report = repo.check(CheckOptions { read_data: false, repair: false }).await.unwrap();
    let name = pack.file_name().unwrap().to_str().unwrap();
    assert!(
        report.errors.iter().any(|e| e.contains(name)),
        "{:?}",
        report.errors
    );
}

#[tokio::test]
async fn missing_tree_is_detected() {
    let (t, repo) = repo_with_data().await;
    let mut trees = walk_files(&t.repo_path().join("trees"));
    trees.sort();
    let tree = trees.remove(0);
    std::fs::remove_file(&tree).unwrap();
    let report = repo.check(CheckOptions { read_data: false, repair: false }).await.unwrap();
    let name = tree.file_name().unwrap().to_str().unwrap();
    assert!(
        report.errors.iter().any(|e| e.contains(name)),
        "{:?}",
        report.errors
    );
}

/// 把物件 A 的檔案複製到物件 B 的名稱上：解密會成功（同 key、同種類），
/// 只有「名稱 = 密文 hash」的驗證能抓到。
fn swap_objects(dir: &std::path::Path) -> (String, String) {
    swap_objects_excluding(dir, &[])
}

/// 同上，但 `exclude`（檔名）不當受害者——例如根 tree，壞了就什麼都還原不了，測不出「其餘照常」。
fn swap_objects_excluding(dir: &std::path::Path, exclude: &[String]) -> (String, String) {
    let mut files = walk_files(dir);
    files.retain(|p| {
        p.is_file()
            && !exclude
                .iter()
                .any(|x| p.file_name().unwrap().to_str().unwrap() == x)
    });
    files.sort();
    let (a, b) = (files[0].clone(), files[1].clone());
    std::fs::copy(&a, &b).unwrap();
    (
        a.file_name().unwrap().to_str().unwrap().to_owned(),
        b.file_name().unwrap().to_str().unwrap().to_owned(),
    )
}

#[tokio::test]
async fn tree_copied_over_another_tree_is_detected() {
    let (t, repo) = repo_with_data().await;
    let key = repo.resolve_snapshot("latest").await.unwrap();
    let root = repo.read_snapshot_by_key(&key).await.unwrap().root.to_hex();
    let (_a, b) = swap_objects_excluding(&t.repo_path().join("trees"), &[root]);
    let report = repo.check(CheckOptions { read_data: false, repair: false }).await.unwrap();
    // v2：tree 以自己的 ID 當 AAD 密封，互拷的檔案在解密（AEAD 驗證）就失敗
    assert!(
        report
            .errors
            .iter()
            .any(|e| e.contains(&b) && e.contains("authentication failed")),
        "{:?}",
        report.errors
    );
    // 壞掉的是某個子目錄的 tree：那個目錄失敗、其餘照常還原，錯誤指名該 tree
    let summary = repo
        .restore(
            &key,
            &t.dir.path().join("out"),
            kist_core::RestoreOptions::default(),
        )
        .await
        .unwrap();
    assert!(summary.errors.iter().any(|e| e.contains(&b)), "{summary:?}");
}

/// v2 的 tree 名稱 = 明文的 keyed hash（AAD = 名稱）。AAD 相符、解密成功，
/// 但內容 hash 不是它名稱的 tree（偽造或寫入端出錯）必須被名稱檢查抓到。
#[tokio::test]
async fn tree_whose_content_does_not_match_its_name_is_detected() {
    let (t, repo) = repo_with_data().await;
    let key = repo.resolve_snapshot("latest").await.unwrap();
    let root = repo.read_snapshot_by_key(&key).await.unwrap().root.to_hex();
    let mut trees = walk_files(&t.repo_path().join("trees"));
    trees.retain(|p| p.is_file() && p.file_name().unwrap().to_str() != Some(&root));
    trees.sort();
    let (a, b) = (trees[0].clone(), trees[1].clone());
    let id_a = kist_format::TreeId::from_hex(a.file_name().unwrap().to_str().unwrap()).unwrap();
    let id_b = kist_format::TreeId::from_hex(b.file_name().unwrap().to_str().unwrap()).unwrap();
    // 把 a 的明文以 b 的名稱（AAD）重新密封、蓋到 b 的檔名上：解密會成功
    let plain_a = repo
        .keys()
        .open_tree(&id_a, &std::fs::read(&a).unwrap())
        .unwrap();
    let forged = repo.keys().seal_tree(&id_b, &plain_a).unwrap();
    std::fs::write(&b, forged).unwrap();

    let report = repo.check(CheckOptions { read_data: false, repair: false }).await.unwrap();
    let name = b.file_name().unwrap().to_str().unwrap();
    assert!(
        report
            .errors
            .iter()
            .any(|e| e.contains(name) && e.contains("name")),
        "{:?}",
        report.errors
    );
}

#[tokio::test]
async fn index_copied_over_another_index_is_detected() {
    let (t, repo) = repo_with_data().await;
    // 第二次 backup 有新 chunk 才會有第二個 index blob
    let src = t.dir.path().join("src");
    std::fs::write(src.join("extra.bin"), random_bytes(99, 50 * 1024)).unwrap();
    repo.backup(std::slice::from_ref(&src), backup_options())
        .await
        .unwrap();
    assert_eq!(t.count("indexes"), 2);
    let (_a, b) = swap_objects(&t.repo_path().join("indexes"));
    let report = repo.check(CheckOptions { read_data: false, repair: false }).await.unwrap();
    assert!(
        report.errors.iter().any(|e| e.contains(&b)),
        "{:?}",
        report.errors
    );
    assert!(
        repo.load_index().await.is_err(),
        "load_index 不該接受名稱不符的 index"
    );
}

#[tokio::test]
async fn snapshot_copied_over_another_snapshot_is_detected() {
    let (t, repo) = repo_with_data().await;
    let src = t.dir.path().join("src");
    repo.backup(std::slice::from_ref(&src), backup_options())
        .await
        .unwrap();
    let client_dir = std::fs::read_dir(t.repo_path().join("snapshots"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let (_a, b) = swap_objects(&client_dir);
    let report = repo.check(CheckOptions { read_data: false, repair: false }).await.unwrap();
    assert!(
        report.errors.iter().any(|e| e.contains(&b)),
        "{:?}",
        report.errors
    );
    let key = repo.resolve_snapshot(&b).await.unwrap();
    assert!(matches!(
        repo.read_snapshot_by_key(&key).await,
        Err(kist_core::CoreError::Corrupt { .. })
    ));
}

/// parity：backup --parity 寫 sidecar、破壞後 check --repair 修好、
/// check 不帶 --repair 只回報不動手。
#[tokio::test]
async fn damaged_pack_is_repaired_from_parity() {
    let t = TestRepo::new().await;
    let src = t.dir.path().join("src");
    make_source(&src);
    let repo = t.open().await;
    let mut opts = backup_options();
    opts.parity = 2;
    repo.backup(std::slice::from_ref(&src), opts).await.unwrap();
    assert!(
        t.repo_path().join("parity").exists() && t.count("parity") > 0,
        "backup --parity 應該寫出 sidecar"
    );

    let pack = some_pack(&t);
    let name = pack.file_name().unwrap().to_str().unwrap().to_owned();
    let mut bytes = std::fs::read(&pack).unwrap();
    bytes[100] ^= 0x01;
    std::fs::write(&pack, bytes).unwrap();

    // 沒有 --repair：回報、不動手
    let report = repo
        .check(CheckOptions {
            read_data: true,
            repair: false,
        })
        .await
        .unwrap();
    assert!(
        report.errors.iter().any(|e| e.contains(&name)),
        "{:?}",
        report.errors
    );
    assert!(report.repaired.is_empty());

    // 有 --repair：修好、再驗乾淨
    let report = repo
        .check(CheckOptions {
            read_data: true,
            repair: true,
        })
        .await
        .unwrap();
    assert!(
        report.repaired.iter().any(|k| k.contains(&name)),
        "{:?}",
        report
    );
    assert!(report.is_ok(), "{:?}", report.errors);

    // 修好的 pack 內容正確：還原逐 byte 相同
    let key = repo.resolve_snapshot("latest").await.unwrap();
    let out = t.dir.path().join("out");
    repo.restore(&key, &out, kist_core::RestoreOptions::default())
        .await
        .unwrap();
    // restore 在 target 底下重建完整絕對路徑（src 在 /tmp 底下）
    let restored_root = out.join(src.strip_prefix("/").unwrap());
    for f in walk_files(&src)
        .into_iter()
        .filter(|f| std::fs::symlink_metadata(f).map(|m| m.is_file()).unwrap_or(false))
    {
        let rel = f.strip_prefix(&src).unwrap();
        let restored = restored_root.join(rel);
        assert_eq!(
            std::fs::read(&restored).unwrap(),
            std::fs::read(&f).unwrap(),
            "{rel:?}"
        );
    }
}

/// 沒有 parity 的 repo：check --repair 對損壞的 pack 只能回報修不了。
#[tokio::test]
async fn repair_without_parity_reports_unrepairable() {
    let (t, repo) = repo_with_data().await;
    let pack = some_pack(&t);
    let name = pack.file_name().unwrap().to_str().unwrap().to_owned();
    let mut bytes = std::fs::read(&pack).unwrap();
    bytes[100] ^= 0x01;
    std::fs::write(&pack, bytes).unwrap();

    let report = repo
        .check(CheckOptions {
            read_data: true,
            repair: true,
        })
        .await
        .unwrap();
    assert!(!report.is_ok(), "{:?}", report.errors);
    assert!(
        report.unrepairable.iter().any(|k| k.contains(&name)),
        "{:?}",
        report
    );
    assert!(report.repaired.is_empty());
}

/// 超過 m 片的損壞：parity 修不了，但不會修出錯的內容。
#[tokio::test]
async fn repair_beyond_m_shards_fails_safely() {
    let t = TestRepo::new().await;
    let src = t.dir.path().join("src");
    make_source(&src);
    let repo = t.open().await;
    let mut opts = backup_options();
    opts.parity = 2;
    repo.backup(&[src], opts).await.unwrap();

    let pack = some_pack(&t);
    let name = pack.file_name().unwrap().to_str().unwrap().to_owned();
    let mut bytes = std::fs::read(&pack).unwrap();
    let shard = bytes.len().div_ceil(16);
    for i in 0..3 {
        bytes[i * shard] ^= 0x01;
    }
    std::fs::write(&pack, bytes).unwrap();

    let report = repo
        .check(CheckOptions {
            read_data: true,
            repair: true,
        })
        .await
        .unwrap();
    assert!(
        report.unrepairable.iter().any(|k| k.contains(&name)),
        "3 片損壞、m=2：{:?}",
        report
    );
    assert!(report.repaired.is_empty());
}

/// prune 刪 pack 時 parity 一起刪，不留孤兒。
#[tokio::test]
async fn prune_removes_parity_sidecars() {
    use kist_core::{ForgetOptions, PruneOptions, RetentionPolicy};
    use std::time::Duration as StdDuration;
    use time::OffsetDateTime;

    const GRACE: StdDuration = StdDuration::from_secs(72 * 3600);
    const INACTIVE: StdDuration = StdDuration::from_secs(30 * 24 * 3600);
    let t = TestRepo::new().await;
    let src = t.dir.path().join("src");
    make_source(&src);
    let repo = t.open().await;
    let started = OffsetDateTime::now_utc();
    let mut opts = backup_options();
    opts.parity = 2;
    opts.now = Some(started);
    repo.backup(&[src], opts).await.unwrap();
    assert!(t.count("parity") > 0);

    // forget 掉唯一的 snapshot，兩階段 prune（標記 → grace 後刪）。
    // 本機後端的 modified 截秒（format.md §11.5）：backup 物件與 GC 標記若在同一
    // 真實秒內，prune 的「標記後被重寫」防護會把物件復活。等過秒再標記。
    std::thread::sleep(std::time::Duration::from_millis(1100));
    let key = repo.resolve_snapshot("latest").await.unwrap();
    repo.forget(ForgetOptions {
        snapshots: vec![key],
        policy: RetentionPolicy::default(),
        dry_run: false,
    })
    .await
    .unwrap();
    let p1 = repo
        .prune(PruneOptions {
            clock_skew: std::time::Duration::ZERO,
            grace: GRACE,
            inactive_after: INACTIVE,
            repack_below_percent: 0,
            dry_run: false,
            now: Some(started + GRACE + StdDuration::from_secs(3600)),
        })
        .await
        .unwrap();
    assert!(p1.marked > 0, "{p1:?}");
    let p2 = repo.prune(PruneOptions {
        clock_skew: std::time::Duration::ZERO,
        grace: GRACE,
        inactive_after: INACTIVE,
        repack_below_percent: 0,
        dry_run: false,
        now: Some(started + GRACE + StdDuration::from_secs(2 * 3600)),
    })
    .await
    .unwrap();
    assert_eq!(t.count("packs"), 0);
    assert_eq!(t.count("parity"), 0, "pack 刪了 parity 也該刪");
}
