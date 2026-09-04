//! 探針：兩個 index 寫入者（兩個交錯的 prune，或 prune 中途 rebuild-index）留下「index 裡有、儲存上沒有」
//! 的 pack（phantom），下一輪 prune 把 phantom 當正本、把真正的持有者標記然後刪掉。
#[path = "/home/newlix/github/at-least/kist-rs/crates/kist-core/tests/common/mod.rs"]
mod common;

use std::collections::HashSet;

use common::*;
use kist_core::{BackupOptions, CheckOptions, PruneOptions};
use kist_format::{keys, ObjectId};
use time::{Duration, OffsetDateTime};

const H: std::time::Duration = std::time::Duration::from_secs(3600);

fn client(id: u8, now: OffsetDateTime) -> BackupOptions {
    BackupOptions {
        client_id: [id; 16],
        hostname: format!("host{id}"),
        username: "tester".to_owned(),
        now: Some(now),
        gc_grace: 72 * H,
    }
}
fn prune_opts(now: OffsetDateTime) -> PruneOptions {
    PruneOptions { grace: 72 * H, inactive_after: 30 * 24 * H, repack_below_percent: 50, dry_run: false, now: Some(now) }
}
fn ids_under(t: &TestRepo, prefix: &str) -> HashSet<ObjectId> {
    let dir = t.repo_path().join(prefix);
    if !dir.is_dir() { return HashSet::new(); }
    std::fs::read_dir(dir).unwrap()
        .map(|e| keys::object_id_from_key(&e.unwrap().file_name().to_string_lossy()).unwrap())
        .collect()
}
fn stamp(t: &TestRepo, prefix: &str, when: OffsetDateTime) {
    let st = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_nanos(when.unix_timestamp_nanos() as u64);
    for p in walk_files(&t.repo_path().join(prefix)) {
        if p.is_file() { filetime::set_file_mtime(&p, filetime::FileTime::from_system_time(st)).unwrap(); }
    }
}
fn forget(repo: &kist_core::Repository, key: String) -> impl std::future::Future<Output = ()> + '_ {
    async move {
        repo.forget(kist_core::ForgetOptions { snapshots: vec![key], policy: Default::default(), dry_run: false }).await.unwrap();
    }
}

