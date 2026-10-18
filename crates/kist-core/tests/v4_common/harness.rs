//! 維護命令（prune、重建）測試共用的 repo：虛擬時鐘、hooked 後端、手工的 pack 與世代。

use std::collections::BTreeMap;
use std::sync::Arc;

use kist_core::v4::pack::PackWriter;
use kist_core::v4::{ForgetOptions, PruneOptions, PruneReport, Repo};
use kist_format::v4::ids::{ItemId, ObjectId};
use kist_format::v4::index::{shard_of, status, Head, Record, Shard, SHARDS};
use kist_format::v4::keys::Key;
use kist_format::v4::meta::node_type;
use kist_format::v4::pack::{Kind, PackEntry};
use kist_format::v4::snapshot::Root;

use super::*;

pub struct T {
    pub clock: Arc<VClock>,
    pub hooked: HookedRepo,
    pub repo: Repo,
    pub src: tempfile::TempDir,
}

pub async fn setup() -> T {
    setup_with(1).await
}

pub async fn setup_with(replicas: u8) -> T {
    let clock = VClock::new();
    let hooked = HookedRepo::with_clock(init_options(replicas), Some(Arc::clone(&clock))).await;
    let repo = hooked.open_uncached().await;
    T {
        clock,
        hooked,
        repo,
        src: tempfile::tempdir().unwrap(),
    }
}

impl T {
    /// client `c` 備份來源目錄裡的 `dir`（一組 roots 一個目錄）。
    pub async fn backup(&self, c: u8, dir: &str) -> String {
        let path = self.src.path().join(dir);
        let mut o = opts(c);
        o.clock = self.clock.clock();
        self.repo.backup(&[path], o).await.unwrap().snapshot
    }

    pub fn write(&self, dir: &str, name: &str, bytes: &[u8]) {
        let d = self.src.path().join(dir);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join(name), bytes).unwrap();
    }

    pub async fn prune(&self) -> PruneReport {
        self.try_prune().await.unwrap()
    }

    pub async fn try_prune(&self) -> kist_core::Result<PruneReport> {
        self.prune_with(kist_core::v4::prune::DEFAULT_REPACK_BELOW)
            .await
    }

    pub async fn prune_with(&self, repack_below: u8) -> kist_core::Result<PruneReport> {
        self.repo
            .prune(PruneOptions {
                repack_below,
                clock: self.clock.clock(),
                ..PruneOptions::default()
            })
            .await
    }

    /// `prune --read-data`。
    pub async fn prune_read_data(&self) -> PruneReport {
        self.repo
            .prune(PruneOptions {
                read_data: true,
                clock: self.clock.clock(),
                ..PruneOptions::default()
            })
            .await
            .unwrap()
    }

    /// 前進 `hours` 小時之後 prune。
    pub async fn prune_after(&self, hours: i64) -> PruneReport {
        self.clock.advance_hours(hours);
        self.prune().await
    }

    pub async fn forget(&self, key: &str) {
        self.repo
            .forget(ForgetOptions {
                snapshots: vec![key.to_owned()],
                retire: true,
                ..Default::default()
            })
            .await
            .unwrap();
    }

    /// 最大世代的 map。
    pub async fn records(&self) -> BTreeMap<ObjectId, Record> {
        let g = *self
            .repo
            .list_heads(None)
            .await
            .unwrap()
            .keys()
            .next_back()
            .unwrap();
        let gen = self.repo.read_head(g).await.unwrap();
        self.repo
            .load_map(&gen)
            .await
            .unwrap()
            .records()
            .iter()
            .map(|r| (r.id, r.clone()))
            .collect()
    }

    pub async fn newest(&self) -> u64 {
        *self
            .repo
            .list_heads(None)
            .await
            .unwrap()
            .keys()
            .next_back()
            .unwrap()
    }

    /// 列表裡的 pack 與 mpack 主體的名稱。
    pub async fn pack_ids(&self, prefix: &str) -> Vec<ObjectId> {
        self.hooked
            .keys(prefix)
            .await
            .iter()
            .filter_map(|k| match Key::parse(k) {
                Ok(Key::Pack { id, .. }) => Some(id),
                Ok(Key::Mpack {
                    id, replica: false, ..
                }) => Some(id),
                _ => None,
            })
            .collect()
    }

    /// 手工寫一個 pack 或 mpack（不寫 delta：prune 從列表收錄它）。回傳名稱與項目。
    pub async fn hand_pack(
        &self,
        c: u8,
        kind: Kind,
        items: &[(ItemId, Vec<u8>)],
    ) -> (ObjectId, Vec<PackEntry>) {
        self.hand_pack_with_parity(c, kind, items, 0).await
    }

    /// 同 [`T::hand_pack`]，另寫 `m` 片 parity（0 不寫）。
    pub async fn hand_pack_with_parity(
        &self,
        c: u8,
        kind: Kind,
        items: &[(ItemId, Vec<u8>)],
        m: usize,
    ) -> (ObjectId, Vec<PackEntry>) {
        let mut w = PackWriter::new(Arc::clone(self.repo.keys()), kind, u64::MAX, 64 * 1024);
        for (id, plain) in items {
            w.add(*id, plain).unwrap();
        }
        let p = w.finish().unwrap().unwrap();
        let key = match kind {
            Kind::Data => Key::Pack {
                client: client(c),
                id: p.id,
            },
            Kind::Meta => Key::Mpack {
                client: client(c),
                id: p.id,
                replica: false,
            },
        };
        if m > 0 {
            let parity = kist_format::v4::parity::encode(&p.id, &p.bytes, m).unwrap();
            self.repo
                .backend()
                .put_if_absent(
                    &Key::Parity {
                        client: client(c),
                        id: p.id,
                    }
                    .path(),
                    parity,
                )
                .await
                .unwrap();
        }
        self.repo
            .backend()
            .put_if_absent(&key.path(), p.bytes)
            .await
            .unwrap();
        (p.id, p.entries)
    }

    /// 沒有項目的資料 pack：trailer 的 `v` 是 `version`（解得開、版本不認得），`None` 是解不開的 trailer。
    pub async fn odd_pack(&self, c: u8, version: Option<u32>) -> ObjectId {
        #[derive(serde::Serialize)]
        struct Trailer {
            v: u32,
            kind: u8,
            entries: Vec<u8>,
        }
        let sealed = match version {
            Some(v) => self
                .repo
                .keys()
                .seal_trailer(
                    &kist_format::cbor::encode(&Trailer {
                        v,
                        kind: 0,
                        entries: vec![],
                    })
                    .unwrap(),
                )
                .unwrap(),
            None => vec![7u8; 64],
        };
        let bytes = kist_format::v4::pack::finish(kist_format::v4::pack::begin(), &sealed);
        let id = ObjectId::of(&bytes);
        self.repo
            .backend()
            .put_if_absent(
                &Key::Pack {
                    client: client(c),
                    id,
                }
                .path(),
                bytes,
            )
            .await
            .unwrap();
        id
    }

    /// 扮演維護角色寫下一代：從最大世代的 map 與 head 開始，`edit` 改記錄與 head；`w` 與分片的修改時間是
    /// 虛擬時鐘當下的值（要偏差就先設時鐘的偏差）。
    pub async fn hand_generation(
        &self,
        edit: impl FnOnce(&mut BTreeMap<ObjectId, Record>, &mut Head),
    ) -> u64 {
        let g = self.newest().await;
        let gen = self.repo.read_head(g).await.unwrap();
        let mut records = self.records().await;
        let mut head = Head {
            g: g + 1,
            w: ns(self.clock.writer_now()),
            parent: Some(gen.hash),
            ..gen.head
        };
        edit(&mut records, &mut head);
        let mut by_shard: BTreeMap<u8, Vec<Record>> = BTreeMap::new();
        for r in records.into_values() {
            by_shard.entry(shard_of(&r.id)).or_default().push(r);
        }
        let mut shards = vec![None; SHARDS];
        for (i, rs) in by_shard {
            let sealed = self
                .repo
                .keys()
                .seal_shard(&Shard::new(i, rs).encode().unwrap())
                .unwrap();
            let id = ObjectId::of(&sealed);
            for replica in [false, true] {
                if replica && head.settings.replicas == 0 {
                    continue;
                }
                self.repo
                    .backend()
                    .put_if_absent(&Key::Shard { id, replica }.path(), sealed.clone())
                    .await
                    .unwrap();
            }
            shards[i as usize] = Some(id);
        }
        head.shards = shards;
        let sealed = self
            .repo
            .keys()
            .seal_head(g + 1, &head.encode().unwrap())
            .unwrap();
        for replica in [false, true] {
            self.repo
                .backend()
                .put_if_absent(&Key::Head { g: g + 1, replica }.path(), sealed.clone())
                .await
                .unwrap();
        }
        g + 1
    }

    /// 列表裡全部的 pack 與 mpack（主體）。
    pub async fn all_packs(&self) -> Vec<ObjectId> {
        let mut v = self.pack_ids("packs").await;
        v.extend(self.pack_ids("mpacks").await);
        v
    }
}

