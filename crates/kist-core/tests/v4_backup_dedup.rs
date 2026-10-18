//! v4 backup 的沿用（format-v4.md §11.2 的 B2）與請求數（ADR 026 需求 R5）：
//! - 別台寫的副本第一次沿用前取回驗證，結果記在本地快取，之後不再下載；
//! - 驗證不過的副本不沿用、重寫；
//! - 沒變的 backup 的請求數與目錄數無關。

#[path = "v4_common/mod.rs"]
mod v4_common;

use v4_common::*;

fn copy_of_sample(h: &HookedRepo, name: &str) -> std::path::PathBuf {
    let p = h.dir.path().join(name);
    sample_source(&p);
    p
}

#[tokio::test]
async fn another_clients_copy_is_verified_once_and_then_trusted_from_the_cache() {
    let h = HookedRepo::new(init_options(0)).await;
    let mine = h.open_as("c1").await;
    mine.backup(&[copy_of_sample(&h, "one")], opts(1))
        .await
        .unwrap();
    // client 1 的 pack 進了 map：B4 不必讀 trailer，下面的 range read 只會是 B2 的取回驗證
    let records = index_everything(&mine, &std::collections::BTreeMap::new()).await;
    write_generation(&mine, records, false).await;

    // 第一次：別台（client 1）的副本要先取回驗證
    let theirs = h.open_as("c2").await;
    h.store.reset();
    let first = theirs
        .backup(&[copy_of_sample(&h, "two")], opts(2))
        .await
        .unwrap();
    let r = first.report;
    assert_eq!(r.chunks_new, 0, "{r:?}");
    assert!(
        r.chunks_reused > 0 && r.verified >= r.chunks_reused,
        "{r:?}"
    );
    assert!(h.store.counts().get("range").copied().unwrap_or(0) > 0);
    assert!(read_tree(&theirs, &first.snapshot)
        .await
        .values()
        .any(|n| *n == Node::File(data("big", 300 * 1024))));

    // 再一份同樣內容的來源（不同路徑、沒有 parent）：快取記得驗證過，不再下載
    h.store.reset();
    let second = theirs
        .backup(&[copy_of_sample(&h, "three")], opts(2))
        .await
        .unwrap();
    let r = second.report;
    assert_eq!((r.chunks_new, r.verified), (0, 0), "{r:?}");
    assert!(r.chunks_reused > 0);
    assert_eq!(
        h.store.counts().get("range"),
        None,
        "{:?}",
        h.store.counts()
    );

    // 沒有快取的同一台 client 不記得：又要驗證一次
    let forgetful = kist_core::v4::Repo::open(h.backend.clone(), PASSWORD, None)
        .await
        .unwrap();
    let third = forgetful
        .backup(&[copy_of_sample(&h, "four")], opts(2))
        .await
        .unwrap();
    assert!(third.report.verified > 0, "{:?}", third.report);
}

/// B2：取回驗證失敗的副本不沿用，這台 client 自己重寫一份；還原讀到的是自己這份。
#[tokio::test]
async fn a_copy_that_fails_verification_is_not_reused() {
    let h = HookedRepo::new(init_options(0)).await;
    let mine = h.open_as("c1").await;
    mine.backup(&[copy_of_sample(&h, "one")], opts(1))
        .await
        .unwrap();
    // 竄改 client 1 的每個資料 pack：每一項的密文都翻一個 byte（名稱不變）
    let reader = mine.reader(None).await.unwrap();
    for key in h.keys(&format!("packs/{}", client(1))).await {
        let parsed = kist_format::v4::keys::Key::parse(&key).unwrap();
        let trailer = reader.trailer(&parsed).await.unwrap();
        let mut bytes = h.backend.get(&key).await.unwrap();
        for e in &trailer.entries {
            bytes[(e.o + 30) as usize] ^= 1;
        }
        h.overwrite(&key, bytes).await;
    }
    let theirs = h.open_as("c2").await;
    let s = theirs
        .backup(&[copy_of_sample(&h, "two")], opts(2))
        .await
        .unwrap();
    assert_eq!(s.report.chunks_reused, 0, "{:?}", s.report);
    assert!(s.report.chunks_new > 0);
    assert!(read_tree(&theirs, &s.snapshot)
        .await
        .values()
        .any(|n| *n == Node::File(data("big", 300 * 1024))));
}

