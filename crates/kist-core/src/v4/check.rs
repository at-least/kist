//! check：驗證 v4 repo（format-v4.md §13.1）。一律讀後端，不用本地快取（快取裡的那一份證明不了後端還在）。
//!
//! 不讀資料（預設）：
//! - 每一代的 head：主體與 `.r1` 都在、解得開、bytes 相同；`parent` 等於前一代的 BLAKE3，世代沒有空洞（head
//!   永不刪除）。
//! - 讀取端用的那一代（列出來的最大、讀得了的）：它的分片都在、名稱相符（`replicas=1` 時 `.r1` 也要在）；記錄
//!   裡 live 與 condemned 的 pack 都在、大小相符（P3）；unreadable 與 unknown 的記錄、列表裡有而 map 與 delta
//!   都沒提到的 pack、解不開的 delta、對應的 pack 已不在的 parity 都回報。
//! - 每一個 snapshot（主體，與只有 `.r1` 的）：讀得出來，走過它引用的每一個 metadata item 與 chunk，照 P5 的
//!   定義每一個都要有可讀、非 tombstone、種類相符的持有者；目錄與 chunk 清單照 §7 檢查，檔案的大小等於 chunk
//!   的合計；巢狀超過深度上限回報、不深入。讀 item 時驗證失敗的副本也回報（讀取端會換下一份）。
//!
//! `--read-data`：map 與 delta 提到的每一個 pack 與 mpack 整份下載，名稱等於 BLAKE3、trailer 解得開、每一項
//! 解得開而 ID 相符；記錄（與 delta）的項目必須都在 trailer 裡、位移與長度相同（記錄 ⊆ trailer：P7c、P7d 只會
//! 從記錄移除項目）。mpack 的 `.r1` 與主體 bytes 相同；有 parity 的修得回自己。
//!
//! `--repair`：只做「只在不存在時建立」的修復（§1、§13）：主體不見而 `.r1` 完好的 head、分片、mpack 從 `.r1`
//! 建立主體；`.r1` 不見的 head（`replicas=1` 時還有分片、mpack、snapshot）從完好的主體建立 `.r1`。snapshot 的
//! 主體不補（§13：可能是 commit 前中斷）。在的、但損壞的物件 v4 不覆寫：有驗證過的完好 bytes（parity 修得回、
//! 或 `.r1` 完好）就列在 `repairable`，不寫回（寫回要先刪，見 ADR 026 規格錯誤第 17 項）。`--repair` 隱含
//! `--read-data`，寬鬆後端（`rclone://`）上拒絕（維護命令，§12）。

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use kist_backend::BackendError;
use kist_format::v4::ids::{ChunkId, ClientId, ItemId, MetaId, ObjectId};
use kist_format::v4::index::{status, Delta, Head};
use kist_format::v4::keys::{Gen, Key, DELTAS, GEN, MAP, MPACKS, PACKS, PARITY, SNAPSHOTS};
use kist_format::v4::meta::{check_dir_child, kind, node_type, Dir, Entry, Item};
use kist_format::v4::pack::{Kind, PackEntry};

use super::pack::{open_chunk, open_meta, trailer_of};
use super::read::Reader;
use super::repo::Repo;
use super::state::{pack_key, Generation, State};
use crate::{blocking, CoreError, Result, MAX_TREE_DEPTH};

