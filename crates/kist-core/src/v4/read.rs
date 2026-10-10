//! 從持有者讀一項（§10）：依排序逐一嘗試，某一份驗證失敗（解不開、解壓超過上限、長度或 ID 不符）就換下
//! 一份；tombstone 的持有者排最後，用到時警告。
//!
//! mpack 一律整份讀（≤ `mpack_target` 加一項），放進記憶體裡有上限的 [`Mpacks`]，有本地快取就也存一份：
//! 走訪 parent 的目錄樹時，請求數是用到的 mpack 數，不是目錄數（需求 R5）。資料 pack 以 range read 讀一項。
//!
//! 第 4 段的 backup 用它讀 parent 的樹、做 B2 的取回驗證與 B4 的 trailer 檢查；restore、mount 與 check 在第
//! 5 段沿用。

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};

use kist_format::v4::ids::{ChunkId, ClientId, ItemId, MetaId, ObjectId};
use kist_format::v4::keys::Key;
use kist_format::v4::meta::{
    check_dir_child, check_file_size, check_index_child, content_type, decode_item, kind, Dir,
    Entry, Item,
};
use kist_format::v4::pack::{self as fpack, Kind, PackTrailer};
use kist_format::v4::snapshot::Root;

use super::cache::Kind as CacheKind;
use super::pack::{decode_trailer, max_r, open_chunk, open_meta};
use super::repo::Repo;
use super::state::{Holder, State};
use crate::{CoreError, Result};

/// 記憶體裡的 mpack（整份 bytes），超過上限時丟掉最久沒用的。
pub struct Mpacks {
    inner: Mutex<Lru>,
}

struct Lru {
    cap: usize,
    used: usize,
    order: VecDeque<ObjectId>,
    map: HashMap<ObjectId, Arc<Vec<u8>>>,
}

