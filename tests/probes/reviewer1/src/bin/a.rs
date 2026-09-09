//! Probe (a): repack window. prune walk → backup commit → prune write_index.
//! Simulated by committing S, then hiding S while prune runs (= walk happened before S).
use kist_core::{CheckOptions, PruneOptions};
use kist_format::keys;
use probe::*;
use time::{Duration, OffsetDateTime};

fn prune_opts(now: OffsetDateTime) -> PruneOptions {
    PruneOptions { grace: 72 * H, inactive_after: 30 * 24 * H, repack_below_percent: 50, dry_run: false, now: Some(now) }
}

#[tokio::main]
async fn main() {
    let t = TestRepo::new().await;
    let a = t.dir.path().join("a");
    let b = t.dir.path().join("b");
    for s in [&a, &b] {
        std::fs::create_dir_all(s).unwrap();
        std::fs::write(s.join("shared.bin"), random_bytes(91, 60 * 1024)).unwrap();
    }
    std::fs::write(a.join("zz-big.bin"), random_bytes(92, 900 * 1024)).unwrap();
    let repo = t.open().await;
    let r = OffsetDateTime::now_utc();

    // client 1 backs up a, forgets it; client 2 backs up b (shared alive, big dead-but-indexed)
    let b1 = repo.backup(std::slice::from_ref(&a), client(1, Some(r))).await.unwrap();
    repo.forget(kist_core::ForgetOptions { snapshots: vec![b1.snapshot_key], policy: Default::default(), dry_run: false }).await.unwrap();
    repo.backup(std::slice::from_ref(&b), client(2, Some(r))).await.unwrap();

    // client 1 backs up a again: big chunks dedup against live pack Q; commit passes (J not written yet)
    let s = repo.backup(std::slice::from_ref(&a), client(1, Some(r))).await.unwrap();
    println!("S = {}", s.snapshot_key);
    let s_path = t.repo_path().join(&s.snapshot_key);
    let hidden = t.dir.path().join("hidden-snapshot");
    std::fs::rename(&s_path, &hidden).unwrap(); // prune's walk did not see S

    let packs_indexed_before: std::collections::HashSet<_> =
        repo.load_index().await.unwrap().packs().map(|(id, _)| *id).collect();
    for id in ids_under(&t, "packs") { set_age(&t.repo_path().join(keys::pack(&id)), 5 * 24 * H); }
    let p1 = t.open().await.prune(prune_opts(r)).await.unwrap();
    println!("P1 = {p1:?}");
    assert!(p1.repacked_packs >= 1, "expected a repack");
    std::fs::rename(&hidden, &s_path).unwrap(); // S reappears (it was there all along in reality)

    let packs_indexed_after: std::collections::HashSet<_> =
        t.open().await.load_index().await.unwrap().packs().map(|(id, _)| *id).collect();
    let orphans: Vec<_> = packs_indexed_before.difference(&packs_indexed_after).copied().collect();
    println!("repacked (now orphan) packs: {orphans:?}");
    println!("(new design: no orphans expected)");

    let chk = t.open().await.check(CheckOptions { read_data: false }).await.unwrap();
    println!("check after P1: {} errors, first: {:?}", chk.errors.len(), chk.errors.first());
    assert!(chk.errors.is_empty(), "check must be clean: {:?}", chk.errors);

    // life goes on: the big file is gone from the source now (S is the backup you'd restore it from);
    // both clients back up again, nothing re-uploads the missing chunks
    std::fs::remove_file(a.join("zz-big.bin")).unwrap();
    t.open().await.backup(std::slice::from_ref(&a), client(1, Some(r + Duration::days(1)))).await.unwrap();
    t.open().await.backup(std::slice::from_ref(&b), client(2, Some(r + Duration::days(1)))).await.unwrap();

    let p2 = t.open().await.prune(prune_opts(r + Duration::days(4))).await;
    println!("P2 = {p2:?}");
    let _p2 = p2.expect("P2 failed");

    let p3 = t.open().await.prune(prune_opts(r + Duration::days(8))).await.unwrap();
    println!("P3 = {p3:?}");
    let packs_now = ids_under(&t, "packs");
    let _ = packs_now;
    assert!(s_path.exists(), "S still exists");
    let out = t.dir.path().join("out");
    let rs = t.open().await.restore(&s.snapshot_key, &out, Default::default()).await.unwrap();
    println!("restore S: files={} errors={:?}", rs.files, rs.errors);
    assert!(rs.errors.is_empty(), "restore of S must succeed");
    let chk = t.open().await.check(CheckOptions { read_data: true }).await.unwrap();
    assert!(chk.errors.is_empty(), "{:?}", chk.errors);
    println!("NO DATA LOSS: snapshot {} restores fully after P1..P3; check --read-data clean", s.snapshot_key);
}
