//! 讀取端的世代（§10）：列 `maint/gen/`、讀 head、載入 map 分片、讀當下列表裡的 delta，以及一個 ID 的
//! 持有者。
//!
//! backup 的規則（B1：最大世代必須讀得了、要有 `.r1`）在 [`super::backup`]；restore 與 mount 可以退回較舊的
//! 世代（第 5 段）。這裡只提供積木。
//!
//! 持有者的順序（§10）：map 裡 live 先於 condemned，同一級 `since` 小的先、名稱小的先；map 沒提到的 pack 才看
//! delta；tombstone 排最後，只在其他都失敗時用；unreadable 與 unknown 不是持有者。

use std::collections::{BTreeMap, HashMap, HashSet};

use kist_format::v4::ids::{ClientId, ItemId, ObjectId};
use kist_format::v4::index::{status, Delta, DeltaPack, Head, Record, Shard};
use kist_format::v4::keys::{Gen, Key, DELTAS, GEN};
use kist_format::v4::pack::{Kind, PackEntry};

use super::cache::Kind as CacheKind;
use super::repo::Repo;
use crate::{CoreError, Result};

/// 一個世代：head 與讀到的那份 bytes 的 BLAKE3（下一代的 `parent`）。
#[derive(Debug, Clone)]
pub struct Generation {
    pub g: Gen,
    pub head: Head,
    pub hash: ObjectId,
}

/// 讀到的一份 head：內容、密封的 bytes、是不是 `.r1`。
#[derive(Debug, Clone)]
pub struct HeadCopy {
    pub gen: Generation,
    pub bytes: Vec<u8>,
    pub replica: bool,
}

/// `maint/gen/` 的列表：每個世代的主體與 `.r1` 在不在、各自的後端修改時間。
pub type HeadListing = BTreeMap<Gen, Copies>;

/// 一個世代在列表裡的兩份：後端修改時間（截到秒，§10 的 `T`），不在列表裡是 `None`。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Copies {
    pub body: Option<time::OffsetDateTime>,
    pub replica: Option<time::OffsetDateTime>,
}

/// 持有者從哪裡來。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Source {
    /// map 的記錄（prune 讀過的 trailer）。
    Map,
    /// 當下列表裡某個 delta 提到、map 沒提到的 pack（只是提示）。
    Delta,
}

/// 一個 ID 的一個持有者：哪個 pack、裡面的哪一項。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Holder {
    pub pack: ObjectId,
    pub client: ClientId,
    pub kind: Kind,
    pub entry: PackEntry,
    pub source: Source,
    pub by_prune: bool,
    /// map 記錄的狀態；delta 的持有者是 live。
    pub status: u8,
}

impl Holder {
    /// 主體的 key（mpack 的 `.r1` 由讀取端另外試）。
    pub fn key(&self) -> Key {
        pack_key(self.kind, self.client, self.pack)
    }
}

pub fn pack_key(kind: Kind, client: ClientId, id: ObjectId) -> Key {
    match kind {
        Kind::Data => Key::Pack { client, id },
        Kind::Meta => Key::Mpack {
            client,
            id,
            replica: false,
        },
    }
}

/// 一組 pack 的項目，依 (ID, 持有者的順序) 排好，以二分搜尋查。
#[derive(Debug, Default)]
struct IdTable {
    rows: Vec<(ItemId, u32, u32)>,
}

impl IdTable {
    fn find(&self, id: &ItemId) -> &[(ItemId, u32, u32)] {
        let start = self.rows.partition_point(|(i, ..)| i < id);
        let end = start + self.rows[start..].partition_point(|(i, ..)| i == id);
        &self.rows[start..end]
    }
}

/// `Map(g)`：全部記錄，與 ID → 持有者的表。
#[derive(Debug, Default)]
pub struct MapView {
    records: Vec<Record>,
    by_pack: HashMap<ObjectId, u32>,
    ids: IdTable,
}

impl MapView {
    pub fn new(records: Vec<Record>) -> Self {
        let by_pack = records
            .iter()
            .enumerate()
            .map(|(i, r)| (r.id, i as u32))
            .collect();
        let mut rows = Vec::new();
        for (ri, r) in records.iter().enumerate() {
            if r.order_key().is_none() {
                continue;
            }
            for (ei, e) in r.entries.iter().enumerate() {
                rows.push((e.i, ri as u32, ei as u32));
            }
        }
        rows.sort_by(|a, b| {
            a.0.cmp(&b.0).then_with(|| {
                records[a.1 as usize]
                    .order_key()
                    .cmp(&records[b.1 as usize].order_key())
            })
        });
        Self {
            records,
            by_pack,
            ids: IdTable { rows },
        }
    }

    pub fn record(&self, pack: &ObjectId) -> Option<&Record> {
        self.by_pack.get(pack).map(|i| &self.records[*i as usize])
    }

