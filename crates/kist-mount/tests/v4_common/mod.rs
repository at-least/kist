//! v4 mount 測試共用：小參數的 repo（對齊 kist-core 的 tests/v4_common）、手工寫 snapshot。

#![allow(dead_code)]

use std::path::Path;
use std::time::Duration;

use kist_backend::Backend;
use kist_core::v4::{BackupOptions, InitOptions, Repo};
use kist_crypto::KdfCost;
use kist_format::v4::config::Chunker;
use kist_format::v4::ids::{ClientId, ObjectId};
use kist_format::v4::index::{Settings, DEFAULT_GRACE};
use kist_format::v4::keys::Key;
use kist_format::v4::meta::Item;
use kist_format::v4::snapshot::Root;

pub const PASSWORD: &[u8] = b"mount test password";

pub const CLIENT: &str = "22222222222222222222222222222222";

pub fn client() -> ClientId {
    ClientId::from_bytes([0x22; 16])
}

pub fn init_options() -> InitOptions {
    InitOptions {
        chunker: Chunker {
            min: 4 * 1024,
            avg: 16 * 1024,
            max: 64 * 1024,
        },
        kdf: KdfCost {
            m_cost_kib: 8,
            t_cost: 1,
            p_cost: 1,
        },
        settings: Some(Settings {
            grace: DEFAULT_GRACE,
            replicas: 0,
            pack_target: 256 * 1024,
            mpack_target: 256 * 1024,
        }),
    }
}

pub fn backup_options() -> BackupOptions {
    let mut o = BackupOptions::new(client(), "mounthost", "tester");
    o.lone_head_wait = Duration::ZERO;
    o
}

pub struct TestRepo {
    pub dir: tempfile::TempDir,
    pub backend: Backend,
}

impl TestRepo {
    pub async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let backend = Backend::local(&dir.path().join("repo")).unwrap();
        Repo::init(backend.clone(), PASSWORD, init_options())
            .await
            .unwrap();
        Self { dir, backend }
    }

    /// 沒有本地快取：mount 讀的都來自 repo。
    pub async fn open(&self) -> Repo {
        Repo::open(self.backend.clone(), PASSWORD, None)
            .await
            .unwrap()
    }

    pub async fn backup(&self, src: &Path) -> String {
        let repo = self.open().await;
        repo.backup(&[src.to_path_buf()], backup_options())
            .await
            .unwrap()
            .snapshot
    }

    pub async fn fs_core(&self) -> kist_mount::v4::FsCore {
        kist_mount::v4::FsCore::new(self.open().await, kist_mount::MountConfig::default())
            .await
            .unwrap()
    }

    /// 暫存目錄的名稱（本機來源的定位 `/tmp/<它>/...` 的第二段）。
    pub fn tmpname(&self) -> String {
        self.dir
            .path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned()
    }
}

/// 手工寫一個 snapshot：`items` 與 roots 的 RootSeg 寫進一個 mpack、記進 delta，時間 `at`（秒）。回傳時間戳。
pub async fn hand_snapshot(repo: &Repo, at: i64, roots: Vec<Root>, items: Vec<Item>) -> String {
    use kist_core::v4::pack::PackWriter;
    use kist_format::v4::index::{Delta, DeltaPack};
    use kist_format::v4::keys::SnapTs;
    use kist_format::v4::pack::Kind;
    use kist_format::v4::snapshot::{RootSeg, Snapshot};

    let keys = std::sync::Arc::clone(repo.keys());
    let mut w = PackWriter::new(
        std::sync::Arc::clone(&keys),
        Kind::Meta,
        u64::MAX,
        64 * 1024,
    );
    let mut seen = std::collections::HashSet::new();
    let mut add = |item: &Item| {
        let cbor = item.encode().unwrap();
        let id = keys.meta_id(&cbor);
        if seen.insert(id) {
            w.add(id.into(), &cbor).unwrap();
        }
        id
    };
    for item in &items {
        add(item);
    }
    let roots_id = add(&Item::RootSeg(RootSeg::new(roots)));
    let p = w.finish().unwrap().unwrap();
    let c = client();
    repo.backend()
        .put_if_absent(
            &Key::Mpack {
                client: c,
                id: p.id,
                replica: false,
            }
            .path(),
            p.bytes.clone(),
        )
        .await
        .unwrap();
    let delta = Delta::new(
        c,
        vec![DeltaPack {
            id: p.id,
            kind: Kind::Meta,
            size: p.bytes.len() as u64,
            entries: p.entries,
        }],
    )
    .encode()
    .unwrap();
    let sealed = keys.seal_delta(&c, &delta).unwrap();
    repo.backend()
        .put_if_absent(
            &Key::Delta {
                client: c,
                id: ObjectId::of(&sealed),
            }
            .path(),
            sealed,
        )
        .await
        .unwrap();
    let time = at * 1_000_000_000;
    let ts = SnapTs::from_nanos(time).unwrap();
    let snap = Snapshot {
        v: kist_format::v4::VERSION,
        roots: roots_id,
        time,
        host: "handcraft".to_owned(),
        user: String::new(),
        client: c,
        parent: None,
        stats: Default::default(),
    };
    let sealed = keys
        .seal_snapshot(&c, &ts, &snap.encode().unwrap())
        .unwrap();
    let key = Key::Snapshot {
        client: c,
        ts: ts.clone(),
        replica: false,
    };
    repo.backend()
        .put_if_absent(&key.path(), sealed)
        .await
        .unwrap();
    ts.as_str().to_owned()
}