#[derive(Debug, Clone, Copy, Default)]
pub struct CheckOptions {
    pub read_data: bool,
    /// 只在不存在時建立的修復（隱含 `read_data`）。
    pub repair: bool,
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct CheckReport {
    /// 讀取端用的世代（列出來的最大、讀得了的）。
    pub generation: Gen,
    pub snapshots: u64,
    /// 走過的 metadata item（目錄、chunk 清單、roots），每個 ID 算一次。
    pub items: u64,
    /// 被引用的 chunk，每個 ID 算一次。
    pub chunks: u64,
    /// map 與 delta 提到的 pack 與 mpack。
    pub packs: u64,
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
    /// 已建立的物件（`--repair`）。
    pub repaired: Vec<String>,
    /// 損壞，但有驗證過的完好 bytes（parity 修得回、或 `.r1` 完好）；v4 不覆寫，沒有寫回。
    pub repairable: Vec<String>,
    /// 損壞而修不了的物件。
    pub unrepairable: Vec<String>,
}

impl CheckReport {
    pub fn is_ok(&self) -> bool {
        self.errors.is_empty()
    }
}

/// 一份讀到的副本：bytes，或為什麼不能用。
type Copy = std::result::Result<Vec<u8>, String>;

struct Check {
    repo: Repo,
    opts: CheckOptions,
    report: CheckReport,
    /// 列出來的每一個物件 → 大小。
    listed: HashMap<String, u64>,
    /// 讀取端那一代的 settings 是 `replicas=1`。
    replicas: bool,
    seen_meta: HashSet<MetaId>,
    seen_chunks: HashSet<ChunkId>,
}

impl Repo {
    pub async fn check(&self, opts: CheckOptions) -> Result<CheckReport> {
        if opts.repair && !self.backend().create_only() {
            return Err(CoreError::Usage(format!(
                "{}: check --repair is a maintenance command, and this backend's conditional create \
                 is not atomic (format-v4.md §12)",
                self.backend().location()
            )));
        }
        let opts = CheckOptions {
            read_data: opts.read_data || opts.repair,
            ..opts
        };
        let repo = self.uncached();
        let mut listed = HashMap::new();
        for prefix in [SNAPSHOTS, DELTAS, PACKS, MPACKS, PARITY, MAP, GEN] {
            for o in repo.backend().list(prefix.trim_end_matches('/')).await? {
                listed.insert(o.key, o.size);
            }
        }
        let mut c = Check {
            repo,
            opts,
            report: CheckReport::default(),
            listed,
            replicas: false,
            seen_meta: HashSet::new(),
            seen_chunks: HashSet::new(),
        };
        c.heads().await?;
        let state = Arc::new(c.state().await?);
        let reader = Reader::new(c.repo.clone(), Arc::clone(&state), None).with_failure_log();
        c.snapshots(&reader).await?;
        c.report.items = c.seen_meta.len() as u64;
        c.report.chunks = c.seen_chunks.len() as u64;
        for f in reader.take_failures() {
            c.report.errors.push(f);
        }
        if c.opts.read_data {
            c.read_data(&state).await;
        }
        Ok(c.report)
    }
}

impl Check {
    fn error(&mut self, msg: String) {
        self.report.errors.push(msg);
    }

    fn warn(&mut self, msg: String) {
        self.report.warnings.push(msg);
    }

    fn is_listed(&self, key: &Key) -> bool {
        self.listed.contains_key(&key.path())
    }

    /// `--repair`：以只在不存在時建立的方式寫 `bytes`（呼叫端已驗證過）。「已存在」是別人先建了，算成功。
    async fn create(&mut self, key: &Key, bytes: Vec<u8>, from: &str) {
        if !self.opts.repair {
            return;
        }
        let path = key.path();
        match self.repo.backend().put_if_absent(&path, bytes).await {
            Ok(()) | Err(BackendError::AlreadyExists(_)) => self
                .report
                .repaired
                .push(format!("{path}: created from {from}")),
            Err(e) => self.error(format!("{path}: repairing it from {from} failed: {e}")),
        }
    }

    /// 以內容命名的物件的一份：列表裡沒有 → `None`；讀不了或名稱不符 → `Some(Err)`。
    async fn named_copy(&self, key: &Key, id: &ObjectId) -> Option<Copy> {
        if !self.is_listed(key) {
            return None;
        }
        Some(match self.repo.backend().get(&key.path()).await {
            Ok(b) if ObjectId::of(&b) == *id => Ok(b),
            Ok(_) => Err("content does not match its name".to_owned()),
            Err(e) => Err(e.to_string()),
        })
    }

    // —— head ——

    async fn head_copy(&self, g: Gen, key: &Key) -> std::result::Result<(Vec<u8>, Head), String> {
        let bytes = self
            .repo
            .backend()
            .get(&key.path())
            .await
            .map_err(|e| e.to_string())?;
        let plain = self
            .repo
            .keys()
            .open_head(g, &bytes)
            .map_err(|e| e.to_string())?;
        let head = Head::decode(&plain, g).map_err(|e| e.to_string())?;
        Ok((bytes, head))
    }

