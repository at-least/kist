//! Probe (b): tree HEAD→DELETE TOCTOU post-state. Backup started after tree X was marked,
//! re-puts X; prune (already past its HEAD) deletes X and clears the mark; backup commits.
use kist_core::CheckOptions;
use kist_format::keys;
use probe::*;

#[tokio::main]
async fn main() {
    let t = TestRepo::new().await;
    let src = t.dir.path().join("src");
    std::fs::create_dir_all(src.join("sub")).unwrap();
    std::fs::write(src.join("f.bin"), random_bytes(1, 50 * 1024)).unwrap();
    std::fs::write(src.join("sub/g.bin"), random_bytes(2, 50 * 1024)).unwrap();
    let repo = t.open().await;
    let b1 = repo.backup(std::slice::from_ref(&src), client(1, None)).await.unwrap();
    repo.forget(kist_core::ForgetOptions { snapshots: vec![b1.snapshot_key], policy: Default::default(), dry_run: false }).await.unwrap();
    // prune P1 marked the (now garbage) root tree X long ago
    let x = b1.root;
    mark(&t, &x, 4 * 24 * H);
    // backup B starts after the mark (sees gc/X), re-puts X
    let prepared = repo.backup_prepare(std::slice::from_ref(&src), client(1, None)).await.unwrap();
    assert!(t.repo_path().join(keys::tree(&x)).exists());
    // prune P2 did HEAD(X) before B's put, now deletes X and clears the mark
    std::fs::remove_file(t.repo_path().join(keys::tree(&x))).unwrap();
    std::fs::remove_file(t.repo_path().join(keys::gc(&x))).unwrap();
    // B commits
    let r = prepared.commit().await;
    println!("commit: {:?}", r.as_ref().map(|s| s.snapshot_key.clone()));
    let s = r.expect("commit should have failed safely but succeeded");
    let chk = t.open().await.check(CheckOptions { read_data: false }).await.unwrap();
    println!("check errors: {:?}", chk.errors);
    assert!(!chk.errors.is_empty());
    let out = t.dir.path().join("out");
    let rs = t.open().await.restore(&s.snapshot_key, &out, Default::default()).await;
    println!("restore: {:?}", rs.as_ref().map(|s| s.errors.len()));
    assert!(rs.is_err() || !rs.unwrap().errors.is_empty());
    println!("BROKEN SNAPSHOT REPRODUCED: {} references deleted tree {x}", s.snapshot_key);
}