    pub fn records(&self) -> &[Record] {
        &self.records
    }

    /// 種類相符的持有者，依 §10 的順序（live、condemned、tombstone）。
    pub fn holders(&self, id: &ItemId, kind: Kind) -> Vec<Holder> {
        self.ids
            .find(id)
            .iter()
            .map(|(_, ri, ei)| {
                let r = &self.records[*ri as usize];
                (r, r.entries[*ei as usize])
            })
            .filter(|(r, _)| r.kind == kind)
            .map(|(r, entry)| Holder {
                pack: r.id,
                client: r.client,
                kind: r.kind,
                entry,
                source: Source::Map,
                by_prune: r.by_prune,
                status: r.status.s,
            })
            .collect()
    }
}

/// 當下列表裡的 delta 提到、map 沒提到的 pack。
#[derive(Debug, Default)]
pub struct Deltas {
    packs: Vec<(ClientId, DeltaPack)>,
    by_pack: HashMap<ObjectId, u32>,
    ids: IdTable,
    /// 列表裡的 delta 的名稱（快取的保留範圍）。
    pub listed: HashSet<ObjectId>,
}

impl Deltas {
    fn new(packs: Vec<(ClientId, DeltaPack)>, listed: HashSet<ObjectId>) -> Self {
        let by_pack = packs
            .iter()
            .enumerate()
            .map(|(i, (_, p))| (p.id, i as u32))
            .collect();
        let mut rows = Vec::new();
        for (pi, (_, p)) in packs.iter().enumerate() {
            for (ei, e) in p.entries.iter().enumerate() {
                rows.push((e.i, pi as u32, ei as u32));
            }
        }
        // 同一個 ID 在幾個 delta pack 裡：依 pack 名稱，順序才固定
        rows.sort_by(|a, b| {
            a.0.cmp(&b.0)
                .then_with(|| packs[a.1 as usize].1.id.cmp(&packs[b.1 as usize].1.id))
        });
        Self {
            packs,
            by_pack,
            ids: IdTable { rows },
            listed,
        }
    }

    pub fn contains(&self, pack: &ObjectId) -> bool {
        self.by_pack.contains_key(pack)
    }

    pub fn packs(&self) -> impl Iterator<Item = (&ClientId, &DeltaPack)> {
        self.packs.iter().map(|(c, p)| (c, p))
    }

    pub fn holders(&self, id: &ItemId, kind: Kind) -> Vec<Holder> {
        self.ids
            .find(id)
            .iter()
            .map(|(_, pi, ei)| {
                let (c, p) = &self.packs[*pi as usize];
                (*c, p, p.entries[*ei as usize])
            })
            .filter(|(_, p, _)| p.kind == kind)
            .map(|(client, p, entry)| Holder {
                pack: p.id,
                client,
                kind: p.kind,
                entry,
                source: Source::Delta,
                by_prune: false,
                status: status::LIVE,
            })
            .collect()
    }
}

/// 一個世代的讀取端狀態。
#[derive(Debug)]
pub struct State {
    pub gen: Generation,
    pub map: MapView,
    pub deltas: Deltas,
}

impl State {
    /// B2 的候選：`Map(g)` 裡 live、種類相符的記錄（依 §10 排序），之後是 map 沒提到的 delta 裡的 pack。
    /// condemned 不在裡面：backup 不沿用 condemned 的 pack（§11.4）。
    pub fn reuse_candidates(&self, id: &ItemId, kind: Kind) -> Vec<Holder> {
        let mut out: Vec<Holder> = self
            .map
            .holders(id, kind)
            .into_iter()
            .filter(|h| h.status == status::LIVE)
            .collect();
        out.extend(self.deltas.holders(id, kind));
        out
    }

    /// 讀取的順序（§10）：live、condemned、map 沒提到的 delta、最後 tombstone。
    pub fn read_candidates(&self, id: &ItemId, kind: Kind) -> Vec<Holder> {
        let map = self.map.holders(id, kind);
        let (tombstones, held): (Vec<_>, Vec<_>) =
            map.into_iter().partition(|h| h.status == status::TOMBSTONE);
        let mut out = held;
        out.extend(self.deltas.holders(id, kind));
        out.extend(tombstones);
        out
    }
}

