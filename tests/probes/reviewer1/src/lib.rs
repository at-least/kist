use std::collections::HashSet;
use std::path::{Path, PathBuf};
use kist_backend::Backend;
use kist_core::{BackupOptions, InitOptions, Repository};
use kist_crypto::KdfCost;
use kist_format::config::ChunkerParams;
use kist_format::{keys, ObjectId};
use rand::{RngExt, SeedableRng};

pub const PASSWORD: &str = "pw";
pub const H: std::time::Duration = std::time::Duration::from_secs(3600);

pub fn init_options() -> InitOptions {
    InitOptions {
        chunker: ChunkerParams { min: 4 * 1024, avg: 16 * 1024, max: 64 * 1024 },
        pack_target_size: 256 * 1024,
        kdf_cost: KdfCost { m_cost_kib: 8, t_cost: 1, p_cost: 1 },
    }
}
pub struct TestRepo { pub dir: tempfile::TempDir, pub backend: Backend }
impl TestRepo {
    pub async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let backend = Backend::local(&dir.path().join("repo")).unwrap();
        Repository::init(backend.clone(), PASSWORD.as_bytes(), init_options()).await.unwrap();
        Self { dir, backend }
    }
    pub async fn open(&self) -> Repository {
        Repository::open(self.backend.clone(), PASSWORD.as_bytes()).await.unwrap()
    }
    pub fn repo_path(&self) -> PathBuf { self.dir.path().join("repo") }
}
pub fn random_bytes(seed: u64, len: usize) -> Vec<u8> {
    let mut rng = rand::rngs::StdRng::seed_from_u64(seed);
    let mut v = vec![0u8; len];
    rng.fill(&mut v[..]);
    v
}
pub fn client(id: u8, now: Option<time::OffsetDateTime>) -> BackupOptions {
    BackupOptions { client_id: [id; 16], hostname: format!("host{id}"), username: "t".into(), now, gc_grace: 72 * H }
}
pub fn ids_under(t: &TestRepo, prefix: &str) -> HashSet<ObjectId> {
    let dir = t.repo_path().join(prefix);
    if !dir.is_dir() { return HashSet::new(); }
    std::fs::read_dir(dir).unwrap()
        .map(|e| keys::object_id_from_key(&e.unwrap().file_name().to_string_lossy()).unwrap())
        .collect()
}
pub fn set_age(path: &Path, age: std::time::Duration) {
    let when = std::time::SystemTime::now() - age;
    filetime::set_file_mtime(path, filetime::FileTime::from_system_time(when)).unwrap();
}
pub fn mark(t: &TestRepo, id: &ObjectId, age: std::time::Duration) {
    let path = t.repo_path().join(keys::gc(id));
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, b"KISTGC1\n").unwrap();
    set_age(&path, age);
}
pub fn snapshot_files(t: &TestRepo) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for c in std::fs::read_dir(t.repo_path().join("snapshots")).unwrap() {
        for s in std::fs::read_dir(c.unwrap().path()).unwrap() { out.push(s.unwrap().path()); }
    }
    out.sort();
    out
}