/// 需求 R5：沒變的 backup 的請求數與目錄數無關（ADR 026 估計 6–11 個）。
#[tokio::test]
async fn an_unchanged_backup_costs_the_same_requests_for_10_or_1000_directories() {
    let mut seen = Vec::new();
    for n in [10, 1000] {
        let h = HookedRepo::new(init_options(0)).await;
        let src = h.dir.path().join("src");
        for i in 0..n {
            let d = src.join(format!("d{i:04}"));
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("f"), format!("file {i}")).unwrap();
        }
        let repo = h.open_as("c").await;
        repo.backup(std::slice::from_ref(&src), opts(1))
            .await
            .unwrap();
        h.store.reset();
        let again = repo.backup(&[src], opts(1)).await.unwrap();
        assert_eq!(again.report.packs_new + again.report.mpacks_new, 0);
        let counts = h.store.counts();
        eprintln!("{n} directories: {counts:?}");
        seen.push(counts);
    }
    assert_eq!(
        seen[0], seen[1],
        "requests depend on the number of directories"
    );
    let total: usize = seen[0].values().sum();
    assert!((6..=11).contains(&total), "{total} requests: {:?}", seen[0]);
}

/// 同上，但 pack 已經被 prune 收進 map：B4 不必讀 delta 裡的 pack 的 trailer，請求更少。
#[tokio::test]
async fn an_unchanged_backup_after_maintenance_needs_no_trailer() {
    let h = HookedRepo::new(init_options(0)).await;
    let src = h.dir.path().join("src");
    sample_source(&src);
    let repo = h.open_as("c").await;
    repo.backup(std::slice::from_ref(&src), opts(1))
        .await
        .unwrap();
    let records = index_everything(&repo, &std::collections::BTreeMap::new()).await;
    write_generation(&repo, records, false).await;
    // 第一次讀這一代的分片會下載它（之後在本地快取），先跑一次
    repo.backup(std::slice::from_ref(&src), opts(1))
        .await
        .unwrap();
    h.store.reset();
    repo.backup(&[src], opts(1)).await.unwrap();
    let counts = h.store.counts();
    eprintln!("after maintenance: {counts:?}");
    assert_eq!(counts.get("range"), None, "{counts:?}");
    let total: usize = counts.values().sum();
    assert!((6..=11).contains(&total), "{total} requests: {counts:?}");
}

/// B2 的第一種可信：`by_prune` 的持有者（prune 自己寫出、讀回驗證過，在全零 client 的前綴下）不必取回驗證。
#[tokio::test]
async fn a_by_prune_holder_is_trusted_without_a_fetch() {
    use kist_core::v4::pack::PackWriter;
    use kist_format::v4::ids::ClientId;
    use kist_format::v4::keys::Key;
    use kist_format::v4::pack::Kind;

    let h = HookedRepo::new(init_options(0)).await;
    let repo = h.open_as("c").await;
    // 小於最小 chunk 的檔案就是一個 chunk
    let content = data("repacked", 3000);
    let keys = std::sync::Arc::clone(repo.keys());
    let mut w = PackWriter::new(
        std::sync::Arc::clone(&keys),
        Kind::Data,
        64 * 1024,
        64 * 1024,
    );
    w.add(keys.chunk_id(&content).into(), &content).unwrap();
    let p = w.finish().unwrap().unwrap();
    h.backend
        .put_if_absent(
            &Key::Pack {
                client: ClientId::MAINT,
                id: p.id,
            }
            .path(),
            p.bytes,
        )
        .await
        .unwrap();
    let records = index_everything(&repo, &std::collections::BTreeMap::new()).await;
    assert!(records.iter().any(|r| r.by_prune));
    write_generation(&repo, records, false).await;

    let src = h.dir.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("f"), &content).unwrap();
    h.store.reset();
    let s = repo.backup(&[src], opts(1)).await.unwrap();
    assert_eq!(
        (
            s.report.chunks_reused,
            s.report.chunks_new,
            s.report.verified
        ),
        (1, 0, 0),
        "{:?}",
        s.report
    );
    assert_eq!(
        h.store.counts().get("range"),
        None,
        "{:?}",
        h.store.counts()
    );
}