/// variant: "two-prunes" 或 "rebuild"。回傳是否示範出資料遺失（phantom 贏了 id 比較）。
async fn run(variant: &str, seed: u64) -> bool {
    println!("=== variant={variant} seed={seed} ===");
    let t = TestRepo::new().await;
    let repo = t.open().await;
    let src = t.dir.path().join("src");
    let src_e = t.dir.path().join("e");
    let src_g = t.dir.path().join("g");
    make_source(&src);
    std::fs::write(src.join("zz-big.bin"), random_bytes(77, 1200 * 1024)).unwrap();
    std::fs::create_dir_all(&src_e).unwrap();
    for i in 0..4 { std::fs::write(src_e.join(format!("e{i}.bin")), random_bytes(1000 + seed * 10 + i, 250 * 1024)).unwrap(); }
    std::fs::create_dir_all(&src_g).unwrap();
    std::fs::write(src_g.join("g.txt"), b"g").unwrap();
    let r = OffsetDateTime::now_utc();

    // r: c1 備份 src（小檔 + 大檔）與 e；c2 備份 g。然後把這些物件的 mtime 全部撥到 r-10d
    //（模擬「比 grace 老」），之後 prune 寫的標記與 backup 寫的新物件維持真實時間 ≈ r。
    let s1 = repo.backup(std::slice::from_ref(&src), client(1, r)).await.unwrap();
    let se1 = repo.backup(std::slice::from_ref(&src_e), client(1, r + Duration::hours(1))).await.unwrap();
    repo.backup(std::slice::from_ref(&src_g), client(2, r + Duration::minutes(30))).await.unwrap();
    for prefix in ["packs", "trees", "indexes"] { stamp(&t, prefix, r - Duration::days(10)); }
    let e_packs_before = ids_under(&t, "packs");
    // e 被 forget → e 的 pack 是垃圾；prune 1（r+1h）標記它們（標記 mtime = 真實時間 ≈ r）
    forget(&repo, se1.snapshot_key).await;
    let p1 = repo.prune(prune_opts(r + Duration::hours(1))).await.unwrap();
    println!("prune1 (r+1h): marked={} deleted={} repacked={}", p1.marked, p1.deleted, p1.repacked_packs);
    let x_set: HashSet<ObjectId> = ids_under(&t, "gc").intersection(&ids_under(&t, "packs")).copied().collect();
    assert!(!x_set.is_empty());
    // r+1d: c1 再備份 e（X 已標記 → 重寫進 W）；刪大檔再備份 src；forget s1 → 跨界 pack 成為 repack 候選
    let se2 = repo.backup(std::slice::from_ref(&src_e), client(1, r + Duration::days(1))).await.unwrap();
    assert!(se2.stats.chunks_new > 0);
    std::fs::remove_file(src.join("zz-big.bin")).unwrap();
    let s2 = repo.backup(std::slice::from_ref(&src), client(1, r + Duration::days(1) + Duration::hours(1))).await.unwrap();
    forget(&repo, s1.snapshot_key).await;
    let w_set: HashSet<ObjectId> = ids_under(&t, "packs").difference(&e_packs_before).copied().collect();
    println!("X (marked, e's old packs) = {}", x_set.len());
    println!("W (e rewritten at r+1d) = {}", w_set.len());

    let packs_before_race = ids_under(&t, "packs");
    match variant {
        "two-prunes" => {
            // r+2d: prune B 的 plan：X 的標記還在等（≈r+72h）；有 repack 候選 → 會寫 index
            let plan_b = repo.prune_plan(prune_opts(r + Duration::days(2))).await.unwrap();
            println!("B.plan: {:?}", plan_b.report());
            assert!(plan_b.report().repacked_packs >= 1, "B needs a repack so that it writes an index");
            assert_eq!(plan_b.report().deleted, 0);
            // r+2d+12h: c2 再備份 → 所有活躍 client 都在標記後有 snapshot
            repo.backup(std::slice::from_ref(&src_g), client(2, r + Duration::days(2) + Duration::hours(12))).await.unwrap();
            // r+3d+1h: prune A 的 plan：X 的標記過期、無人阻擋 → 要刪 X
            let plan_a = repo.prune_plan(prune_opts(r + Duration::days(3) + Duration::hours(1))).await.unwrap();
            println!("A.plan: {:?}", plan_a.report());
            // B 先執行（寫的 index 含 X），A 後執行（寫的 index 不含 X，supersedes 只涵蓋 A plan 時的 blob；刪 X）
            let rb = plan_b.execute().await.unwrap();
            println!("B.execute: new_packs={} deleted={} marked={}", rb.new_packs, rb.deleted, rb.marked);
            let ra = plan_a.execute().await.unwrap();
            println!("A.execute: new_packs={} deleted={} revived={} marked={}", ra.new_packs, ra.deleted, ra.revived, ra.marked);
            assert!(ra.deleted as usize >= x_set.len(), "A should have deleted X: {ra:?}");
        }
        "rebuild" => {
            repo.backup(std::slice::from_ref(&src_g), client(2, r + Duration::days(2) + Duration::hours(12))).await.unwrap();
            let plan_a = repo.prune_plan(prune_opts(r + Duration::days(3) + Duration::hours(1))).await.unwrap();
            println!("A.plan: {:?}", plan_a.report());
            let rb = repo.rebuild_index().await.unwrap();
            println!("rebuild-index during prune: packs={} superseded={}", rb.packs, rb.superseded);
            let ra = plan_a.execute().await.unwrap();
            println!("A.execute: new_packs={} deleted={} revived={} marked={}", ra.new_packs, ra.deleted, ra.revived, ra.marked);
            assert!(ra.deleted as usize >= x_set.len(), "A should have deleted X: {ra:?}");
        }
        _ => unreachable!(),
    }
    let packs_now = ids_under(&t, "packs");
    let deleted: HashSet<ObjectId> = packs_before_race.difference(&packs_now).copied().collect();
    assert!(x_set.is_subset(&deleted), "X should be gone");
    // phantom：index 裡還有 X
    let fresh = t.open().await;
    let idx = fresh.load_index().await.unwrap();
    let in_index: HashSet<ObjectId> = idx.packs().map(|(id, _)| *id).collect();
    let phantoms: Vec<ObjectId> = x_set.iter().filter(|x| in_index.contains(x)).copied().collect();
    println!("phantom packs (in effective index, not in storage): {}", phantoms.len());
    let c = fresh.check(CheckOptions { read_data: false }).await.unwrap();
    println!("check after race: {} error(s); first: {:?}", c.errors.len(), c.errors.first());
    assert!(!phantoms.is_empty(), "no phantom → scenario did not reproduce");
    assert!(c.errors.iter().any(|e| e.contains("pack is missing")));

    // 下一輪 prune：phantom 若 id 比 W 小就是正本 → W 被標記
    let p3 = repo.prune(prune_opts(r + Duration::days(4))).await.unwrap();
    println!("prune C (r+4d): marked={} deleted={} revived={} live_packs={}", p3.marked, p3.deleted, p3.revived, p3.live_packs);
    let marked_now = ids_under(&t, "gc");
    let w_marked: Vec<ObjectId> = w_set.iter().filter(|w| marked_now.contains(w)).copied().collect();
    println!("W packs (real holders of e's chunks) now marked as garbage: {}/{}", w_marked.len(), w_set.len());
    for w in &w_marked {
        let smaller: Vec<String> = phantoms.iter().filter(|x| *x < w).map(|x| x.to_string()[..8].to_owned()).collect();
        println!("  W {} marked; phantoms with smaller id: {:?}", &w.to_string()[..8], smaller);
    }
    assert!(w_marked.is_empty(), "a real holder got marked: {w_marked:?}");
    let idx_after_c = t.open().await.load_index().await.unwrap();
    let still: Vec<_> = phantoms.iter().filter(|x| idx_after_c.packs().any(|(p, _)| p == *x)).collect();
    println!("phantoms still in the index after prune C: {}", still.len());
    assert!(still.is_empty(), "phantoms must be dropped from the index");
    // 標記的 mtime 是真實時間（≈ r），所以下一輪就過期；真實世界要再等一個 grace，邏輯相同
    let p4 = repo.prune(prune_opts(r + Duration::days(8))).await.unwrap();
    println!("prune D (r+8d): deleted={} skipped={:?}", p4.deleted, p4.skipped);
    let packs_final = ids_under(&t, "packs");
    let w_gone: Vec<_> = w_marked.iter().filter(|w| !packs_final.contains(w)).collect();
    println!("W packs deleted: {}/{}", w_gone.len(), w_marked.len());
    let fresh = t.open().await;
    let c = fresh.check(CheckOptions { read_data: true }).await.unwrap();
    println!("check --read-data: {} error(s); e.g. {:?}", c.errors.len(), c.errors.first());
    let out = t.dir.path().join("out");
    let rr = fresh.restore(&se2.snapshot_key, &out, Default::default()).await.unwrap();
    println!("restore of snapshot {} (e, committed r+1d): {} error(s); e.g. {:?}", se2.snapshot_key, rr.errors.len(), rr.errors.first());
    let _ = s2;
    let cc = fresh.check(kist_core::CheckOptions { read_data: true }).await.unwrap();
    println!("check --read-data after prune D: {} error(s), {} warning(s)", cc.errors.len(), cc.warnings.len());
    assert!(rr.errors.is_empty() && cc.errors.is_empty(), "DATA LOSS ({variant})");
    println!("NO DATA LOSS ({variant}): restore ok, check clean");
    true
}

#[tokio::main]
async fn main() {
    let variant = std::env::args().nth(1).unwrap_or_else(|| "two-prunes".to_owned());
    for seed in 0..3 {
        assert!(run(&variant, seed).await);
    }
    println!("ALL SEEDS OK ({variant})");
    std::process::exit(2);
}