    /// 每一代的 head：兩份都在、解得開、相同；`parent` 接得上；沒有空洞。
    async fn heads(&mut self) -> Result<()> {
        let listing = self.repo.list_heads(None).await?;
        let mut expected: Gen = 0;
        let mut prev: Option<ObjectId> = None;
        for (&g, copies) in &listing {
            if g != expected {
                self.error(format!(
                    "generations {expected}..={} are missing (heads are never deleted)",
                    g - 1
                ));
                prev = None;
            }
            expected = g + 1;
            let body = Key::Head { g, replica: false };
            let r1 = Key::Head { g, replica: true };
            let b = if copies.body {
                Some(self.head_copy(g, &body).await)
            } else {
                None
            };
            let r = if copies.replica {
                Some(self.head_copy(g, &r1).await)
            } else {
                None
            };
            let good = match (b, r) {
                (Some(Ok(b)), Some(Ok(r))) => {
                    if b.0 != r.0 {
                        self.error(format!(
                            "{}: the body and its .r1 differ (two writers for one generation?)",
                            body.path()
                        ));
                    }
                    Some(b)
                }
                (Some(Ok(b)), None) => {
                    self.warn(format!(
                        "{}: has no .r1 (its writer stopped between the two writes)",
                        body.path()
                    ));
                    self.create(&r1, b.0.clone(), "the body").await;
                    Some(b)
                }
                (Some(Ok(b)), Some(Err(e))) => {
                    self.error(format!("{}: {e}", r1.path()));
                    self.report.unrepairable.push(r1.path());
                    Some(b)
                }
                (None, Some(Ok(r))) => {
                    self.error(format!("{}: missing; its .r1 is intact", body.path()));
                    self.create(&body, r.0.clone(), "its .r1").await;
                    Some(r)
                }
                (Some(Err(e)), Some(Ok(r))) => {
                    self.error(format!(
                        "{}: {e}; readers use its .r1 (heads are never deleted, so the body stays)",
                        body.path()
                    ));
                    self.report.unrepairable.push(body.path());
                    Some(r)
                }
                (b, r) => {
                    let why = |c: Option<std::result::Result<(Vec<u8>, Head), String>>| match c {
                        None => "missing".to_owned(),
                        Some(Err(e)) => e,
                        Some(Ok(_)) => "readable".to_owned(),
                    };
                    let msg = format!(
                        "generation {g} is unreadable: body {}; .r1 {}",
                        why(b),
                        why(r)
                    );
                    self.error(msg);
                    self.report.unrepairable.push(body.path());
                    None
                }
            };
            match good {
                Some((bytes, head)) => {
                    if let Some(p) = prev {
                        if head.parent != Some(p) {
                            self.error(format!(
                                "{}: its parent hash does not match generation {}",
                                body.path(),
                                g - 1
                            ));
                        }
                    }
                    prev = Some(ObjectId::of(&bytes));
                }
                None => prev = None,
            }
        }
        Ok(())
    }

    // —— 讀取端那一代 ——

    async fn state(&mut self) -> Result<State> {
        let (state, skipped) = self.repo.usable_state().await?;
        for s in skipped {
            self.error(format!(
                "{s}; readers fall back to an older generation, backup and prune stop"
            ));
        }
        self.report.generation = state.gen.g;
        self.replicas = state.gen.head.settings.replicas == 1;
        self.shards(&state.gen).await;
        self.records(&state).await;
        self.unrecorded(&state);
        self.deltas(&state).await;
        self.parity_orphans();
        Ok(state)
    }

    /// 這一代的分片：主體在、名稱相符；`replicas=1` 時 `.r1` 也是。
    async fn shards(&mut self, gen: &Generation) {
        for id in gen.head.shards.iter().flatten() {
            let body = Key::Shard {
                id: *id,
                replica: false,
            };
            let r1 = Key::Shard {
                id: *id,
                replica: true,
            };
            let b = self.named_copy(&body, id).await;
            let r = self.named_copy(&r1, id).await;
            self.two_copies(&body, &r1, b, r, self.replicas).await;
        }
    }