pub fn status_of(records: &BTreeMap<ObjectId, Record>, id: &ObjectId) -> Option<u8> {
    records.get(id).map(|r| r.status.s)
}

/// keep 與 gone 兩組 roots 各備份一次、forget 掉 gone：回傳 (keep 的 pack, 只屬於 gone 的 pack, gone 的 key)。
pub async fn keep_and_gone(t: &T) -> (Vec<ObjectId>, Vec<ObjectId>, String) {
    t.write("keep", "k", &data("keep", 100 * 1024));
    t.write("gone", "g", &data("gone", 100 * 1024));
    t.backup(1, "keep").await;
    let before = t.all_packs().await;
    let gone = t.backup(1, "gone").await;
    let theirs = t
        .all_packs()
        .await
        .into_iter()
        .filter(|id| !before.contains(id))
        .collect();
    t.forget(&gone).await;
    (before, theirs, gone)
}

/// 讓 `ids` 標成 condemned（在 g3）：prune 三輪。
pub async fn condemn(t: &T, ids: &[ObjectId]) {
    t.prune().await;
    t.prune_after(73).await;
    t.prune_after(73).await;
    let recs = t.records().await;
    for id in ids {
        assert_eq!(status_of(&recs, id), Some(status::CONDEMNED), "{id}");
    }
}

/// 把 pack 或 mpack 主體裡某一項的 sealed bytes 改掉一個 byte（位元損壞；名稱不變）。
pub async fn damage(t: &T, key: &Key, entry: &PackEntry) {
    let mut bytes = t.repo.backend().get(&key.path()).await.unwrap();
    bytes[(entry.o + 30) as usize] ^= 0x55;
    t.hooked.overwrite(&key.path(), bytes).await;
}

pub fn file_root(name: &str, chunk: kist_format::v4::ids::ChunkId, len: u64) -> Root {
    let mut e = entry(
        name.as_bytes(),
        node_type::FILE,
        kist_format::v4::meta::meta_kind::GENERIC,
    );
    e.size = len;
    e.chunks = vec![chunk.into()];
    Root {
        path: format!("/{name}").into_bytes(),
        t: node_type::FILE,
        tree: None,
        entry: Some(e),
    }
}