impl Repo {
    /// 列 `maint/gen/`。`from` 是上次看過的世代：從它的主體開始列（含它的 `.r1` 與之後的世代）；`None`
    /// 列全部。不是 v4 head 的名稱略過並警告。
    pub async fn list_heads(&self, from: Option<Gen>) -> Result<HeadListing> {
        let prefix = GEN.trim_end_matches('/');
        let objects = match from {
            Some(g) if g > 0 => {
                let after = Key::Head {
                    g: g - 1,
                    replica: true,
                };
                self.backend().list_after(prefix, &after.path()).await?
            }
            _ => self.backend().list(prefix).await?,
        };
        let mut out = HeadListing::new();
        for o in objects {
            match Key::parse(&o.key) {
                Ok(Key::Head { g, replica }) => {
                    let c = out.entry(g).or_default();
                    if replica {
                        c.replica = Some(o.modified);
                    } else {
                        c.body = Some(o.modified);
                    }
                }
                _ => tracing::warn!("{}: not a v4 generation head; ignored", o.key),
            }
        }
        Ok(out)
    }

    /// 讀 head g：主體，不行讀 `.r1`。兩份都不行（讀不了、解不開、版本不認得）就是錯誤，訊息帶兩份的原因。
    pub async fn read_head(&self, g: Gen) -> Result<Generation> {
        Ok(self.read_head_copy(g).await?.gen)
    }

    /// [`Repo::read_head`]，連同讀到的那一份的密封 bytes 與它是不是 `.r1`：維護角色補寫 `.r1` 要原樣的 bytes
    /// （P1），`T(g)` 取讀到的那一份的修改時間。
    pub async fn read_head_copy(&self, g: Gen) -> Result<HeadCopy> {
        let mut reasons = Vec::new();
        for replica in [false, true] {
            let key = Key::Head { g, replica }.path();
            let result = async {
                let bytes = self.backend().get(&key).await?;
                let plain = self
                    .keys()
                    .open_head(g, &bytes)
                    .map_err(|e| CoreError::corrupt(&key, e.to_string()))?;
                let head = Head::decode(&plain, g)?;
                Ok::<_, CoreError>(HeadCopy {
                    gen: Generation {
                        g,
                        head,
                        hash: ObjectId::of(&bytes),
                    },
                    bytes,
                    replica,
                })
            }
            .await;
            match result {
                Ok(copy) => {
                    if replica {
                        tracing::warn!("generation {g} read from its .r1 (the body is unusable)");
                    }
                    return Ok(copy);
                }
                Err(e) => reasons.push(format!("{key}: {e}")),
            }
        }
        Err(CoreError::Generation(format!(
            "cannot read generation head {g}: {}",
            reasons.join("; ")
        )))
    }

    /// 以內容命名的物件：本地快取，不然 repo 的主體，不行讀 `.r1`（`replica` 給 key）。名稱一律重算。
    async fn get_named(
        &self,
        kind: CacheKind,
        id: &ObjectId,
        body: &Key,
        replica: Option<&Key>,
    ) -> Result<Vec<u8>> {
        if let Some(bytes) = self.cache().and_then(|c| c.get(kind, id)) {
            return Ok(bytes);
        }
        let mut reasons = Vec::new();
        for key in std::iter::once(body).chain(replica) {
            let path = key.path();
            match self.backend().get(&path).await {
                Ok(bytes) if ObjectId::of(&bytes) == *id => {
                    if let Some(c) = self.cache() {
                        c.put(kind, id, &bytes);
                    }
                    return Ok(bytes);
                }
                Ok(_) => reasons.push(format!("{path}: content does not match its name")),
                Err(e) => reasons.push(format!("{path}: {e}")),
            }
        }
        Err(CoreError::corrupt(body.path(), reasons.join("; ")))
    }

    /// 載入 head 指向的分片（依名稱快取），組成 `Map(g)`。任何一片讀不了就是錯誤：map 不完整就不知道誰是
    /// 持有者。
    pub async fn load_map(&self, gen: &Generation) -> Result<MapView> {
        let mut records = Vec::new();
        let mut referenced = HashSet::new();
        for (i, id) in gen.head.shards.iter().enumerate() {
            let Some(id) = id else { continue };
            referenced.insert(*id);
            let body = Key::Shard {
                id: *id,
                replica: false,
            };
            let replica = Key::Shard {
                id: *id,
                replica: true,
            };
            let shard = async {
                let bytes = self
                    .get_named(CacheKind::Shard, id, &body, Some(&replica))
                    .await?;
                let plain = self
                    .keys()
                    .open_shard(&bytes)
                    .map_err(|e| CoreError::corrupt(body.path(), e.to_string()))?;
                Ok::<_, CoreError>(Shard::decode(&plain, i as u8)?)
            }
            .await
            .map_err(|e| {
                CoreError::Generation(format!(
                    "cannot read map shard {i} ({id}) of generation {}: {e}; run maintenance to rebuild the map",
                    gen.g
                ))
            })?;
            records.extend(shard.records);
        }
        if let Some(c) = self.cache() {
            c.retain(CacheKind::Shard, &referenced);
        }
        Ok(MapView::new(records))
    }