    /// 先寫主體再寫 `.r1` 的物件（分片、mpack）的兩份。`want_r1` 是 `.r1` 該不該在。
    async fn two_copies(
        &mut self,
        body: &Key,
        r1: &Key,
        b: Option<Copy>,
        r: Option<Copy>,
        want_r1: bool,
    ) {
        let (bp, rp) = (body.path(), r1.path());
        match (b, r) {
            (Some(Ok(_)), Some(Ok(_))) => {}
            (Some(Ok(b)), None) => {
                if want_r1 {
                    self.warn(format!("{bp}: has no .r1 (replicas=1)"));
                    self.create(r1, b, "the body").await;
                }
            }
            (Some(Ok(_)), Some(Err(e))) => {
                self.error(format!("{rp}: {e}"));
                self.report.unrepairable.push(rp);
            }
            (None, Some(Ok(r))) => {
                self.error(format!("{bp}: missing; its .r1 is intact"));
                self.create(body, r, "its .r1").await;
            }
            (Some(Err(e)), Some(Ok(_))) => {
                self.error(format!("{bp}: {e}; its .r1 is intact"));
                self.report
                    .repairable
                    .push(format!("{bp}: its .r1 is intact"));
            }
            (None, None) => {
                self.error(format!("{bp}: missing"));
                self.report.unrepairable.push(bp);
            }
            (b, r) => {
                let why = |c: Option<Copy>| match c {
                    None => "missing".to_owned(),
                    Some(Err(e)) => e,
                    Some(Ok(_)) => "intact".to_owned(),
                };
                let msg = format!("{bp}: body {}; .r1 {}", why(b), why(r));
                self.error(msg);
                self.report.unrepairable.push(bp);
            }
        }
    }

    /// 記錄與列表（P3）：live 與 condemned 的 pack 要在、大小相符；mpack 的 `.r1`。
    async fn records(&mut self, state: &State) {
        self.report.packs += state.map.records().len() as u64;
        for r in state.map.records() {
            let body = pack_key(r.kind, r.client, r.id);
            let path = body.path();
            if r.by_prune && r.client != ClientId::MAINT {
                self.error(format!(
                    "{path}: recorded as written by prune but not under the maintenance prefix"
                ));
            }
            match r.status.s {
                status::LIVE | status::CONDEMNED => {}
                status::UNREADABLE => {
                    self.warn(format!(
                        "{path}: unreadable (nobody uses it; prune condemns it)"
                    ));
                    continue;
                }
                status::UNKNOWN => {
                    self.warn(format!(
                        "{path}: written by a newer format version; kept until a maintenance \
                         command removes it"
                    ));
                    continue;
                }
                _ => continue,
            }
            match self.listed.get(&path).copied() {
                Some(size) if size != r.size => self.error(format!(
                    "{path}: {size} bytes, but its record says {}",
                    r.size
                )),
                Some(_) => {}
                None if r.kind == Kind::Data => {
                    self.error(format!("{path}: missing (data loss)"));
                    self.report.unrepairable.push(path.clone());
                }
                None => {}
            }
            if r.kind == Kind::Meta {
                let r1 = Key::Mpack {
                    client: r.client,
                    id: r.id,
                    replica: true,
                };
                let body_listed = self.is_listed(&body);
                let r1_listed = self.is_listed(&r1);
                // 只在要修或要判斷時才下載：兩份都在、或不要 .r1 時不必
                if !body_listed || (self.replicas && !r1_listed) {
                    let b = self.named_copy(&body, &r.id).await;
                    let c = self.named_copy(&r1, &r.id).await;
                    self.two_copies(&body, &r1, b, c, self.replicas).await;
                }
            }
        }
    }

    /// 列表裡有、map 沒有記錄的 pack：delta 提到的是還沒收錄（正常）；都沒提到的是中斷的 backup 留下的。
    fn unrecorded(&mut self, state: &State) {
        let mut found = Vec::new();
        for key in self.listed.keys() {
            let id = match Key::parse(key) {
                Ok(Key::Pack { id, .. }) => id,
                // mpack 的 .r1：主體也在列表裡時只回報主體一次
                Ok(Key::Mpack {
                    client,
                    id,
                    replica: true,
                }) if self.is_listed(&Key::Mpack {
                    client,
                    id,
                    replica: false,
                }) =>
                {
                    continue
                }
                Ok(Key::Mpack { id, .. }) => id,
                _ => continue,
            };
            if state.map.record(&id).is_none() && !state.deltas.contains(&id) {
                found.push(key.clone());
            }
        }
        found.sort();
        for key in found {
            self.warn(format!(
                "{key}: in no map record and no delta (an interrupted backup); the next prune \
                 indexes it"
            ));
        }
    }