impl Mpacks {
    pub fn new(cap: usize) -> Self {
        Self {
            inner: Mutex::new(Lru {
                cap,
                used: 0,
                order: VecDeque::new(),
                map: HashMap::new(),
            }),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Lru> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn get(&self, id: &ObjectId) -> Option<Arc<Vec<u8>>> {
        let mut l = self.lock();
        let bytes = l.map.get(id).cloned()?;
        l.order.retain(|x| x != id);
        l.order.push_back(*id);
        Some(bytes)
    }

    /// 剛寫出的 mpack 也放進來：下一個目錄常常就在同一個 mpack 裡。
    pub fn insert(&self, id: ObjectId, bytes: Arc<Vec<u8>>) {
        let mut l = self.lock();
        if l.map.contains_key(&id) {
            return;
        }
        l.used += bytes.len();
        l.map.insert(id, bytes);
        l.order.push_back(id);
        while l.used > l.cap && l.order.len() > 1 {
            if let Some(old) = l.order.pop_front() {
                if let Some(b) = l.map.remove(&old) {
                    l.used -= b.len();
                }
            }
        }
    }
}

/// 記憶體裡 mpack 的上限。
pub const MPACK_MEMORY: usize = 64 * 1024 * 1024;

/// 讀取端：一個世代的狀態加上讀 mpack 的快取。`own` 是這台 client。
///
/// 也記著這台 client 驗證過哪些 (item, pack)（B2 的第三種可信）：開始時從本地快取載入，之後每次從別台的副本
/// 讀到並驗證成功就加一筆；backup 結束時把新的寫回快取。自己前綴下的與 `by_prune` 的持有者不記：它們本來就
/// 可信。
#[derive(Clone)]
pub struct Reader {
    pub repo: Repo,
    pub state: Arc<State>,
    pub mpacks: Arc<Mpacks>,
    own: Option<ClientId>,
    verified: Arc<Mutex<Verified>>,
}

#[derive(Default)]
struct Verified {
    known: HashSet<(ItemId, ObjectId)>,
    new: Vec<(ItemId, ObjectId)>,
}

impl Reader {
    pub fn new(repo: Repo, state: Arc<State>, own: Option<ClientId>) -> Self {
        Self {
            repo,
            state,
            mpacks: Arc::new(Mpacks::new(MPACK_MEMORY)),
            own,
            verified: Arc::new(Mutex::new(Verified::default())),
        }
    }

    /// 開始時從本地快取載入的驗證記錄。
    pub fn with_verified(self, known: HashSet<(ItemId, ObjectId)>) -> Self {
        self.lock_verified().known = known;
        self
    }

    fn lock_verified(&self) -> std::sync::MutexGuard<'_, Verified> {
        self.verified.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// B2 的「可信」：`by_prune`、在自己的前綴下、或這台 client 取回驗證過。
    pub fn trusted(&self, id: &ItemId, h: &Holder) -> bool {
        h.by_prune
            || Some(h.client) == self.own
            || self.lock_verified().known.contains(&(*id, h.pack))
    }

    /// 從這個持有者讀到 `id` 並驗證成功。
    pub fn note_verified(&self, id: ItemId, h: &Holder) {
        if h.by_prune || Some(h.client) == self.own {
            return;
        }
        let mut v = self.lock_verified();
        if v.known.insert((id, h.pack)) {
            v.new.push((id, h.pack));
        }
    }

    /// 這次新驗證的 (item, pack)，寫回本地快取用。
    pub fn take_new_verified(&self) -> Vec<(ItemId, ObjectId)> {
        std::mem::take(&mut self.lock_verified().new)
    }

    /// 一個 mpack 的整份 bytes：記憶體、本地快取、repo 的主體，不行讀 `.r1`。名稱一律重算。
    pub async fn mpack(&self, client: ClientId, id: ObjectId) -> Result<Arc<Vec<u8>>> {
        if let Some(b) = self.mpacks.get(&id) {
            return Ok(b);
        }
        if let Some(b) = self.repo.cache().and_then(|c| c.get(CacheKind::Mpack, &id)) {
            let b = Arc::new(b);
            self.mpacks.insert(id, Arc::clone(&b));
            return Ok(b);
        }
        let mut reasons = Vec::new();
        for replica in [false, true] {
            let key = Key::Mpack {
                client,
                id,
                replica,
            }
            .path();
            match self.repo.backend().get(&key).await {
                Ok(bytes) if ObjectId::of(&bytes) == id => {
                    if let Some(c) = self.repo.cache() {
                        c.put(CacheKind::Mpack, &id, &bytes);
                    }
                    let b = Arc::new(bytes);
                    self.mpacks.insert(id, Arc::clone(&b));
                    return Ok(b);
                }
                Ok(_) => reasons.push(format!("{key}: content does not match its name")),
                Err(e) => reasons.push(format!("{key}: {e}")),
            }
        }
        Err(CoreError::corrupt(
            Key::Mpack {
                client,
                id,
                replica: false,
            }
            .path(),
            reasons.join("; "),
        ))
    }

    /// 從一個持有者取回一項並驗證：chunk 回明文，metadata item 回規範 CBOR。
    pub async fn fetch(&self, h: &Holder) -> Result<Vec<u8>> {
        let e = h.entry;
        // 記錄與 delta 裡的位移不受格式層檢查（§10）：溢位或超出範圍就是這一份不可信
        let end = e.o.checked_add(e.l).ok_or_else(|| {
            CoreError::corrupt(h.key().path(), format!("entry {}+{} overflows", e.o, e.l))
        })?;
        match h.kind {
            Kind::Data => {
                let key = h.key().path();
                let sealed = self.repo.backend().get_range(&key, e.o..end).await?;
                let max = u64::from(self.repo.config().chunker.max);
                open_chunk(self.repo.keys(), &e.i.to_chunk(), &sealed, e.r, max)
            }
            Kind::Meta => {
                let bytes = self.mpack(h.client, h.pack).await?;
                let sealed = usize::try_from(e.o)
                    .ok()
                    .zip(usize::try_from(end).ok())
                    .and_then(|(start, end)| bytes.get(start..end))
                    .ok_or_else(|| {
                        CoreError::corrupt(
                            h.key().path(),
                            format!("entry {}..{end} is outside the mpack", e.o),
                        )
                    })?;
                open_meta(self.repo.keys(), &e.i.to_meta(), sealed, e.r)
            }
        }
    }

    /// 依讀取順序嘗試每個持有者；全部失敗就是錯誤，訊息帶每一份的原因。
    async fn read(&self, id: ItemId, kind: Kind) -> Result<Vec<u8>> {
        let holders = self.state.read_candidates(&id, kind);
        if holders.is_empty() {
            return Err(CoreError::corrupt(
                format!("{id}"),
                "no holder in the map or the deltas",
            ));
        }
        let mut reasons = Vec::new();
        for h in &holders {
            if h.status == kist_format::v4::index::status::TOMBSTONE {
                tracing::warn!(
                    "{id}: every other holder failed; reading tombstoned pack {}",
                    h.pack
                );
            }
            match self.fetch(h).await {
                Ok(plain) => {
                    self.note_verified(id, h);
                    return Ok(plain);
                }
                Err(e) => {
                    tracing::warn!("{id}: holder {} failed verification: {e}", h.pack);
                    reasons.push(format!("{}: {e}", h.pack));
                }
            }
        }
        Err(CoreError::corrupt(format!("{id}"), reasons.join("; ")))
    }

    pub async fn chunk(&self, id: &ChunkId) -> Result<Vec<u8>> {
        self.read((*id).into(), Kind::Data).await
    }

    /// 讀一個 metadata item 並解碼（含它自己的結構檢查）。
    pub async fn item(&self, id: &MetaId) -> Result<Item> {
        let cbor = self.read((*id).into(), Kind::Meta).await?;
        Ok(decode_item(&cbor)?)
    }

    /// 把一棵 Dir 展開成依序的第一層 (Dir, 第 i 段)：上層的每一段載入後照 §7 檢查。
    async fn expand(&self, top: Dir, leaf: u8) -> Result<VecDeque<(Arc<Dir>, usize)>> {
        let mut out = VecDeque::new();
        let mut stack = vec![(Arc::new(top), 0usize)];
        while let Some((dir, i)) = stack.pop() {
            if dir.level == 1 {
                out.extend((0..dir.segs.len()).map(|i| (Arc::clone(&dir), i)));
                continue;
            }
            if i >= dir.segs.len() {
                continue;
            }
            stack.push((Arc::clone(&dir), i + 1));
            let child = self.item(&dir.segs[i].id).await?;
            check_dir_child(&dir, i, &child, leaf)?;
            match child {
                Item::Dir(d) => stack.push((Arc::new(d), 0)),
                _ => {
                    return Err(CoreError::corrupt(
                        format!("{}", dir.segs[i].id),
                        "expected a directory index",
                    ))
                }
            }
        }
        Ok(out)
    }

    /// 一個目錄的內容（`tree` 指向 Seg 或 Dir），依名稱一段一段讀。
    pub async fn dir(&self, tree: &MetaId) -> Result<DirStream> {
        match self.item(tree).await? {
            Item::Seg(s) => Ok(DirStream {
                pending: VecDeque::new(),
                current: s.entries.into_iter().peekable(),
            }),
            Item::Dir(d) => Ok(DirStream {
                pending: self.expand(d, kind::SEG).await?,
                current: Vec::new().into_iter().peekable(),
            }),
            _ => Err(CoreError::corrupt(
                format!("{tree}"),
                "a directory tree must be a segment or a directory index",
            )),
        }
    }

    /// 載入第 i 段並照 §7 檢查。
    async fn segment(&self, dir: &Dir, i: usize, leaf: u8) -> Result<Item> {
        let child = self.item(&dir.segs[i].id).await?;
        check_dir_child(dir, i, &child, leaf)?;
        Ok(child)
    }

    /// snapshot 的 roots（RootSeg 或 Dir）全部讀出來，依 path 升冪。
    pub async fn roots(&self, roots: &MetaId) -> Result<Vec<Root>> {
        match self.item(roots).await? {
            Item::RootSeg(s) => Ok(s.roots),
            Item::Dir(d) => {
                let mut out = Vec::new();
                for (dir, i) in self.expand(d, kind::ROOT_SEG).await? {
                    match self.segment(&dir, i, kind::ROOT_SEG).await? {
                        Item::RootSeg(s) => out.extend(s.roots),
                        _ => {
                            return Err(CoreError::corrupt(
                                format!("{roots}"),
                                "expected a roots segment",
                            ))
                        }
                    }
                }
                Ok(out)
            }
            _ => Err(CoreError::corrupt(
                format!("{roots}"),
                "roots must be a roots segment or a directory index",
            )),
        }
    }

    /// 檔案的 chunk 清單（依內容順序），連同 §7 的大小檢查：每一段的 `size` 等於它涵蓋的 chunk 明文長度
    /// （取自持有者的 `r`）、全部等於 Entry 的 `size`。
    pub async fn file_chunks(&self, entry: &Entry) -> Result<Vec<ChunkId>> {
        let chunk_len = |id: &ChunkId| -> Result<u64> {
            self.state
                .read_candidates(&(*id).into(), Kind::Data)
                .first()
                .map(|h| h.entry.r)
                .ok_or_else(|| {
                    CoreError::corrupt(format!("chunk {id}"), "no holder in the map or the deltas")
                })
        };
        let ids: Vec<ChunkId> = if entry.ct == content_type::DIRECT {
            entry.chunks.iter().map(|c| c.to_chunk()).collect()
        } else {
            let root = entry
                .chunks
                .first()
                .ok_or_else(|| CoreError::corrupt("<entry>", "an indirect file without a list"))?
                .to_meta();
            let mut out = Vec::new();
            // (索引, 第 i 段) 的堆疊；ChunkList 直接收下
            let mut stack: Vec<(Arc<kist_format::v4::meta::ChunkIndex>, usize)> = Vec::new();
            match self.item(&root).await? {
                Item::ChunkList(l) => out = l.chunks,
                Item::ChunkIndex(x) => stack.push((Arc::new(x), 0)),
                _ => {
                    return Err(CoreError::corrupt(
                        format!("{root}"),
                        "expected a chunk list",
                    ))
                }
            }
            while let Some((index, i)) = stack.pop() {
                if i >= index.parts.len() {
                    continue;
                }
                stack.push((Arc::clone(&index), i + 1));
                let child = self.item(&index.parts[i].id).await?;
                match &child {
                    Item::ChunkList(l) => {
                        let covered = sum_len(&l.chunks, &chunk_len)?;
                        check_index_child(&index, i, &child, Some(covered))?;
                        out.extend(l.chunks.iter().copied());
                    }
                    Item::ChunkIndex(x) => {
                        check_index_child(&index, i, &child, None)?;
                        stack.push((Arc::new(x.clone()), 0));
                    }
                    _ => {
                        return Err(CoreError::corrupt(
                            format!("{}", index.parts[i].id),
                            "expected a chunk list or index",
                        ))
                    }
                }
            }
            out
        };
        check_file_size(entry, sum_len(&ids, &chunk_len)?)?;
        Ok(ids)
    }

    /// B4：一個 pack 的 trailer，直接從 repo 讀（HEAD 取大小、range 讀檔尾與 trailer），照 §6 檢查。
    pub async fn trailer(&self, key: &Key) -> Result<PackTrailer> {
        let path = key.path();
        let kind = fpack::Kind::of_key(key)
            .ok_or_else(|| CoreError::corrupt(&path, "not a pack or mpack key"))?;
        let size = self.repo.backend().head(&path).await?.size;
        let footer_len = fpack::FOOTER_LEN as u64;
        if size < (fpack::HEADER_LEN as u64) + footer_len {
            return Err(CoreError::corrupt(
                &path,
                format!("{size} bytes is too short"),
            ));
        }
        let footer = self
            .repo
            .backend()
            .get_range(&path, size - footer_len..size)
            .await?;
        let len =
            fpack::parse_footer(&footer).map_err(|e| CoreError::corrupt(&path, e.to_string()))?;
        let start = (size - footer_len)
            .checked_sub(len)
            .filter(|s| *s >= fpack::HEADER_LEN as u64)
            .ok_or_else(|| {
                CoreError::corrupt(&path, format!("trailer length {len} does not fit"))
            })?;
        let sealed = self
            .repo
            .backend()
            .get_range(&path, start..size - footer_len)
            .await?;
        decode_trailer(
            self.repo.keys(),
            key,
            start,
            &sealed,
            max_r(kind, self.repo.config().chunker.max),
        )
    }
}

/// chunk 明文長度的合計；溢位是腐壞（長度取自不受格式層檢查的記錄）。
fn sum_len(ids: &[ChunkId], len: &dyn Fn(&ChunkId) -> Result<u64>) -> Result<u64> {
    ids.iter().try_fold(0u64, |acc, c| {
        acc.checked_add(len(c)?)
            .ok_or_else(|| CoreError::corrupt("<chunk list>", "chunk sizes overflow"))
    })
}

/// 一個目錄的內容，依名稱一段一段讀（backup 對 parent 做 merge-join 用）：一次只放一段在記憶體。
pub struct DirStream {
    pending: VecDeque<(Arc<Dir>, usize)>,
    current: std::iter::Peekable<std::vec::IntoIter<Entry>>,
}

impl DirStream {
    /// 名稱等於 `name` 的 entry；呼叫端必須以遞增的名稱查詢。
    pub async fn take(&mut self, reader: &Reader, name: &[u8]) -> Result<Option<Entry>> {
        loop {
            while self.current.peek().is_some_and(|e| e.n.as_slice() < name) {
                self.current.next();
            }
            match self.current.peek() {
                Some(e) if e.n.as_slice() == name => return Ok(self.current.next()),
                Some(_) => return Ok(None),
                None => {}
            }
            let Some((dir, i)) = self.pending.pop_front() else {
                return Ok(None);
            };
            match reader.segment(&dir, i, kind::SEG).await? {
                Item::Seg(s) => self.current = s.entries.into_iter().peekable(),
                _ => {
                    return Err(CoreError::corrupt(
                        format!("{}", dir.segs[i].id),
                        "expected a directory segment",
                    ))
                }
            }
        }
    }

    /// 剩下的全部 entry（測試與第 5 段的讀取用）。
    pub async fn rest(mut self, reader: &Reader) -> Result<Vec<Entry>> {
        let mut out: Vec<Entry> = self.current.by_ref().collect();
        while let Some((dir, i)) = self.pending.pop_front() {
            match reader.segment(&dir, i, kind::SEG).await? {
                Item::Seg(s) => out.extend(s.entries),
                _ => {
                    return Err(CoreError::corrupt(
                        format!("{}", dir.segs[i].id),
                        "expected a directory segment",
                    ))
                }
            }
        }
        Ok(out)
    }
}

impl Repo {
    /// 讀取端的狀態：列出來的最大世代（主體不行讀 `.r1`；不要求 `.r1` 在列表裡）、它的 map 與當下列表裡的
    /// delta。backup 的規則更嚴（B1，見 `backup`）；restore 與 mount 退回較舊世代的規則在第 5 段。
    pub async fn reader(&self, own: Option<ClientId>) -> Result<Reader> {
        let listing = self.list_heads(None).await?;
        let Some(&g) = listing.keys().next_back() else {
            return Err(CoreError::Generation(
                "no generation head under maint/gen/".to_owned(),
            ));
        };
        let gen = self.read_head(g).await?;
        let map = self.load_map(&gen).await?;
        let deltas = self.load_deltas(&map).await?;
        Ok(Reader::new(
            self.clone(),
            Arc::new(State { gen, map, deltas }),
            own,
        ))
    }
}