    /// 列 `deltas/` 並讀列表裡的每一份（依名稱快取）。解不開、名稱不符、明文的 client 與前綴不同的跳過並
    /// 警告（§10）；map 已經有記錄的 pack 不看 delta。
    pub async fn load_deltas(&self, map: &MapView) -> Result<Deltas> {
        let mut packs = Vec::new();
        let mut seen = HashSet::new();
        let mut listed = HashSet::new();
        for o in self.backend().list(DELTAS.trim_end_matches('/')).await? {
            let Ok(key @ Key::Delta { client, id }) = Key::parse(&o.key) else {
                tracing::warn!("{}: not a v4 delta; ignored", o.key);
                continue;
            };
            listed.insert(id);
            let delta = async {
                let bytes = self.get_named(CacheKind::Delta, &id, &key, None).await?;
                let plain = self
                    .keys()
                    .open_delta(&client, &bytes)
                    .map_err(|e| CoreError::corrupt(key.path(), e.to_string()))?;
                Ok::<_, CoreError>(Delta::decode(&plain, &client)?)
            }
            .await;
            let delta = match delta {
                Ok(d) => d,
                Err(e) => {
                    tracing::warn!("{}: delta skipped: {e}", o.key);
                    continue;
                }
            };
            for p in delta.packs {
                if map.record(&p.id).is_none() && seen.insert(p.id) {
                    packs.push((client, p));
                }
            }
        }
        if let Some(c) = self.cache() {
            c.retain(CacheKind::Delta, &listed);
        }
        Ok(Deltas::new(packs, listed))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use kist_format::v4::index::Status;

    fn entry(b: u8) -> PackEntry {
        PackEntry {
            i: ItemId::from_bytes([b; 32]),
            o: 8,
            l: 41,
            r: 0,
        }
    }

    fn record(id: u8, s: u8, since: Gen, kind: Kind, entries: &[u8]) -> Record {
        let status = match s {
            status::CONDEMNED => Status {
                s,
                at: 1,
                age: 7200,
            },
            status::TOMBSTONE => Status { s, at: 1, age: 0 },
            _ => Status { s, at: 0, age: 0 },
        };
        Record {
            id: ObjectId::from_bytes([id; 32]),
            client: ClientId::from_bytes([1; 16]),
            kind,
            size: 0,
            entries: entries.iter().map(|b| entry(*b)).collect(),
            by_prune: false,
            since,
            status,
            held: false,
        }
    }

    #[test]
    fn holders_follow_the_spec_order_and_skip_non_holders() {
        let map = MapView::new(vec![
            record(0x50, status::TOMBSTONE, 0, Kind::Data, &[1]),
            record(0x40, status::CONDEMNED, 0, Kind::Data, &[1]),
            record(0x30, status::LIVE, 5, Kind::Data, &[1, 2]),
            record(0x20, status::LIVE, 2, Kind::Data, &[1]),
            record(0x10, status::UNREADABLE, 0, Kind::Data, &[1]),
            record(0x05, status::LIVE, 0, Kind::Meta, &[1]),
        ]);
        let order: Vec<u8> = map
            .holders(&ItemId::from_bytes([1; 32]), Kind::Data)
            .iter()
            .map(|h| h.pack.as_bytes()[0])
            .collect();
        assert_eq!(
            order,
            [0x20, 0x30, 0x40, 0x50],
            "since, then status; unreadable is no holder"
        );
        let deltas = Deltas::new(
            vec![(
                ClientId::from_bytes([2; 16]),
                DeltaPack {
                    id: ObjectId::from_bytes([0x60; 32]),
                    kind: Kind::Data,
                    size: 0,
                    entries: vec![entry(1)],
                },
            )],
            HashSet::new(),
        );
        let state = State {
            gen: Generation {
                g: 0,
                head: Head::initial(0, kist_format::v4::index::Settings::defaults(0)),
                hash: ObjectId::from_bytes([0; 32]),
            },
            map,
            deltas,
        };
        let id = ItemId::from_bytes([1; 32]);
        let reuse: Vec<u8> = state
            .reuse_candidates(&id, Kind::Data)
            .iter()
            .map(|h| h.pack.as_bytes()[0])
            .collect();
        assert_eq!(reuse, [0x20, 0x30, 0x60], "live from the map, then deltas");
        let read: Vec<u8> = state
            .read_candidates(&id, Kind::Data)
            .iter()
            .map(|h| h.pack.as_bytes()[0])
            .collect();
        assert_eq!(read, [0x20, 0x30, 0x40, 0x60, 0x50], "tombstone last");
        assert!(state
            .reuse_candidates(&ItemId::from_bytes([9; 32]), Kind::Data)
            .is_empty());
        assert_eq!(
            state.read_candidates(&id, Kind::Meta).len(),
            1,
            "kind must match"
        );
    }
}