    /// 解不開的 delta（讀取端跳過）；delta 提到、卻不在列表裡的 pack。
    async fn deltas(&mut self, state: &State) {
        let mut keys: Vec<String> = self
            .listed
            .keys()
            .filter(|k| k.starts_with(DELTAS))
            .cloned()
            .collect();
        keys.sort();
        for path in keys {
            let Ok(Key::Delta { client, id }) = Key::parse(&path) else {
                self.warn(format!("{path}: not a v4 delta key"));
                continue;
            };
            let decoded = async {
                let bytes = self
                    .repo
                    .backend()
                    .get(&path)
                    .await
                    .map_err(|e| e.to_string())?;
                if ObjectId::of(&bytes) != id {
                    return Err("content does not match its name".to_owned());
                }
                let plain = self
                    .repo
                    .keys()
                    .open_delta(&client, &bytes)
                    .map_err(|e| e.to_string())?;
                Delta::decode(&plain, &client).map_err(|e| e.to_string())
            }
            .await;
            if let Err(e) = decoded {
                self.warn(format!("{path}: {e}; readers skip it"));
            }
        }
        self.report.packs += state.deltas.packs().count() as u64;
        let mut missing = Vec::new();
        for (client, p) in state.deltas.packs() {
            let body = pack_key(p.kind, *client, p.id);
            let r1 = Key::Mpack {
                client: *client,
                id: p.id,
                replica: true,
            };
            if !self.is_listed(&body) && !(p.kind == Kind::Meta && self.is_listed(&r1)) {
                missing.push(body.path());
            }
        }
        for path in missing {
            self.warn(format!(
                "{path}: a delta lists it, but it is not in the repository"
            ));
        }
    }

    /// 對應的 pack 已不在的 parity（prune 在一個 grace 之後刪）。
    fn parity_orphans(&mut self) {
        let mut found = Vec::new();
        for key in self.listed.keys() {
            let Ok(Key::Parity { client, id }) = Key::parse(key) else {
                continue;
            };
            let pack = Key::Pack { client, id };
            let mpack = Key::Mpack {
                client,
                id,
                replica: false,
            };
            if !self.is_listed(&pack) && !self.is_listed(&mpack) {
                found.push(key.clone());
            }
        }
        found.sort();
        for key in found {
            self.warn(format!("{key}: its pack is gone"));
        }
    }

    // —— snapshot ——

