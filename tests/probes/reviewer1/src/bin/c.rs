//! Probe (c): chunk present in both an expired-marked pack P and a fresh pack P_B;
//! does verify_referenced_chunks resolve it to P (spurious PackMissing) or P_B?
use kist_core::CoreError;
use kist_format::keys;
use probe::*;

#[tokio::main]
async fn main() {
    let mut failures = 0;
    let n = 8;
    for i in 0..n {
        let t = TestRepo::new().await;
        let src = t.dir.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("f.bin"), random_bytes(7, 100 * 1024)).unwrap();
        let repo = t.open().await;
        repo.backup(std::slice::from_ref(&src), client(1, None)).await.unwrap();
        let victim = *ids_under(&t, "packs").iter().next().unwrap();
        // young mark: next backup rewrites victim's chunks into a new pack, commit OK
        mark(&t, &victim, H);
        let s2 = repo.backup(std::slice::from_ref(&src), client(1, None)).await.unwrap();
        assert!(s2.stats.chunks_new > 0);
        // mark ages past grace without a prune run in between; client backs up again
        set_age(&t.repo_path().join(keys::gc(&victim)), 4 * 24 * H);
        let r = t.open().await.backup(std::slice::from_ref(&src), client(1, None)).await;
        match r {
            Ok(s) => println!("run {i}: commit OK (chunks_new {})", s.stats.chunks_new),
            Err(CoreError::PackMissing { pack, .. }) => { failures += 1; println!("run {i}: PackMissing pack={pack} (victim={victim})"); }
            Err(e) => panic!("{e}"),
        }
    }
    println!("{failures}/{n} runs failed with spurious PackMissing");
}