    async fn snapshots(&mut self, reader: &Reader) -> Result<()> {
        let mut snaps: Vec<(Key, bool, bool)> = Vec::new();
        let mut index: HashMap<String, usize> = HashMap::new();
        let mut keys: Vec<&String> = self
            .listed
            .keys()
            .filter(|k| k.starts_with(SNAPSHOTS))
            .collect();
        keys.sort();
        let mut odd = Vec::new();
        for k in keys {
            match Key::parse(k) {
                Ok(Key::Snapshot {
                    client,
                    ts,
                    replica,
                }) => {
                    let body = Key::Snapshot {
                        client,
                        ts,
                        replica: false,
                    };
                    let i = *index.entry(body.path()).or_insert_with(|| {
                        snaps.push((body, false, false));
                        snaps.len() - 1
                    });
                    if replica {
                        snaps[i].2 = true;
                    } else {
                        snaps[i].1 = true;
                    }
                }
                _ => odd.push(k.clone()),
            }
        }
        for k in odd {
            self.warn(format!("{k}: not a v4 snapshot key"));
        }
        self.report.snapshots = snaps.len() as u64;
        for (body, has_body, has_r1) in snaps {
            let r1 = match &body {
                Key::Snapshot { client, ts, .. } => Key::Snapshot {
                    client: *client,
                    ts: ts.clone(),
                    replica: true,
                },
                _ => continue,
            };
            let path = body.path();
            let snap = if has_body {
                match self.repo.read_snapshot_copy(&body).await {
                    Ok(s) => {
                        if !has_r1 && self.replicas {
                            self.warn(format!("{path}: has no .r1 (replicas=1)"));
                            if let Ok(bytes) = self.repo.backend().get(&path).await {
                                self.create(&r1, bytes, "the body").await;
                            }
                        }
                        Some(s)
                    }
                    Err(e) if has_r1 => match self.repo.read_snapshot_copy(&r1).await {
                        Ok(s) => {
                            self.error(format!("{path}: {e}; its .r1 is intact"));
                            self.report
                                .repairable
                                .push(format!("{path}: its .r1 is intact"));
                            Some(s)
                        }
                        Err(e2) => {
                            self.error(format!("{path}: body {e}; .r1 {e2}"));
                            self.report.unrepairable.push(path.clone());
                            None
                        }
                    },
                    Err(e) => {
                        self.error(format!("{path}: {e}"));
                        self.report.unrepairable.push(path.clone());
                        None
                    }
                }
            } else {
                // §13：先寫 .r1 再寫主體，所以可能是 commit 前中斷，也可能是主體遺失；prune 照樣當成起點（P5）
                self.warn(format!(
                    "{path}: only its .r1 exists (a backup interrupted before its commit, or a lost \
                     body); prune keeps what it references. Forget it, or restore from the .r1"
                ));
                match self.repo.read_snapshot_copy(&r1).await {
                    Ok(s) => Some(s),
                    Err(e) => {
                        self.error(format!("{}: {e}", r1.path()));
                        None
                    }
                }
            };
            let Some(snap) = snap else { continue };
            if !self.held(reader, snap.roots.into(), Kind::Meta, &path) {
                continue;
            }
            self.seen_meta.insert(snap.roots);
            let roots = match reader.roots(&snap.roots).await {
                Ok(r) => r,
                Err(e) => {
                    self.error(format!("{path}: roots: {e}"));
                    continue;
                }
            };
            for root in roots {
                let at = format!("{path}:{}", String::from_utf8_lossy(&root.path));
                match (root.t, root.tree, root.entry) {
                    (node_type::DIR, Some(tree), _) => self.tree(reader, tree, 0, at).await,
                    (node_type::FILE, _, Some(e)) => self.file(reader, &e, &at).await,
                    _ => {}
                }
            }
        }
        Ok(())
    }

    /// P5 的持有者：可讀、非 tombstone、種類相符的至少一個。沒有任何持有者就讀不了（回 false）。
    fn held(&mut self, reader: &Reader, id: ItemId, kind: Kind, at: &str) -> bool {
        let holders = reader.state.read_candidates(&id, kind);
        if holders.is_empty() {
            self.error(format!("{at}: {id} has no holder"));
            return false;
        }
        if holders.iter().all(|h| h.status == status::TOMBSTONE) {
            self.error(format!(
                "{at}: {id} is held only by tombstoned packs, which prune deletes"
            ));
        }
        true
    }

    /// 一個目錄（Seg 或 Dir）。`depth` 是它的巢狀深度（root 的目錄是 0）。
    fn tree<'a>(
        &'a mut self,
        reader: &'a Reader,
        id: MetaId,
        depth: usize,
        at: String,
    ) -> Pin<Box<dyn Future<Output = ()> + 'a>> {
        Box::pin(async move {
            if !self.seen_meta.insert(id) || !self.held(reader, id.into(), Kind::Meta, &at) {
                return;
            }
            match reader.item(&id).await {
                Ok(Item::Seg(s)) => {
                    for e in s.entries {
                        self.entry(reader, e, depth, &at).await;
                    }
                }
                Ok(Item::Dir(d)) => self.index(reader, d, depth, at).await,
                Ok(_) => self.error(format!("{at}: {id} is not a directory segment or index")),
                Err(e) => self.error(format!("{at}: {e}")),
            }
        })
    }

    /// 目錄的索引：每一段載入之後照 §7 檢查（第一個名稱、不越過下一段、種類與層級）。
    fn index<'a>(
        &'a mut self,
        reader: &'a Reader,
        dir: Dir,
        depth: usize,
        at: String,
    ) -> Pin<Box<dyn Future<Output = ()> + 'a>> {
        Box::pin(async move {
            for i in 0..dir.segs.len() {
                let id = dir.segs[i].id;
                if !self.seen_meta.insert(id) || !self.held(reader, id.into(), Kind::Meta, &at) {
                    continue;
                }
                let child = match reader.item(&id).await {
                    Ok(c) => c,
                    Err(e) => {
                        self.error(format!("{at}: {e}"));
                        continue;
                    }
                };
                if let Err(e) = check_dir_child(&dir, i, &child, kind::SEG) {
                    self.error(format!("{at}: {e}"));
                    continue;
                }
                match child {
                    Item::Seg(s) => {
                        for e in s.entries {
                            self.entry(reader, e, depth, &at).await;
                        }
                    }
                    Item::Dir(d) => self.index(reader, d, depth, at.clone()).await,
                    _ => {}
                }
            }
        })
    }

    async fn entry(&mut self, reader: &Reader, e: Entry, depth: usize, at: &str) {
        let path = format!("{at}/{}", String::from_utf8_lossy(&e.n));
        match (e.t, e.tree) {
            (node_type::DIR, Some(tree)) => {
                if depth + 1 >= MAX_TREE_DEPTH {
                    self.error(format!(
                        "{path}: nesting deeper than {MAX_TREE_DEPTH} levels (corrupt or hostile \
                         repository)"
                    ));
                } else {
                    self.tree(reader, tree, depth + 1, path).await;
                }
            }
            (node_type::FILE, _) => self.file(reader, &e, &path).await,
            _ => {}
        }
    }

    /// 檔案：chunk 清單的每一個 item 與每一個 chunk 都有持有者；§7 的大小檢查（每段的 size、合計等於 Entry）。
    async fn file(&mut self, reader: &Reader, e: &Entry, at: &str) {
        let mut lists = Vec::new();
        match reader.file_chunks_visit(e, &mut |id| lists.push(id)).await {
            Ok(chunks) => {
                for id in lists {
                    if self.seen_meta.insert(id) {
                        self.held(reader, id.into(), Kind::Meta, at);
                    }
                }
                for id in chunks {
                    if self.seen_chunks.insert(id) {
                        self.held(reader, id.into(), Kind::Data, at);
                    }
                }
            }
            Err(err) => {
                for id in lists {
                    self.seen_meta.insert(id);
                }
                self.error(format!("{at}: {err}"));
            }
        }
    }

    // —— 讀資料 ——

    /// map 與 delta 提到的每一個 pack 與 mpack（tombstone 也讀：它可能是最後的那一份）。
    async fn read_data(&mut self, state: &State) {
        let mut todo: Vec<(Kind, ClientId, ObjectId, Vec<PackEntry>, &'static str)> = Vec::new();
        for r in state.map.records() {
            if matches!(
                r.status.s,
                status::LIVE | status::CONDEMNED | status::TOMBSTONE
            ) {
                todo.push((r.kind, r.client, r.id, r.entries.clone(), "its record"));
            }
        }
        for (c, p) in state.deltas.packs() {
            todo.push((p.kind, *c, p.id, p.entries.clone(), "its delta"));
        }
        for (kind, client, id, claimed, source) in todo {
            self.pack_data(kind, client, id, claimed, source).await;
        }
    }

    async fn pack_data(
        &mut self,
        kind: Kind,
        client: ClientId,
        id: ObjectId,
        claimed: Vec<PackEntry>,
        source: &'static str,
    ) {
        let key = pack_key(kind, client, id);
        let path = key.path();
        if !self.is_listed(&key) {
            // 不見的已在前面回報（P3、delta）
            return;
        }
        let bytes = match self.repo.backend().get(&path).await {
            Ok(b) => b,
            Err(e) => {
                self.error(format!("{path}: {e}"));
                return;
            }
        };
        let (bytes, intact) = match blocking(move || {
            let ok = ObjectId::of(&bytes) == id;
            Ok((bytes, ok))
        })
        .await
        {
            Ok(r) => r,
            Err(e) => {
                self.error(format!("{path}: {e}"));
                return;
            }
        };
        if !intact {
            self.error(format!("{path}: content does not match its name"));
            match self.repair_proof(kind, client, id, bytes).await {
                Some(how) => self.report.repairable.push(format!("{path}: {how}")),
                None => self.report.unrepairable.push(path),
            }
            return;
        }
        let keys = Arc::clone(self.repo.keys());
        let chunk_max = self.repo.config().chunker.max;
        let bytes = Arc::new(bytes);
        let task_bytes = Arc::clone(&bytes);
        let task_path = path.clone();
        let checked = blocking(move || {
            let trailer = trailer_of(&keys, &key, &task_bytes, chunk_max)?;
            let mut errs = Vec::new();
            let mut bad = HashSet::new();
            let mut in_trailer = HashMap::new();
            for e in &trailer.entries {
                in_trailer.insert(e.i, *e);
                let sealed = usize::try_from(e.o)
                    .ok()
                    .zip(usize::try_from(e.o + e.l).ok())
                    .and_then(|(a, b)| task_bytes.get(a..b));
                let opened = match (sealed, kind) {
                    (None, _) => Err(CoreError::corrupt(&task_path, "entry outside the pack")),
                    (Some(s), Kind::Data) => {
                        open_chunk(&keys, &e.i.to_chunk(), s, e.r, u64::from(chunk_max)).map(|_| ())
                    }
                    (Some(s), Kind::Meta) => open_meta(&keys, &e.i.to_meta(), s, e.r).map(|_| ()),
                };
                if opened.is_err() {
                    bad.insert(e.i);
                }
            }
            for c in &claimed {
                match in_trailer.get(&c.i) {
                    None => errs.push(format!(
                        "{task_path}: {source} lists {} but the trailer does not",
                        c.i
                    )),
                    Some(t) if t != c => errs.push(format!(
                        "{task_path}: {source} places {} differently from the trailer",
                        c.i
                    )),
                    Some(_) if bad.contains(&c.i) => errs.push(format!(
                        "{task_path}: {} fails verification, and {source} still lists it",
                        c.i
                    )),
                    Some(_) => {}
                }
            }
            Ok(errs)
        })
        .await;
        match checked {
            Ok(errs) => self.report.errors.extend(errs),
            Err(e) => self.error(format!("{path}: {e}")),
        }
        if kind == Kind::Meta {
            let r1 = Key::Mpack {
                client,
                id,
                replica: true,
            };
            if let Some(Err(e)) = self.named_copy(&r1, &id).await {
                self.error(format!("{}: {e}", r1.path()));
                self.report.unrepairable.push(r1.path());
            }
        }
        let parity = Key::Parity { client, id };
        if self.is_listed(&parity) {
            let repaired = match self.repo.backend().get(&parity.path()).await {
                Ok(raw) => {
                    let bytes = Arc::clone(&bytes);
                    blocking(move || {
                        let obj = kist_format::v4::parity::parse(&raw)?;
                        Ok(obj.repair(&id, &bytes)?)
                    })
                    .await
                    .map(|_| ())
                }
                Err(e) => Err(e.into()),
            };
            if let Err(e) = repaired {
                self.warn(format!("{}: cannot repair its pack: {e}", parity.path()));
            }
        }
    }

    /// 損壞的 pack 或 mpack：parity 修得回（重算 BLAKE3 等於名稱，§13），或 mpack 的 `.r1` 完好，就有驗證過的完好
    /// bytes。v4 不覆寫，只回報。
    async fn repair_proof(
        &self,
        kind: Kind,
        client: ClientId,
        id: ObjectId,
        damaged: Vec<u8>,
    ) -> Option<String> {
        let parity = Key::Parity { client, id };
        if self.is_listed(&parity) {
            if let Ok(raw) = self.repo.backend().get(&parity.path()).await {
                let repaired = blocking(move || {
                    let obj = kist_format::v4::parity::parse(&raw)?;
                    Ok(obj.repair(&id, &damaged)?)
                })
                .await;
                if repaired.is_ok() {
                    return Some("its parity repairs it (verified by its name)".to_owned());
                }
            }
        }
        if kind == Kind::Meta {
            let r1 = Key::Mpack {
                client,
                id,
                replica: true,
            };
            if let Some(Ok(_)) = self.named_copy(&r1, &id).await {
                return Some("its .r1 is intact".to_owned());
            }
        }
        None
    }
}
