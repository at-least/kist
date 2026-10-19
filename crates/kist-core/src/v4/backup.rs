//! v4 的 backup：§11.2 的 B1–B5。
//!
//! - **B1**（[`Repo::backup_prepare`] 開頭）：記下開始時間（牆鐘與含休眠的單調時鐘）；讀列出來的最大世代
//!   `g_s`（讀得了、版本認得，列表裡要有它的 `.r1`，沒有就稍候重新列出一次），載入它的分片與當下列表裡的
//!   delta；找 parent：自己的 snapshot 主體裡最新、roots 的路徑與 `t` 都相同的那一份。
//! - **B2**（走訪）：依名稱排序遞迴走訪。每個 chunk 與 metadata item 只在有種類相符的持有者（`Map(g_s)` 裡
//!   live 的記錄、delta 提到而 map 沒提到的 pack、這次自己寫的 pack）而且可信（`by_prune`、在自己的前綴下、
//!   或這台 client 取回驗證過）時沿用；不可信的候選先取回驗證（解密、重算 ID），成功就記進本地快取；都不行
//!   就重寫。目錄、chunk 清單、roots 依 §7 切段；目錄邊走邊切，記憶體裡只有一段。
//!   **沿用 parent 的子樹**：同一條路徑上新建出來的目錄（或 chunk 清單）的 meta ID 等於 parent 的，而且它有
//!   可信的持有者，就只記這一筆（子樹裡的其他 item 不查持有者，靠 parent 一直存在），B4 對 parent 做 HEAD。
//!   比 GC 模型的規則窄：模型比對 parent 裡任何位置的 item，這裡只比對同一條路徑。
//! - **B3**：pack 與 mpack 以只在不存在時建立寫入（mpack 在 replicas=1 時先主體、後 `.r1`；parity 選配），每
//!   寫幾個就寫一份 delta 列出它們（重跑時沿用已上傳的），最後再寫一份。`E ≥ grace − M` 之後不再寫 delta。
//! - **B4、B5**：[`PreparedBackup::commit`]。
//!
//! 走訪的骨架（排除規則、記號檔、不跨檔案系統、深度上限、硬連結、快速路徑）來自 v3（ADR 019–021、024）；
//! 來源與排除規則的型別（[`SourceSpec`]、[`Exclusions`]、[`Marker`]）在 v3 移除時從 v3 的 backup 搬來這裡。

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use kist_backend::source::{LocalSource, SortedItems, Source, SourceItem, SourceItemKind};
use kist_backend::{Backend, BackendError};
use kist_crypto::v4::Keys;
use kist_format::chunker::ChunkerParams;
use kist_format::v4::ids::{ChunkId, ClientId, ItemId, MetaId, ObjectId};
use kist_format::v4::index::{status, Delta, DeltaPack, Settings};
use kist_format::v4::keys::{Gen, Key, SnapTs, SNAPSHOTS};
use kist_format::v4::meta::{
    build_chunks, content_type, meta_kind, node_type, DirBuilder, Entry, Limits,
};
use kist_format::v4::pack::Kind;
use kist_format::v4::snapshot::{build_roots, split_locator, Root, Snapshot, SnapshotStats};
use tokio::task::JoinSet;

use super::cache::{Cache, Kind as CacheKind};
use super::pack::{FinishedPack, FinishedSpool, PackWriter, SpoolWriter};
use super::read::{DirStream, Reader};
use super::repo::Repo;
use super::spill::WrittenSet;
use super::state::{pack_key, Generation, Holder, Source as HolderSource, State};
use crate::filter::{FilterState, PathFilter};
use crate::{blocking, fsmeta, CoreError, Result};

/// §11.1 的 `M`。
pub const M: Duration = Duration::from_secs(3600);

/// 同時上傳中的 pack 數（每個最多 pack_target 的記憶體，同 v3）。
const MAX_INFLIGHT_UPLOADS: usize = 1;

/// 備份來源的指定（[`BackupOptions::source`]）。
#[derive(Clone, Default)]
pub enum SourceSpec {
    /// `paths` 是本機檔案系統路徑（可多個；預設）。
    #[default]
    LocalPaths,
    /// 單一遠端 URL（`sftp://`、`s3://`）：`backup`/`backup_prepare` 的
    /// `paths` 必須正好是這個 URL——整個 backup 只有這一個 root。
    Url(String),
    /// 直接注入來源（測試與程式內嵌用）：給定 `Source` 與 `Root.path` 的
    /// 定位 bytes，不經 `open_source`；`paths` 被忽略。
    Injected(Arc<dyn Source>, Vec<u8>),
}

impl SourceSpec {
    /// 要備份的路徑決定來源：第一個是 `sftp://` 或 `s3://` URL 就是那個遠端來源，而且它必須是唯一的路徑；
    /// 否則是本機路徑。`kist backup` 的參數與設定檔的 `[backup] paths` 共用這條規則。
    pub fn for_paths(paths: &[PathBuf]) -> Result<Self> {
        let Some(url) = paths
            .first()
            .and_then(|p| p.to_str())
            .filter(|u| kist_backend::source::is_remote_spec(u))
        else {
            return Ok(Self::LocalPaths);
        };
        if paths.len() > 1 {
            return Err(CoreError::Usage(
                "a remote source URL (sftp:// or s3://) is the only path a remote backup takes"
                    .to_owned(),
            ));
        }
        Ok(Self::Url(url.to_owned()))
    }
}

// `Arc<dyn Source>` 沒有 Debug（trait object 不帶），手動把定位印出來。
impl std::fmt::Debug for SourceSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::LocalPaths => f.write_str("LocalPaths"),
            Self::Url(url) => f.debug_tuple("Url").field(url).finish(),
            Self::Injected(_, locator) => f
                .debug_tuple("Injected")
                .field(&String::from_utf8_lossy(locator))
                .finish(),
        }
    }
}

/// backup 的排除規則（ADR 020、021）。[`Default`]＝什麼都不排除。被排除的項目都計進
/// [`BackupReport::excluded`]；指名的來源本身一律照備，規則只管它們底下的東西。
#[derive(Debug, Clone, Default)]
pub struct Exclusions {
    /// 路徑樣式：比對到的檔案與目錄不進 snapshot（目錄整棵不走）。
    pub patterns: PathFilter,
    /// 記號檔：目錄裡有其中任何一個時，除了記號檔本身，目錄的內容都不備份；目錄本身照備。
    pub if_present: Vec<Marker>,
    /// 比這大（bytes）的檔案不備份；剛好等於的照備。
    pub larger_than: Option<u64>,
    /// 不進入別的檔案系統：裝置號碼與它所屬來源不同的目錄（掛載點、btrfs 子卷）照備成
    /// 空目錄，這樣的檔案（單一檔案的 bind mount）不備份。只適用本機來源。
    pub one_file_system: bool,
}

impl Exclusions {
    /// CLI 旗標與設定檔共用的組法（審查修正：原本兩邊各組一次）：`if_present` 逐一以
    /// [`Marker::parse`] 解析，`exclude_caches` 再加上 [`Marker::cachedir_tag`]。不支援的平台上
    /// `one_file_system` 在這裡就報錯：CLI 在碰 repo 之前、設定檔在載入時，不是每次 backup 才失敗。
    pub fn new(
        patterns: PathFilter,
        if_present: &[String],
        exclude_caches: bool,
        larger_than: Option<u64>,
        one_file_system: bool,
    ) -> Result<Self> {
        if one_file_system && cfg!(not(unix)) {
            return Err(one_file_system_unsupported());
        }
        let mut markers = if_present
            .iter()
            .map(|spec| Marker::parse(spec))
            .collect::<Result<Vec<_>>>()?;
        if exclude_caches {
            markers.push(Marker::cachedir_tag());
        }
        Ok(Self {
            patterns,
            if_present: markers,
            larger_than,
            one_file_system,
        })
    }
}

fn one_file_system_unsupported() -> CoreError {
    CoreError::Usage(
        "one-file-system is not supported on this platform (no device numbers)".to_owned(),
    )
}

/// [`Exclusions::if_present`] 的記號檔：名稱，加上檔案開頭必須是的 bytes（空的＝任何
/// 種類、任何內容都算）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Marker {
    pub(crate) name: Vec<u8>,
    pub(crate) header: Vec<u8>,
}

impl Marker {
    /// `NAME` 或 `NAME:HEADER`（同 restic 的 `--exclude-if-present`）。NAME 是單一路徑元件；
    /// 有 HEADER 時，記號必須是開頭為 HEADER 的一般檔案。
    pub fn parse(spec: &str) -> Result<Self> {
        let (name, header) = spec.split_once(':').unwrap_or((spec, ""));
        // Windows 上 `\` 也是路徑分隔符（同 fsmeta::validate_child_name；審查修正）。
        let separator = |c: char| c == '/' || c == '\0' || (cfg!(windows) && c == '\\');
        if name.is_empty() || name == "." || name == ".." || name.contains(separator) {
            return Err(CoreError::Usage(format!(
                "exclude marker {spec:?}: NAME must be a single file name (NAME or NAME:HEADER)"
            )));
        }
        Ok(Self {
            name: name.as_bytes().to_vec(),
            header: header.as_bytes().to_vec(),
        })
    }

    /// CACHEDIR.TAG（<https://bford.info/cachedir/>）：開頭是規格固定的 43 bytes 簽章。
    pub fn cachedir_tag() -> Self {
        Self {
            name: b"CACHEDIR.TAG".to_vec(),
            header: b"Signature: 8a477f597d28d172789f06886806bc55".to_vec(),
        }
    }
}

/// 一次時鐘讀數：牆鐘與含休眠的單調時鐘（§11.1）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reading {
    pub wall: time::OffsetDateTime,
    pub boot: Duration,
}

/// backup 的時鐘；測試注入假的。
#[derive(Clone)]
pub struct Clock(Arc<dyn Fn() -> Reading + Send + Sync>);

impl Clock {
    pub fn system() -> Self {
        Self(Arc::new(|| Reading {
            wall: time::OffsetDateTime::now_utc(),
            boot: boot_time(),
        }))
    }

    pub fn new(f: impl Fn() -> Reading + Send + Sync + 'static) -> Self {
        Self(Arc::new(f))
    }

    pub fn now(&self) -> Reading {
        (self.0)()
    }
}

impl std::fmt::Debug for Clock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Clock")
    }
}

/// 含休眠的單調時鐘（§11.1）。Linux：`CLOCK_BOOTTIME`（clock_gettime(2)：「includes any time that the system is
/// suspended」）。macOS：`CLOCK_MONOTONIC`——Apple 的 clock_gettime(3)：「will continue to increment while the system
/// is asleep」；不能用 `Instant`：它在 macOS 是 `CLOCK_UPTIME_RAW`（std 的 time.rs），同一份 man page 說它「does not
/// increment while the system is asleep」。
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn boot_time() -> Duration {
    #[cfg(target_os = "linux")]
    let id = rustix::time::ClockId::Boottime;
    #[cfg(target_os = "macos")]
    let id = rustix::time::ClockId::Monotonic;
    let t = rustix::time::clock_gettime(id);
    Duration::new(
        u64::try_from(t.tv_sec).unwrap_or(0),
        u32::try_from(t.tv_nsec).unwrap_or(0),
    )
}

/// Windows：`Instant` 是 QueryPerformanceCounter（std 的 time.rs），Microsoft 的〈Acquiring high-resolution time
/// stamps〉：「returns the total number of ticks that have occurred since the Windows operating system was started,
/// including the time when the machine was in a sleep state such as standby, hibernate, or connected standby」。
/// 其他平台（不在發行的目標裡）沒有查證；E 另外取牆鐘的經過，取大。
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn boot_time() -> Duration {
    static BASE: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    BASE.get_or_init(std::time::Instant::now).elapsed()
}

/// §11.1 的 E：max(牆鐘經過, 含休眠的單調時鐘經過)。
pub fn elapsed(start: &Reading, now: &Reading) -> Duration {
    let wall = Duration::try_from(now.wall - start.wall).unwrap_or(Duration::ZERO);
    wall.max(now.boot.saturating_sub(start.boot))
}

#[derive(Debug, Clone)]
pub struct BackupOptions {
    pub client: ClientId,
    pub hostname: String,
    pub username: String,
    /// 每個 pack 與 mpack 旁的 Reed-Solomon 同位片數（0..=8；0 不寫）。寫失敗只警告：pack 本身已安全。
    pub parity: u8,
    pub source: SourceSpec,
    pub exclude: Exclusions,
    #[doc(hidden)]
    pub clock: Clock,
    /// B1：列表裡沒有最大世代的 `.r1` 時，等這麼久再列一次。
    #[doc(hidden)]
    pub lone_head_wait: Duration,
    /// B3：每寫這麼多個 pack（資料與 mpack 合計）寫一份中間的 delta。
    #[doc(hidden)]
    pub delta_every: usize,
    /// 進度回報：每處理完一個項目一次，結尾的 flush、B4 的檢查與 B5 的 commit 各一次。
    pub progress: Option<ProgressCallback>,
    /// SFTP 來源的快速路徑（選用，預設關；M12 第 7 段之三第八步，修改 ADR 016 的分級）：`Some(every)` 時，大小與
    /// mtime 都和 parent 相同、mtime 那一秒在 parent 開始之前就結束的檔案沿用 parent 的內容、不重讀——但這台 client
    /// 上一次整份讀這個來源超過 `every` 了（或沒有紀錄：沒有本機快取、第一次）就整份讀（見 [`MtimeTrust`]）。大小與
    /// mtime 都是寫檔案的人設得了的（`touch -d`、`cp -p`）：內容變了、兩個都沒變的檔案要到下一次整份讀才備份到。
    /// 不是 SFTP 來源是用法錯誤。
    pub sftp_trust_mtime: Option<Duration>,
}

/// [`BackupOptions::sftp_trust_mtime`] 的預設間隔：一週整份讀一次。
pub const DEFAULT_REREAD_EVERY: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// backup 進行中的即時狀態（給 UI 顯示進度）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct BackupProgress {
    /// "files" | "flush" | "verify" | "commit"
    pub phase: &'static str,
    pub stats: SnapshotStats,
    pub report: BackupReport,
    /// 剛處理完的路徑（lossy UTF-8）；結尾的階段是 `None`。
    pub current: Option<String>,
}

/// 進度 callback：`Arc` 包起來讓 [`BackupOptions`] 仍可 `Clone`；backup 的 future 在多執行緒的 runtime 上，所以
/// 要 `Send + Sync`。
#[derive(Clone)]
pub struct ProgressCallback(pub Arc<dyn Fn(&BackupProgress) + Send + Sync>);

impl std::fmt::Debug for ProgressCallback {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ProgressCallback")
    }
}

fn report_progress(
    progress: Option<&ProgressCallback>,
    phase: &'static str,
    stats: SnapshotStats,
    report: BackupReport,
    current: Option<String>,
) {
    if let Some(cb) = progress {
        (cb.0)(&BackupProgress {
            phase,
            stats,
            report,
            current,
        });
    }
}

impl BackupOptions {
    pub fn new(client: ClientId, hostname: impl Into<String>, username: impl Into<String>) -> Self {
        Self {
            client,
            hostname: hostname.into(),
            username: username.into(),
            parity: 0,
            source: SourceSpec::default(),
            exclude: Exclusions::default(),
            clock: Clock::system(),
            lone_head_wait: Duration::from_secs(5),
            delta_every: 16,
            progress: None,
            sftp_trust_mtime: None,
        }
    }
}

/// 過程計數（不在格式裡）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
pub struct BackupReport {
    /// 這次新寫的 chunk 與 metadata item。
    pub chunks_new: u64,
    pub items_new: u64,
    /// 沿用既有持有者的 chunk 與 metadata item（只算逐一判斷的；沿用的子樹裡面的不算）。
    pub chunks_reused: u64,
    pub items_reused: u64,
    /// 取回驗證之後才沿用的（別台寫的、第一次用）。
    pub verified: u64,
    /// 沿用 parent 的子樹的次數（由下往上，巢狀的也算）。
    pub subtrees: u64,
    pub packs_new: u64,
    pub mpacks_new: u64,
    pub deltas_new: u64,
    /// 新寫的 chunk 明文 bytes。
    pub bytes_stored: u64,
    /// 走快速路徑、沒有重讀內容的檔案。
    pub files_reused: u64,
    /// 讀不到或放不進格式而略過的項目（snapshot 照樣寫，CLI 以非 0 結束）。
    pub errors: u64,
    /// 被排除規則略過的項目。
    pub excluded: u64,
}

/// 回報用的一個 root：定位（lossy UTF-8）與種類（`file`、`dir`、`symlink`）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct RootSummary {
    pub path: String,
    #[serde(rename = "type")]
    pub kind: &'static str,
}

impl RootSummary {
    fn new(path: &[u8], t: u8) -> Self {
        Self {
            path: String::from_utf8_lossy(path).into_owned(),
            kind: match t {
                node_type::DIR => "dir",
                node_type::SYMLINK => "symlink",
                _ => "file",
            },
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct BackupSummary {
    /// snapshot 主體的 key。
    pub snapshot: String,
    pub parent: Option<String>,
    /// B4 讀到的世代 `g_c`。
    pub generation: Gen,
    pub stats: SnapshotStats,
    pub report: BackupReport,
    /// 這個 snapshot 的 roots。
    pub roots: Vec<RootSummary>,
    /// 開了 SFTP 的快速路徑（[`BackupOptions::sftp_trust_mtime`]）時，這次的決定。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mtime_trust: Option<MtimeTrust>,
}

/// SFTP 的快速路徑這次用不用（[`BackupOptions::sftp_trust_mtime`]）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct MtimeTrust {
    /// 這次沿用大小與 mtime 都沒變的檔案，不重讀。
    pub trusted: bool,
    /// 整份讀的原因；`trusted` 時是 `None`。
    pub reason: Option<&'static str>,
    /// 這台 client 上一次整份讀這個來源的時間（backup 的開始，Unix 奈秒）。
    pub last_full_read: Option<i64>,
    /// 多久至少整份讀一次（秒）。
    pub reread_every_secs: u64,
}

/// 這台 client 上一次整份讀一個 SFTP 來源的紀錄，在本機快取裡的檔名：client 與來源的定位一起取 BLAKE3（同一個
/// 快取目錄給兩個 client id 用時各自一份）。
fn full_read_name(client: &ClientId, locator: &[u8]) -> String {
    let mut h = blake3::Hasher::new();
    h.update(client.as_bytes());
    h.update(locator);
    format!("full-read/{}", &h.finalize().to_hex()[..32])
}

/// SFTP 的快速路徑這次能不能用：要有 parent、有本機快取、快取裡記著這台 client 上一次整份讀這個來源的時間，那次距
/// 這次開始不到 `every`，而且 parent 不比那次舊（不然 forget 掉新的 snapshot 之後，parent 可能是很久沒有整份讀過的
/// 那一串）。紀錄在這次開始之後（時鐘往回調過）當成沒有。
fn decide_mtime_trust(
    cache: Option<&Cache>,
    name: &str,
    parent_time: Option<i64>,
    now: i64,
    every: Duration,
) -> MtimeTrust {
    let last = cache.and_then(|c| c.last_full_read(name));
    let every_ns = i64::try_from(every.as_nanos()).unwrap_or(i64::MAX);
    let reason = match (parent_time, cache, last) {
        (None, _, _) => Some("no parent snapshot"),
        (_, None, _) => Some("no local cache"),
        (_, _, None) => Some("no full read recorded on this machine"),
        (_, _, Some(t)) if t > now => Some("the recorded full read is later than now"),
        (_, _, Some(t)) if now - t >= every_ns => Some("the last full read is too old"),
        (Some(p), _, Some(t)) if p < t => {
            Some("the parent snapshot is older than the last full read")
        }
        _ => None,
    };
    MtimeTrust {
        trusted: reason.is_none(),
        reason,
        last_full_read: last,
        reread_every_secs: every.as_secs(),
    }
}

/// SFTP 來源的大小與 mtime 快速路徑（[`BackupOptions::sftp_trust_mtime`]）：parent 的 Entry 是 sftp、大小與 mtime 都
/// 相同，而且 mtime 那一秒在 parent 開始之前就結束了。SFTP 的 mtime 是伺服器的秒：同一秒裡讀過之後又改的檔案看
/// 起來沒變；伺服器的時鐘比這台慢多少，這個判斷就弱多少。
fn sftp_unchanged(pe: &Entry, size: u64, mtime_ns: i64, parent_start: i64) -> bool {
    pe.mk == meta_kind::SFTP
        && pe.size == size
        && pe.mtime == Some(mtime_ns)
        && mtime_ns.saturating_add(1_000_000_000) <= parent_start
}

/// 一個檔案在 Entry 裡的內容。
#[derive(Debug, Clone)]
struct FileContent {
    size: u64,
    ct: u8,
    chunks: Vec<ItemId>,
}

/// 沿用的 (item, 持有者)：持有者是 [`UsedPacks`] 裡的位置，記憶體裡每筆 36 bytes。
type Used = Vec<(ItemId, u32)>;

/// 這次去重用到的 pack（B4 逐一檢查）。
#[derive(Debug, Clone)]
struct UsedPack {
    id: ObjectId,
    client: ClientId,
    kind: Kind,
    source: HolderSource,
}

#[derive(Debug, Default)]
struct UsedPacks {
    index: HashMap<ObjectId, u32>,
    packs: Vec<UsedPack>,
}

impl UsedPacks {
    fn intern(&mut self, h: &Holder) -> u32 {
        if let Some(i) = self.index.get(&h.pack) {
            return *i;
        }
        let i = self.packs.len() as u32;
        self.packs.push(UsedPack {
            id: h.pack,
            client: h.client,
            kind: h.kind,
            source: h.source,
        });
        self.index.insert(h.pack, i);
        i
    }
}

/// B2 的結果。
enum Decision {
    /// 這次已經寫過（在自己這次寫的 pack 裡）。
    InRun,
    Reuse(Holder),
    Write,
}

/// 一個 metadata item 放進 snapshot 的方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Reused(u32),
    InRun,
    Written,
}

/// B2：判斷沿用或重寫。只在 blocking 執行緒上用：取回驗證要以 `block_on` 等後端。
struct Dedup {
    reader: Reader,
    written: WrittenSet,
    handle: tokio::runtime::Handle,
    verified: u64,
}

/// [`Dedup::decide_cached`] 判斷不了。
enum Undecided {
    /// 第一個候選不可信，要取回驗證（只能在 blocking 執行緒上做）。
    NeedFetch,
    /// 查「這次寫過」失敗（暫存檔讀不了）。
    Failed(CoreError),
}

impl Dedup {
    /// 不取回就判斷得了的 B2（async 端也能用）：這次寫過、沒有候選、第一個候選可信——與 [`Dedup::decide`] 的結果
    /// 相同；第一個候選不可信時 `decide` 會先取回它驗證，這裡回 [`Undecided::NeedFetch`]。
    fn decide_cached(&self, id: ItemId, kind: Kind) -> std::result::Result<Decision, Undecided> {
        if self.written.contains(&id).map_err(Undecided::Failed)? {
            return Ok(Decision::InRun);
        }
        match self
            .reader
            .state
            .reuse_candidates(&id, kind)
            .into_iter()
            .next()
        {
            None => Ok(Decision::Write),
            Some(h) if self.reader.trusted(&id, &h) => Ok(Decision::Reuse(h)),
            Some(_) => Err(Undecided::NeedFetch),
        }
    }

    fn decide(&mut self, id: ItemId, kind: Kind) -> Result<Decision> {
        if self.written.contains(&id)? {
            return Ok(Decision::InRun);
        }
        for h in self.reader.state.reuse_candidates(&id, kind) {
            if self.reader.trusted(&id, &h) {
                return Ok(Decision::Reuse(h));
            }
            match self.handle.block_on(self.reader.fetch(&h)) {
                Ok(_) => {
                    self.reader.note_verified(id, &h);
                    self.verified += 1;
                    return Ok(Decision::Reuse(h));
                }
                Err(e) => tracing::warn!(
                    "{id}: the copy in pack {} failed verification; not reusing it: {e}",
                    h.pack
                ),
            }
        }
        Ok(Decision::Write)
    }
}

/// 一批 item 各自的結果，與因此寫滿的 mpack。
type Placed = (Vec<(MetaId, Outcome)>, Vec<FinishedPack>);

/// 走訪一個目錄的 future（遞迴，要 boxed；`Send` 讓整個 backup 能在別的 task 上跑）。
type DirFuture<'a> =
    std::pin::Pin<Box<dyn std::future::Future<Output = Result<(MetaId, Used)>> + Send + 'a>>;

/// 在 blocking 執行緒之間移進移出的可變狀態。
struct Work {
    dedup: Dedup,
    data: SpoolWriter,
    meta: PackWriter,
    packs: UsedPacks,
}

impl Work {
    /// 快速路徑的一個檔案照 B2 判斷內容（`decide` 是取回驗證的 [`Dedup::decide`] 或不取回的
    /// [`Dedup::decide_cached`]）：`ct=0` 每個 chunk 都要能沿用；`ct=1` 只判斷清單的根（沿用 parent 的子樹）。
    /// `Ok(None)` 是不能沿用（重讀）；`Ok(Some((used, subtree)))` 是沿用。
    fn reuse_file<E>(
        &mut self,
        c: &FileContent,
        mut decide: impl FnMut(&mut Dedup, ItemId, Kind) -> std::result::Result<Decision, E>,
    ) -> std::result::Result<Option<(Used, bool)>, E> {
        let mut used = Used::new();
        if c.ct == content_type::DIRECT {
            for id in &c.chunks {
                match decide(&mut self.dedup, *id, Kind::Data)? {
                    Decision::InRun => {}
                    Decision::Reuse(h) => used.push((*id, self.packs.intern(&h))),
                    Decision::Write => return Ok(None),
                }
            }
            return Ok(Some((used, false)));
        }
        let Some(id) = c.chunks.first() else {
            return Ok(None);
        };
        Ok(match decide(&mut self.dedup, *id, Kind::Meta)? {
            Decision::InRun => Some((used, false)),
            Decision::Reuse(h) => {
                used.push((*id, self.packs.intern(&h)));
                Some((used, true))
            }
            Decision::Write => None,
        })
    }

    /// 照 B2 放一批 metadata item：沿用的記進 `used`，要寫的加進 mpack。回傳每一項的結果與寫滿的 mpack。
    fn place(
        &mut self,
        items: Vec<(MetaId, Vec<u8>)>,
        used: &mut Used,
        report: &mut BackupReport,
    ) -> Result<Placed> {
        let mut outcomes = Vec::with_capacity(items.len());
        let mut full = Vec::new();
        for (id, cbor) in items {
            let item: ItemId = id.into();
            let outcome = match self.dedup.decide(item, Kind::Meta)? {
                Decision::InRun => Outcome::InRun,
                Decision::Reuse(h) => {
                    let i = self.packs.intern(&h);
                    used.push((item, i));
                    report.items_reused += 1;
                    Outcome::Reused(i)
                }
                Decision::Write => {
                    self.meta.add(item, &cbor)?;
                    self.dedup.written.insert(item)?;
                    report.items_new += 1;
                    if self.meta.is_full() {
                        full.extend(self.meta.finish()?);
                    }
                    Outcome::Written
                }
            };
            outcomes.push((id, outcome));
        }
        Ok((outcomes, full))
    }
}

/// 預讀的檔案最大多少（M12 第 7 段之三第九步）：大的檔案自己就有好幾個 READ／GET 在等，不缺預讀；先開始讀的
/// 每個檔案在記憶體裡最多約它的大小（遠端讀取的 4 格 channel），這個上限讓預讀的記憶體有界。
const READ_AHEAD_MAX: u64 = 1024 * 1024;

/// 遠端讀取的內容（還沒讀的 [`Source::read_sized`] 的結果）。
type SourceRead = std::result::Result<Box<dyn std::io::Read + Send>, BackendError>;

/// 目錄裡下一個要處理的項目，已經判斷過排除、取好 parent 的 entry（[`Walk::next_ahead`]）；預讀的檔案帶著已經
/// 開始讀的內容。
struct Ahead {
    item: SourceItem,
    posix: Option<kist_backend::fsmeta::PosixMeta>,
    rel: Vec<u8>,
    visit: Visit,
    parent_entry: Option<Entry>,
    read: Option<SourceRead>,
}

/// 走訪端看到的檔案事實：快速路徑與硬連結判別用。
struct FileFacts {
    size: u64,
    /// 列表給的 mtime（奈秒）；只有 SFTP 的快速路徑看它（posix 的在 `posix` 裡）。
    mtime_ns: i64,
    posix: Option<kist_backend::fsmeta::PosixMeta>,
    etag: Option<Vec<u8>>,
}

/// 一個 root 的走訪來源。
struct SourceCtx {
    source: Arc<dyn Source>,
    named: Named,
}

impl SourceCtx {
    fn display_path(&self, rel: &[u8]) -> String {
        match self.source.local_path(rel) {
            Some(p) => p.to_string_lossy().into_owned(),
            None => String::from_utf8_lossy(rel).into_owned(),
        }
    }
}

/// 一個 root 底下另外被指名的來源（同 v3）。
#[derive(Default)]
struct Named {
    paths: HashSet<Vec<u8>>,
    toward: HashSet<Vec<u8>>,
}

impl Named {
    fn new(paths: Vec<Vec<u8>>) -> Self {
        let mut toward = HashSet::new();
        for path in &paths {
            let mut at = path.as_slice();
            while let Some(i) = at.iter().rposition(|&b| b == b'/') {
                at = &at[..i];
                toward.insert(at.to_vec());
            }
        }
        Self {
            paths: paths.into_iter().collect(),
            toward,
        }
    }
}

/// 一個項目在排除規則裡的位置（同 v3）。
#[derive(Clone)]
struct Visit {
    depth: usize,
    filter: FilterState,
    dev_bound: Option<u64>,
    pass_through: bool,
    named: bool,
    toward_named: bool,
}

impl Visit {
    fn root(filter: FilterState, dev_bound: Option<u64>) -> Self {
        Self {
            depth: 1,
            filter,
            dev_bound,
            pass_through: false,
            named: true,
            toward_named: false,
        }
    }
}

fn rel_bytes(rel: &Path) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    for comp in rel.components() {
        if !out.is_empty() {
            out.push(b'/');
        }
        out.extend_from_slice(&fsmeta::name_to_bytes(comp.as_os_str())?);
    }
    Ok(out)
}

fn join_rel(dir: &[u8], name: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(dir.len() + name.len() + 1);
    v.extend_from_slice(dir);
    if !dir.is_empty() {
        v.push(b'/');
    }
    v.extend_from_slice(name);
    v
}

/// posix 欄位（§8）：mode、uid、gid、mtime 必填；ctime 有才記；dev、ino、nlink 只在檔案且 nlink > 1 時記
/// （硬連結的識別，同 v3）。
fn fill_posix(entry: &mut Entry, p: &kist_backend::fsmeta::PosixMeta, is_file: bool) {
    entry.mode = Some(p.mode);
    entry.uid = Some(p.uid);
    entry.gid = Some(p.gid);
    entry.mtime = Some(p.mtime_ns);
    entry.ctime = Some(p.ctime_ns).filter(|v| *v != 0);
    if is_file && p.nlink > 1 {
        entry.dev = Some(p.dev);
        entry.ino = Some(p.inode);
        entry.nlink = Some(p.nlink);
    }
}

/// parent 的檔案 entry → 快速路徑比較用的 FsMeta（缺席的選填欄位是 0，不拿來比）。
fn meta_of(e: &Entry) -> fsmeta::FsMeta {
    fsmeta::FsMeta {
        mode: e.mode.unwrap_or(0),
        uid: e.uid.unwrap_or(0),
        gid: e.gid.unwrap_or(0),
        mtime_ns: e.mtime.unwrap_or(0),
        ctime_ns: e.ctime.unwrap_or(0),
        inode: e.ino.unwrap_or(0),
        dev: e.dev.unwrap_or(0),
        nlink: e.nlink.unwrap_or(0),
    }
}

/// 本機單一檔案或 symlink 的來源項目：直接 lstat，不列整個父目錄（同 v3）。
fn local_file_item(path: &Path, name: &[u8]) -> Result<SourceItem> {
    let meta = std::fs::symlink_metadata(path).map_err(|e| CoreError::io(path, e))?;
    let posix = kist_backend::fsmeta::capture(&meta);
    let kind = if meta.file_type().is_symlink() {
        let target = std::fs::read_link(path).map_err(|e| CoreError::io(path, e))?;
        SourceItemKind::Symlink {
            target: fsmeta::path_to_bytes(&target)?,
        }
    } else if !meta.is_file() {
        return Err(CoreError::Usage(format!(
            "{}: not a regular file (fifo/socket/device); cannot back up",
            path.display()
        )));
    } else {
        SourceItemKind::File {
            size: meta.len(),
            mtime_ns: posix.mtime_ns,
            etag: None,
            vern: None,
        }
    };
    Ok(SourceItem {
        name: name.to_vec(),
        kind,
    })
}

/// 一個 root 的規劃：在 B1 之前決定好，parent 的比對要用到 `t`。
enum RootPlan {
    LocalDir {
        path: PathBuf,
        named: Vec<Vec<u8>>,
    },
    LocalFile {
        path: PathBuf,
        item: SourceItem,
    },
    /// `listing`：`plan_roots` 列過的根，walk 接著用（[`Peeked`]）。
    RemoteDir {
        source: Arc<dyn Source>,
        listing: Box<dyn SortedItems + Send>,
    },
    RemoteFile {
        source: Arc<dyn Source>,
        item: SourceItem,
    },
}

/// 已經取走第一個條目的列表：先交出那一個，之後照原本的列表。遠端 root 的列表由 `plan_roots` 開始、walk 接著用，
/// 根不必再列一次（M12 第 7 段之三第十二步）。
struct Peeked {
    first: Option<Option<std::result::Result<SourceItem, BackendError>>>,
    rest: Box<dyn SortedItems + Send>,
}

impl SortedItems for Peeked {
    fn next_item(&mut self) -> Option<std::result::Result<SourceItem, BackendError>> {
        match self.first.take() {
            Some(first) => first,
            None => self.rest.next_item(),
        }
    }

    /// 交出取走的那一個之後，原本的列表最後交出的也是它（取走之後沒有再往下列），posix 照它的。
    fn posix(&self) -> Option<kist_backend::fsmeta::PosixMeta> {
        self.rest.posix()
    }
}

struct Planned {
    path: Vec<u8>,
    t: u8,
    plan: RootPlan,
}

fn item_type(item: &SourceItem) -> u8 {
    match item.kind {
        SourceItemKind::Dir => node_type::DIR,
        SourceItemKind::File { .. } => node_type::FILE,
        SourceItemKind::Symlink { .. } => node_type::SYMLINK,
    }
}

fn last_segment(path: &[u8]) -> Result<Vec<u8>> {
    split_locator(path)?.pop().ok_or_else(|| {
        CoreError::Usage(format!(
            "cannot derive a name for source {}",
            String::from_utf8_lossy(path)
        ))
    })
}

/// 來源規劃（同 v3）：遠端 URL 或注入的來源是單一 root；本機路徑 canonicalize、依 bytes 排序去重、`/a` 蓋過
/// `/a/b`（b 記成指名的）。
async fn plan_roots(paths: &[PathBuf], opts: &BackupOptions) -> Result<Vec<Planned>> {
    let remote = |path: Vec<u8>, source: Arc<dyn Source>| async move {
        // 遠端 root 的種類：列根，恰好一個檔案而且名稱等於定位的最後一段 → 檔案（同 v3）
        let probe = Arc::clone(&source);
        let mut items = blocking(move || probe.list(b"").map_err(CoreError::from)).await?;
        let last = split_locator(&path)?.pop();
        let first = items.next_item();
        let plan = match (first, last) {
            (Some(Ok(item)), Some(last))
                if matches!(item.kind, SourceItemKind::File { .. }) && item.name == last =>
            {
                Planned {
                    path,
                    t: node_type::FILE,
                    plan: RootPlan::RemoteFile { source, item },
                }
            }
            (first, _) => Planned {
                path,
                t: node_type::DIR,
                plan: RootPlan::RemoteDir {
                    source,
                    listing: Box::new(Peeked {
                        first: Some(first),
                        rest: items,
                    }),
                },
            },
        };
        Ok::<_, CoreError>(plan)
    };
    match &opts.source {
        SourceSpec::LocalPaths => {
            let mut with_bytes = Vec::new();
            for p in paths {
                let abs = std::fs::canonicalize(p).map_err(|e| CoreError::io(p, e))?;
                with_bytes.push((fsmeta::path_to_bytes(&abs)?, abs));
            }
            with_bytes.sort();
            with_bytes.dedup();
            let mut kept: Vec<(Vec<u8>, PathBuf, Vec<Vec<u8>>)> = Vec::new();
            let mut kept_index: HashMap<PathBuf, usize> = HashMap::new();
            for (bytes, path) in with_bytes {
                let outer = path
                    .ancestors()
                    .skip(1)
                    .find_map(|a| kept_index.get(a).map(|&i| (a, i)));
                if let Some((outer, i)) = outer {
                    let rel = path
                        .strip_prefix(outer)
                        .map_err(|e| CoreError::Usage(format!("{}: {e}", path.display())))?;
                    tracing::info!(
                        "{}: inside source {}; backed up as part of it",
                        path.display(),
                        outer.display()
                    );
                    kept[i].2.push(rel_bytes(rel)?);
                    continue;
                }
                kept_index.insert(path.clone(), kept.len());
                kept.push((bytes, path, Vec::new()));
            }
            let mut out = Vec::new();
            for (bytes, path, named) in kept {
                let meta = std::fs::symlink_metadata(&path).map_err(|e| CoreError::io(&path, e))?;
                if meta.is_dir() {
                    out.push(Planned {
                        path: bytes,
                        t: node_type::DIR,
                        plan: RootPlan::LocalDir { path, named },
                    });
                } else {
                    let item = local_file_item(&path, &last_segment(&bytes)?)?;
                    out.push(Planned {
                        path: bytes,
                        t: item_type(&item),
                        plan: RootPlan::LocalFile { path, item },
                    });
                }
            }
            Ok(out)
        }
        SourceSpec::Url(url) => {
            if paths.len() != 1 || paths[0] != Path::new(url.as_str()) {
                return Err(CoreError::Usage(format!(
                    "a remote source URL backs up exactly one root: pass only {url:?} as the path"
                )));
            }
            let source: Arc<dyn Source> = kist_backend::source::open_source(url).await?.into();
            Ok(vec![remote(url.as_bytes().to_vec(), source).await?])
        }
        SourceSpec::Injected(source, locator) => {
            Ok(vec![remote(locator.clone(), Arc::clone(source)).await?])
        }
    }
}

/// parent：自己的一個 snapshot 主體。
struct Parent {
    key: String,
    snapshot: Snapshot,
    /// path → 那個 root。
    roots: HashMap<Vec<u8>, Root>,
}

impl Repo {
    /// B1：列出來的最大世代。列表從看過的世代開始；它不在列表裡就警告、改列全部。最大世代的 `.r1` 不在列表
    /// 裡（寫入者在兩次 PUT 之間或已經當掉）→ 等 `wait` 再列一次，仍然沒有就停止（§11.2）。
    async fn newest_for_backup(&self, wait: Duration) -> Result<Generation> {
        let cached = self.cache().and_then(|c| c.seen_gen());
        let mut listing = self.list_heads(cached).await?;
        if let Some(g) = cached {
            if listing.is_empty() {
                tracing::warn!(
                    "generation {g}, the newest this client saw before, is no longer listed; listing all generations"
                );
                listing = self.list_heads(None).await?;
            }
        }
        let Some((&g, copies)) = listing.iter().next_back() else {
            return Err(CoreError::Generation(
                "no generation head under maint/gen/: not a v4 repository, or its init did not finish"
                    .to_owned(),
            ));
        };
        let mut g = g;
        if copies.replica.is_none() {
            tokio::time::sleep(wait).await;
            let again = self.list_heads(Some(g)).await?;
            match again.iter().next_back() {
                Some((&newest, c)) if c.replica.is_some() => g = newest,
                _ => {
                    return Err(CoreError::Generation(format!(
                        "generation head {g} has no .r1 yet (its writer is between its two writes or crashed); \
                         run maintenance to repair it, then rerun the backup"
                    )))
                }
            }
        }
        let gen = self.read_head(g).await?;
        if let Some(c) = self.cache() {
            c.set_seen_gen(g);
        }
        Ok(gen)
    }

    /// B1：parent 是自己的 snapshot 主體裡最新、roots 的路徑與 `t` 都相同的那一份。讀不了的跳過並警告。
    async fn find_parent(
        &self,
        reader: &Reader,
        client: ClientId,
        roots: &[(Vec<u8>, u8)],
    ) -> Result<Option<Parent>> {
        let prefix = format!("{SNAPSHOTS}{client}");
        let mut keys: Vec<(SnapTs, Key)> = Vec::new();
        let mut listed = HashSet::new();
        for o in self.backend().list(&prefix).await? {
            match Key::parse(&o.key) {
                Ok(
                    key @ Key::Snapshot {
                        client: c,
                        replica: false,
                        ..
                    },
                ) if c == client => {
                    let Key::Snapshot { ts, .. } = &key else {
                        continue;
                    };
                    listed.insert(o.key.clone());
                    keys.push((ts.clone(), key));
                }
                Ok(Key::Snapshot { replica: true, .. }) => {}
                _ => tracing::warn!("{}: not a v4 snapshot key; ignored", o.key),
            }
        }
        if let Some(last) = self.cache().and_then(|c| c.last_snapshot()) {
            if !listed.contains(&last) {
                tracing::warn!(
                    "{last}, this client's last snapshot, is no longer in the repository"
                );
            }
        }
        keys.sort_by(|a, b| b.0.cmp(&a.0));
        for (_, key) in keys {
            let path = key.path();
            let snap = match self.read_snapshot(&key).await {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!("cannot read {path}: {e}; not using it as the parent");
                    continue;
                }
            };
            let theirs = match reader.roots(&snap.roots).await {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(
                        "cannot read the roots of {path}: {e}; not using it as the parent"
                    );
                    continue;
                }
            };
            let same = theirs.len() == roots.len()
                && theirs
                    .iter()
                    .zip(roots)
                    .all(|(r, (p, t))| r.path == *p && r.t == *t);
            if same {
                return Ok(Some(Parent {
                    key: path,
                    snapshot: snap,
                    roots: theirs.into_iter().map(|r| (r.path.clone(), r)).collect(),
                }));
            }
        }
        Ok(None)
    }

    /// B1 到 B3：除了 snapshot 之外全部寫完。`paths` 是本機來源；遠端來源由 `opts.source` 給。
    pub async fn backup_prepare(
        &self,
        paths: &[PathBuf],
        opts: BackupOptions,
    ) -> Result<PreparedBackup> {
        if usize::from(opts.parity) > kist_format::v4::parity::MAX_PARITY_SHARDS {
            return Err(CoreError::Usage(format!(
                "parity must be 0..={}, got {}",
                kist_format::v4::parity::MAX_PARITY_SHARDS,
                opts.parity
            )));
        }
        if opts.exclude.one_file_system && !matches!(opts.source, SourceSpec::LocalPaths) {
            return Err(CoreError::Usage(
                "one-file-system compares local device numbers; it works on local source paths only"
                    .to_owned(),
            ));
        }
        if opts.exclude.one_file_system && cfg!(not(unix)) {
            return Err(CoreError::Usage(
                "one-file-system is not supported on this platform (no device numbers)".to_owned(),
            ));
        }
        if opts.client == ClientId::MAINT {
            return Err(CoreError::Usage(
                "the all-zero client id belongs to the maintenance role".to_owned(),
            ));
        }

        // B1
        let started = opts.clock.now();
        let planned = plan_roots(paths, &opts).await?;
        let gen = self.newest_for_backup(opts.lone_head_wait).await?;
        let map = self.load_map(&gen).await?;
        let deltas = self.load_deltas(&map).await?;
        if let Some(c) = self.cache() {
            let live = |p: &ObjectId| map.record(p).is_some() || deltas.contains(p);
            let mpacks: HashSet<ObjectId> = map
                .records()
                .iter()
                .filter(|r| r.kind == Kind::Meta)
                .map(|r| r.id)
                .chain(
                    deltas
                        .packs()
                        .filter(|(_, p)| p.kind == Kind::Meta)
                        .map(|(_, p)| p.id),
                )
                .collect();
            c.retain(CacheKind::Mpack, &mpacks);
            c.compact_verified(live);
        }
        let settings = gen.head.settings;
        let state = Arc::new(State { gen, map, deltas });
        let known = self.cache().map(|c| c.verified()).unwrap_or_default();
        let reader =
            Reader::new(self.clone(), Arc::clone(&state), Some(opts.client)).with_verified(known);
        let root_types: Vec<(Vec<u8>, u8)> =
            planned.iter().map(|p| (p.path.clone(), p.t)).collect();
        let sftp_root = planned.iter().find_map(|p| match &p.plan {
            RootPlan::RemoteDir { source, .. } | RootPlan::RemoteFile { source, .. }
                if source.meta_kind() == meta_kind::SFTP =>
            {
                Some(p.path.clone())
            }
            _ => None,
        });
        let parent = self.find_parent(&reader, opts.client, &root_types).await?;
        // SFTP 來源：整份讀完、commit 之後記下這次的開始（下一次的 [`decide_mtime_trust`] 看它）。
        let full_read = sftp_root.map(|locator| full_read_name(&opts.client, &locator));
        let started_ns = i64::try_from(started.wall.unix_timestamp_nanos())
            .map_err(|_| CoreError::Usage("the clock is out of range".to_owned()))?;
        let mtime_trust = match (opts.sftp_trust_mtime, &full_read) {
            (Some(every), Some(name)) => Some(decide_mtime_trust(
                self.cache(),
                name,
                parent.as_ref().map(|p| p.snapshot.time),
                started_ns,
                every,
            )),
            (Some(_), None) => {
                return Err(CoreError::Usage(
                    "trusting sizes and modification times is for sftp:// sources only".to_owned(),
                ))
            }
            (None, _) => None,
        };
        let full_read = full_read.filter(|_| !mtime_trust.as_ref().is_some_and(|t| t.trusted));

        let chunker = self.config().chunker;
        let keys = Arc::clone(self.keys());
        // 這次寫過的 chunk 與累積中的資料 pack 的暫存檔放在這裡
        let spool_dir = self
            .cache()
            .map(|c| c.dir().to_path_buf())
            .unwrap_or_else(std::env::temp_dir);
        let mut w = Walk {
            repo: self.clone(),
            keys: Arc::clone(&keys),
            own: opts.client,
            cdc: kist_chunker::Chunker::new(ChunkerParams {
                min: chunker.min,
                avg: chunker.avg,
                max: chunker.max,
            }),
            work: Some(Work {
                dedup: Dedup {
                    reader: reader.clone(),
                    written: WrittenSet::new(spool_dir.clone()),
                    handle: tokio::runtime::Handle::current(),
                    verified: 0,
                },
                data: SpoolWriter::new(
                    Arc::clone(&keys),
                    Kind::Data,
                    settings.pack_target,
                    chunker.max,
                    spool_dir,
                ),
                meta: PackWriter::new(
                    Arc::clone(&keys),
                    Kind::Meta,
                    settings.mpack_target,
                    chunker.max,
                ),
                packs: UsedPacks::default(),
            }),
            reader: reader.clone(),
            uploads: JoinSet::new(),
            since_delta: Vec::new(),
            own_packs: Vec::new(),
            used_subtree: false,
            stats: SnapshotStats::default(),
            report: BackupReport::default(),
            parent_start: parent.as_ref().map_or(0, |p| p.snapshot.time),
            trust_sftp_mtime: mtime_trust.as_ref().is_some_and(|t| t.trusted),
            hardlinks: HashMap::new(),
            counted: HashSet::new(),
            chunk_bufs: Vec::new(),
            exclude: opts.exclude.clone(),
            settings,
            parity: opts.parity,
            started,
            clock: opts.clock.clone(),
            delta_every: opts.delta_every.max(1),
            progress: opts.progress.clone(),
        };

        // B2、B3：走訪每個 root
        let mut roots = Vec::with_capacity(planned.len());
        let mut used = Used::new();
        for p in planned {
            let parent_root = parent
                .as_ref()
                .and_then(|par| par.roots.get(&p.path))
                .filter(|r| r.t == p.t);
            let (root, root_used) = w.walk_root(p, parent_root).await?;
            roots.push(root);
            used.extend(root_used);
        }
        let mut items = Vec::new();
        let built = build_roots(roots, Limits::V4, &|n| keys.is_cut(n), &|b| keys.meta_id(b))?;
        items.extend(built.items);
        let mut roots_used = Used::new();
        let outcomes = w.place(items, &mut roots_used).await?;
        used.extend(roots_used);
        if parent
            .as_ref()
            .is_some_and(|p| p.snapshot.roots == built.root)
        {
            w.collapse(built.root, &outcomes, &mut used);
        }

        // B3：剩下的 pack、上傳、最後一份 delta
        w.flush().await?;
        let mut work = w
            .work
            .take()
            .ok_or_else(|| CoreError::Join("work missing".into()))?;
        w.report.verified = work.dedup.verified;
        let packs = std::mem::take(&mut work.packs);
        Ok(PreparedBackup {
            repo: self.clone(),
            reader,
            state,
            client: opts.client,
            hostname: opts.hostname,
            username: opts.username,
            clock: opts.clock,
            started,
            roots: built.root,
            parent: parent.map(|p| p.key),
            stats: w.stats,
            report: w.report,
            used,
            packs,
            own_packs: w.own_packs,
            used_subtree: w.used_subtree,
            settings,
            progress: opts.progress,
            root_summaries: root_types
                .iter()
                .map(|(p, t)| RootSummary::new(p, *t))
                .collect(),
            full_read,
            mtime_trust,
        })
    }

    /// 完整的 backup：[`Self::backup_prepare`] 加 [`PreparedBackup::commit`]。
    pub async fn backup(&self, paths: &[PathBuf], opts: BackupOptions) -> Result<BackupSummary> {
        self.backup_prepare(paths, opts).await?.commit().await
    }
}

/// 走訪中的 backup。
struct Walk {
    repo: Repo,
    reader: Reader,
    keys: Arc<Keys>,
    own: ClientId,
    cdc: kist_chunker::Chunker,
    work: Option<Work>,
    uploads: JoinSet<Result<()>>,
    /// 上一份 delta 之後寫的 pack。
    since_delta: Vec<DeltaPack>,
    /// 這次寫的 pack 與 mpack（B4：不能是 tombstone）。
    own_packs: Vec<ObjectId>,
    /// 沿用了 parent 的子樹（B4 對 parent 做 HEAD）。
    used_subtree: bool,
    stats: SnapshotStats,
    report: BackupReport,
    /// parent 的開始時間（快速路徑的 racy 判斷）。
    parent_start: i64,
    /// 這次用 SFTP 的大小與 mtime 快速路徑（[`decide_mtime_trust`]）。
    trust_sftp_mtime: bool,
    hardlinks: HashMap<(u64, u64), FileContent>,
    counted: HashSet<(u64, u64)>,
    chunk_bufs: Vec<Vec<u8>>,
    exclude: Exclusions,
    settings: Settings,
    parity: u8,
    started: Reading,
    clock: Clock,
    delta_every: usize,
    progress: Option<ProgressCallback>,
}

/// 一個檔案切塊到一半的狀態，在 blocking closure 之間移進移出：一輪最多封一個 pack 就回 async 端上傳。
struct ChunkState<R: std::io::Read> {
    chunks: kist_chunker::Chunks<R>,
    list: Vec<(ChunkId, u64)>,
    used: Used,
    bytes_total: u64,
    bytes_new: u64,
    chunks_new: u64,
    chunks_reused: u64,
}

impl Walk {
    fn take_work(&mut self) -> Result<Work> {
        self.work
            .take()
            .ok_or_else(|| CoreError::Join("backup state missing".into()))
    }

    fn skip(&mut self, what: &str, reason: &str) {
        tracing::warn!("{what}: {reason}; skipped");
        self.report.errors += 1;
    }

    /// 照 B2 放一批 metadata item（在 blocking 執行緒上），寫滿的 mpack 交出去上傳。
    async fn place(
        &mut self,
        items: Vec<(MetaId, Vec<u8>)>,
        used: &mut Used,
    ) -> Result<Vec<(MetaId, Outcome)>> {
        if items.is_empty() {
            return Ok(Vec::new());
        }
        let mut work = self.take_work()?;
        let mut report = self.report;
        let mut local = Used::new();
        let (work, result, report) = blocking(move || {
            let r = work
                .place(items, &mut local, &mut report)
                .map(|x| (x, local));
            Ok((work, r, report))
        })
        .await?;
        self.work = Some(work);
        self.report = report;
        let ((outcomes, full), local) = result?;
        used.extend(local);
        for p in full {
            self.handle_finished(p).await?;
        }
        Ok(outcomes)
    }

    /// 沿用 parent 的子樹：新建出來的根等於 parent 同一條路徑上的根，而且根有可信的持有者（或是這次已經寫過）
    /// → 子樹只記根這一筆。
    fn collapse(&mut self, root: MetaId, outcomes: &[(MetaId, Outcome)], used: &mut Used) {
        let Some((_, outcome)) = outcomes.iter().rev().find(|(id, _)| *id == root) else {
            return;
        };
        match outcome {
            Outcome::Reused(i) => *used = vec![(root.into(), *i)],
            Outcome::InRun => used.clear(),
            Outcome::Written => return,
        }
        self.used_subtree = true;
        self.report.subtrees += 1;
    }

    async fn walk_root(&mut self, p: Planned, parent: Option<&Root>) -> Result<(Root, Used)> {
        let parent_tree = parent.and_then(|r| r.tree);
        let parent_entry = parent.and_then(|r| r.entry.clone());
        match p.plan {
            RootPlan::LocalDir { path, named } => {
                let root_dev = if self.exclude.one_file_system {
                    let meta =
                        std::fs::symlink_metadata(&path).map_err(|e| CoreError::io(&path, e))?;
                    Some(kist_backend::fsmeta::capture(&meta).dev)
                } else {
                    None
                };
                let ctx = SourceCtx {
                    source: Arc::new(LocalSource::new(path)?),
                    named: Named::new(named),
                };
                let visit = Visit::root(self.root_state(&p.path)?, root_dev);
                let (tree, used) = self.walk_dir(&ctx, b"", None, parent_tree, visit).await?;
                Ok((dir_root(p.path, tree), used))
            }
            RootPlan::LocalFile { path, item } => {
                let dir = path.parent().ok_or_else(|| {
                    CoreError::Usage(format!(
                        "cannot derive a parent directory for source path {}",
                        path.display()
                    ))
                })?;
                let ctx = SourceCtx {
                    source: Arc::new(LocalSource::new(dir.to_path_buf())?),
                    named: Named::default(),
                };
                let name = item.name.clone();
                let entry = self
                    .process_entry(
                        &ctx,
                        &name,
                        item,
                        None,
                        parent_entry.as_ref(),
                        Visit::root(FilterState::default(), None),
                        None,
                    )
                    .await?;
                file_root(p.path, entry, &path.display().to_string())
            }
            RootPlan::RemoteDir { source, listing } => {
                let ctx = SourceCtx {
                    source,
                    named: Named::default(),
                };
                let visit = Visit::root(self.root_state(&p.path)?, None);
                let (tree, used) = self
                    .walk_dir(&ctx, b"", Some(listing), parent_tree, visit)
                    .await?;
                Ok((dir_root(p.path, tree), used))
            }
            RootPlan::RemoteFile { source, item } => {
                let ctx = SourceCtx {
                    source,
                    named: Named::default(),
                };
                let display = String::from_utf8_lossy(&p.path).into_owned();
                let entry = self
                    .process_entry(
                        &ctx,
                        b"",
                        item,
                        None,
                        parent_entry.as_ref(),
                        Visit::root(FilterState::default(), None),
                        None,
                    )
                    .await?;
                file_root(p.path, entry, &display)
            }
        }
    }

    /// 目錄來源在排除樣式裡的狀態：從頂端沿它的定位切段走下去（§9 的切段）。
    fn root_state(&self, locator: &[u8]) -> Result<FilterState> {
        let patterns = &self.exclude.patterns;
        let mut state = patterns.start();
        for name in split_locator(locator)? {
            state = patterns.descend(&state, &name);
        }
        Ok(state)
    }

    async fn markers_in(&self, ctx: &SourceCtx, dir_rel: &[u8]) -> Option<Vec<Vec<u8>>> {
        let mut found = Vec::new();
        for marker in &self.exclude.if_present {
            let rel = join_rel(dir_rel, &marker.name);
            let source = Arc::clone(&ctx.source);
            let limit = marker.header.len();
            let probe = blocking(move || source.probe(&rel, limit).map_err(CoreError::from)).await;
            match probe {
                Ok(Some(head)) if head == marker.header => found.push(marker.name.clone()),
                Ok(_) => {}
                Err(e) => tracing::warn!(
                    "{}: cannot check for {}: {e}; backing the directory up in full",
                    ctx.display_path(dir_rel),
                    String::from_utf8_lossy(&marker.name)
                ),
            }
        }
        (!found.is_empty()).then_some(found)
    }

    /// 一個空目錄（讀不到的目錄、別的檔案系統上的目錄）。
    async fn empty_dir(&mut self, used: &mut Used) -> Result<MetaId> {
        let keys = Arc::clone(&self.keys);
        let mut items = Vec::new();
        let root = DirBuilder::new(Limits::V4)?.finish(
            &|n| keys.is_cut(n),
            &|b| keys.meta_id(b),
            &mut items,
        )?;
        self.place(items, used).await?;
        Ok(root)
    }

    /// 遞迴走訪一個目錄的內容：回傳它的根（Seg 或 Dir）與這棵子樹沿用的 (item, 持有者)。
    /// `listed`：這個目錄已經開始的列表（遠端 root，[`Peeked`]）；沒有就在這裡列。
    fn walk_dir<'a>(
        &'a mut self,
        ctx: &'a SourceCtx,
        dir_rel: &'a [u8],
        listed: Option<Box<dyn SortedItems + Send>>,
        parent_tree: Option<MetaId>,
        visit: Visit,
    ) -> DirFuture<'a> {
        Box::pin(async move {
            let mut used = Used::new();
            let mut parent: Option<DirStream> = match parent_tree {
                Some(id) => match self.reader.dir(&id).await {
                    Ok(s) => Some(s),
                    Err(e) => {
                        tracing::warn!("cannot read the parent's directory {id}: {e}; re-reading this directory");
                        None
                    }
                },
                None => None,
            };

            let listing = match listed {
                Some(l) => Ok(l),
                None => {
                    let list_source = Arc::clone(&ctx.source);
                    let list_dir = dir_rel.to_vec();
                    blocking(move || list_source.list(&list_dir).map_err(CoreError::from)).await
                }
            };
            let mut listing = match listing {
                Ok(l) => l,
                Err(e) => {
                    self.skip(&ctx.display_path(dir_rel), &e.to_string());
                    let root = self.empty_dir(&mut used).await?;
                    return Ok((root, used));
                }
            };
            let markers = if self.exclude.if_present.is_empty() || visit.pass_through {
                None
            } else {
                self.markers_in(ctx, dir_rel).await
            };

            let keys = Arc::clone(&self.keys);
            let is_cut = |n: &[u8]| keys.is_cut(n);
            let meta_id = |b: &[u8]| keys.meta_id(b);
            let mut builder = DirBuilder::new(Limits::V4)?;
            let mut outcomes = Vec::new();
            // 遠端來源先判斷、先開始讀後面的檔案（[`Source::read_ahead`]，本機來源是 0：一個一個來，同原本）。
            // 佇列裡最多是正在處理的這個加上 read_ahead 個；遇到目錄就不再往後看——它的子樹走完之前，後面的檔案不
            // 先讀。parent 的 entry 照名稱的順序取，同原本。
            let read_ahead = ctx.source.read_ahead();
            let mut queue: VecDeque<Ahead> = VecDeque::new();
            let mut listed_all = false;
            loop {
                while !listed_all
                    && queue.len() <= read_ahead
                    && !queue
                        .back()
                        .is_some_and(|a| matches!(a.item.kind, SourceItemKind::Dir))
                {
                    match self
                        .next_ahead(
                            ctx,
                            dir_rel,
                            &mut *listing,
                            &mut parent,
                            markers.as_deref(),
                            &visit,
                        )
                        .await?
                    {
                        Some(a) => queue.push_back(a),
                        None => listed_all = true,
                    }
                }
                for a in queue.iter_mut().skip(1) {
                    if let (None, Some(size)) = (&a.read, self.reads_ahead(a)) {
                        a.read = Some(ctx.source.read_sized(&a.rel, size));
                    }
                }
                let Some(a) = queue.pop_front() else {
                    break;
                };
                let display = ctx.display_path(&a.rel);
                let Some((entry, child_used)) = self
                    .process_entry(
                        ctx,
                        &a.rel,
                        a.item,
                        a.posix,
                        a.parent_entry.as_ref(),
                        a.visit,
                        a.read,
                    )
                    .await?
                else {
                    continue;
                };
                let mut items = Vec::new();
                match builder.push(entry, &is_cut, &meta_id, &mut items) {
                    Ok(()) => used.extend(child_used),
                    // §7：放不進格式的項目記成這個項目的錯誤，目錄照樣寫
                    Err(e) => {
                        self.skip(&display, &e.to_string());
                        continue;
                    }
                }
                outcomes.extend(self.place(items, &mut used).await?);
            }
            let mut items = Vec::new();
            let root = builder.finish(&is_cut, &meta_id, &mut items)?;
            outcomes.extend(self.place(items, &mut used).await?);
            if parent_tree == Some(root) {
                self.collapse(root, &outcomes, &mut used);
            }
            Ok((root, used))
        })
    }

    /// 目錄列表裡下一個要處理的項目：跳過排除的、超過深度的（同原本的迴圈），判斷好它的 [`Visit`]，取好 parent
    /// 的 entry（呼叫端照名稱的順序呼叫）。列表完了是 `None`。
    async fn next_ahead(
        &mut self,
        ctx: &SourceCtx,
        dir_rel: &[u8],
        listing: &mut (dyn SortedItems + Send),
        parent: &mut Option<DirStream>,
        markers: Option<&[Vec<u8>]>,
        visit: &Visit,
    ) -> Result<Option<Ahead>> {
        while let Some(item) = listing.next_item() {
            let item = match item {
                Ok(i) => i,
                Err(e) => {
                    self.skip(&ctx.display_path(dir_rel), &e.to_string());
                    continue;
                }
            };
            // 本機來源：列舉時那一次 lstat 的 metadata（不必再 stat 一次）
            let posix = listing.posix();
            let child_rel = join_rel(dir_rel, &item.name);
            let named = ctx.named.paths.contains(&child_rel);
            let toward_named = !named && ctx.named.toward.contains(&child_rel);
            if visit.pass_through && !named && !toward_named {
                self.report.excluded += 1;
                continue;
            }
            let child_filter = self.exclude.patterns.descend(&visit.filter, &item.name);
            let too_large = match (self.exclude.larger_than, &item.kind) {
                (Some(limit), SourceItemKind::File { size, .. }) => *size > limit,
                _ => false,
            };
            let beside_marker = markers.is_some_and(|names| !names.contains(&item.name));
            let excluded = !named && (child_filter.is_match() || too_large || beside_marker);
            if excluded && !toward_named {
                tracing::debug!("{}: excluded", ctx.display_path(&child_rel));
                self.report.excluded += 1;
                continue;
            }
            // 深度上限與讀取端同一把尺（同 v3）：寫得出、還原不回就是自造的損壞。
            if matches!(item.kind, SourceItemKind::Dir) && visit.depth >= crate::MAX_TREE_DEPTH {
                self.skip(
                    &ctx.display_path(&child_rel),
                    &format!(
                        "directory nesting deeper than {} levels; the read side refuses deeper trees",
                        crate::MAX_TREE_DEPTH
                    ),
                );
                continue;
            }
            let parent_entry = match parent.as_mut() {
                Some(s) => match s.take(&self.reader, &item.name).await {
                    Ok(e) => e,
                    Err(e) => {
                        tracing::warn!(
                            "cannot read the parent's copy of {}: {e}; re-reading the rest of this directory",
                            ctx.display_path(dir_rel)
                        );
                        *parent = None;
                        None
                    }
                },
                None => None,
            };
            let child = Visit {
                depth: visit.depth,
                filter: child_filter,
                dev_bound: visit.dev_bound,
                pass_through: !named && (visit.pass_through || excluded),
                named,
                toward_named,
            };
            return Ok(Some(Ahead {
                item,
                posix,
                rel: child_rel,
                visit: child,
                parent_entry,
                read: None,
            }));
        }
        Ok(None)
    }

    /// 預讀這個項目嗎：是要讀內容的檔案（快速路徑不能沿用 parent 的，[`Walk::unchanged`]）、不大於
    /// [`READ_AHEAD_MAX`]。是的話回它列表上的大小。
    fn reads_ahead(&self, a: &Ahead) -> Option<u64> {
        let SourceItemKind::File {
            size,
            mtime_ns,
            ref etag,
            ..
        } = a.item.kind
        else {
            return None;
        };
        if size > READ_AHEAD_MAX {
            return None;
        }
        let facts = FileFacts {
            size,
            mtime_ns,
            posix: a.posix,
            etag: etag.clone(),
        };
        let reused = a
            .parent_entry
            .as_ref()
            .is_some_and(|pe| pe.t == node_type::FILE && self.unchanged(&facts, pe));
        (!reused).then_some(size)
    }

    /// [`Walk::process_entry_inner`]，做完回報一次進度（不論寫進目錄、略過或記成錯誤）。
    #[allow(clippy::too_many_arguments)] // 一個項目的全部事實；包成 struct 只是換個地方寫同樣的欄位
    async fn process_entry(
        &mut self,
        ctx: &SourceCtx,
        rel: &[u8],
        item: SourceItem,
        posix: Option<kist_backend::fsmeta::PosixMeta>,
        parent: Option<&Entry>,
        visit: Visit,
        read: Option<SourceRead>,
    ) -> Result<Option<(Entry, Used)>> {
        let out = self
            .process_entry_inner(ctx, rel, item, posix, parent, visit, read)
            .await?;
        report_progress(
            self.progress.as_ref(),
            "files",
            self.stats,
            self.report,
            Some(ctx.display_path(rel)),
        );
        Ok(out)
    }

    /// 處理一個目錄項目：回傳它的 Entry 與它沿用的 (item, 持有者)；讀不到的、別的檔案系統上的檔案回 `None`。
    /// `listed` 是列舉時取得的 metadata（本機來源的目錄列舉有；root 與其他來源沒有，這裡自己 lstat）。
    #[allow(clippy::too_many_arguments)] // 同 process_entry
    async fn process_entry_inner(
        &mut self,
        ctx: &SourceCtx,
        rel: &[u8],
        item: SourceItem,
        listed: Option<kist_backend::fsmeta::PosixMeta>,
        parent: Option<&Entry>,
        visit: Visit,
        read: Option<SourceRead>,
    ) -> Result<Option<(Entry, Used)>> {
        let mk = ctx.source.meta_kind();
        let posix = if mk == meta_kind::POSIX {
            listed.or_else(|| {
                ctx.source
                    .local_path(rel)
                    .and_then(|p| std::fs::symlink_metadata(&p).ok())
                    .map(|m| kist_backend::fsmeta::capture(&m))
            })
        } else {
            None
        };
        let other_fs = match (visit.dev_bound, &posix) {
            (Some(bound), Some(p)) => p.dev != bound && !visit.named,
            _ => false,
        };
        let mut entry = Entry::new(item.name.clone(), node_type::FILE, mk);
        let mut used = Used::new();
        match item.kind {
            SourceItemKind::Symlink { target } => {
                self.stats.symlinks += 1;
                entry.t = node_type::SYMLINK;
                entry.target = target;
                if let Some(p) = &posix {
                    fill_posix(&mut entry, p, false);
                }
            }
            SourceItemKind::Dir => {
                let parent_tree = match parent {
                    Some(e) if e.t == node_type::DIR => e.tree,
                    _ => None,
                };
                let tree = if other_fs && !visit.toward_named {
                    self.report.excluded += 1;
                    self.empty_dir(&mut used).await?
                } else {
                    let dev_bound = match (visit.named, visit.dev_bound, &posix) {
                        (true, Some(_), Some(p)) => Some(p.dev),
                        _ => visit.dev_bound,
                    };
                    let inside = Visit {
                        depth: visit.depth + 1,
                        filter: visit.filter.clone(),
                        dev_bound,
                        pass_through: visit.pass_through || other_fs,
                        named: false,
                        toward_named: false,
                    };
                    let (tree, sub) = self.walk_dir(ctx, rel, None, parent_tree, inside).await?;
                    used = sub;
                    tree
                };
                self.stats.dirs += 1;
                entry.t = node_type::DIR;
                entry.tree = Some(tree);
                match mk {
                    meta_kind::POSIX => {
                        if let Some(p) = &posix {
                            fill_posix(&mut entry, p, false);
                        }
                    }
                    // object_store 的前綴沒有時間：sftp 的目錄改記 generic（同 v3）
                    meta_kind::SFTP => entry.mk = meta_kind::GENERIC,
                    _ => {}
                }
            }
            SourceItemKind::File {
                size,
                mtime_ns,
                etag,
                vern,
            } => {
                if other_fs {
                    self.report.excluded += 1;
                    return Ok(None);
                }
                let facts = FileFacts {
                    size,
                    mtime_ns,
                    posix,
                    etag: etag.clone(),
                };
                let Some((content, file_used)) =
                    self.process_file(ctx, rel, &facts, parent, read).await?
                else {
                    return Ok(None);
                };
                used = file_used;
                self.stats.files += 1;
                entry.t = node_type::FILE;
                entry.size = content.size;
                entry.ct = content.ct;
                entry.chunks = content.chunks;
                entry.mtime = Some(mtime_ns);
                if mk == meta_kind::S3 {
                    entry.etag = etag.map(serde_bytes::ByteBuf::from);
                    entry.vern = vern.map(serde_bytes::ByteBuf::from);
                }
                match mk {
                    meta_kind::POSIX => match facts.posix {
                        Some(p) => fill_posix(&mut entry, &p, true),
                        None => {
                            self.skip(
                                &ctx.display_path(rel),
                                "posix source did not provide metadata",
                            );
                            return Ok(None);
                        }
                    },
                    meta_kind::SFTP => {
                        if let Some(p) = &facts.posix {
                            entry.mode = Some(p.mode);
                            entry.uid = Some(p.uid);
                            entry.gid = Some(p.gid);
                        }
                    }
                    _ => {}
                }
            }
        }
        if mk == meta_kind::POSIX {
            if let Some(path) = ctx.source.local_path(rel) {
                entry.xattrs = fsmeta::read_xattrs(&path);
            }
        }
        Ok(Some((entry, used)))
    }

    /// 一個檔案：快速路徑 → 硬連結 → 讀檔切塊。`None` 是讀不到、已記錄略過。
    /// `read` 是預讀已經開始讀的內容（[`Walk::reads_ahead`]）；沒有就在這裡開始讀。
    async fn process_file(
        &mut self,
        ctx: &SourceCtx,
        rel: &[u8],
        f: &FileFacts,
        parent: Option<&Entry>,
        read: Option<SourceRead>,
    ) -> Result<Option<(FileContent, Used)>> {
        #[cfg(unix)]
        if let Some(p) = f.posix {
            if p.mode & 0o170000 != 0o100000 {
                self.skip(
                    &ctx.display_path(rel),
                    "not a regular file (fifo/socket/device)",
                );
                return Ok(None);
            }
        }
        if let Some(reused) = self.try_reuse(f, parent).await? {
            self.report.files_reused += 1;
            return Ok(Some(reused));
        }
        let hl_key = f.posix.filter(|p| p.nlink > 1).map(|p| (p.dev, p.inode));
        if let Some(key) = hl_key {
            if let Some(hit) = self.hardlinks.get(&key) {
                // 第一個名字已經處理過它的持有者（或它在沿用的子樹裡，靠 parent）
                return Ok(Some((hit.clone(), Used::new())));
            }
        }
        let reader = match read.unwrap_or_else(|| ctx.source.read_sized(rel, f.size)) {
            Ok(r) => r,
            Err(e) => {
                self.skip(&ctx.display_path(rel), &e.to_string());
                return Ok(None);
            }
        };
        let display = ctx.display_path(rel);
        let Some(chunked) = self.chunk_reader(reader, &display).await? else {
            return Ok(None);
        };
        let ChunkState {
            list,
            mut used,
            bytes_total,
            bytes_new,
            chunks_new,
            chunks_reused,
            ..
        } = chunked;
        match hl_key {
            Some(key) => {
                if self.counted.insert(key) {
                    self.stats.bytes += bytes_total;
                }
            }
            None => self.stats.bytes += bytes_total,
        }
        self.report.bytes_stored += bytes_new;
        self.report.chunks_new += chunks_new;
        self.report.chunks_reused += chunks_reused;
        let keys = Arc::clone(&self.keys);
        let built = build_chunks(list, Limits::V4, &|n| keys.is_cut(n), &|b| keys.meta_id(b))?;
        self.place(built.items, &mut used).await?;
        let content = FileContent {
            size: bytes_total,
            ct: built.ct,
            chunks: built.chunks,
        };
        if let Some(key) = hl_key {
            self.hardlinks.insert(key, content.clone());
        }
        Ok(Some((content, used)))
    }

    /// 快速路徑的 metadata 判斷（同 v3 的分級）：posix 看 size、mtime、ctime、inode；s3 看 etag 與 size；sftp 預設
    /// 與 generic 一樣一律重讀，開了 [`BackupOptions::sftp_trust_mtime`] 而且這次可以用（[`decide_mtime_trust`]）時
    /// 看 size 與 mtime。只看列表給的事實與 parent 的 Entry：預讀（[`Walk::reads_ahead`]）用同一個判斷。
    fn unchanged(&self, f: &FileFacts, pe: &Entry) -> bool {
        match (f.posix.as_ref(), pe.mk) {
            (Some(p), meta_kind::POSIX) => {
                let now = fsmeta::FsMeta {
                    mode: p.mode,
                    uid: p.uid,
                    gid: p.gid,
                    mtime_ns: p.mtime_ns,
                    ctime_ns: p.ctime_ns,
                    inode: p.inode,
                    dev: p.dev,
                    nlink: p.nlink,
                };
                fsmeta::file_unchanged(&meta_of(pe), pe.size, &now, f.size, self.parent_start)
            }
            (None, meta_kind::S3) => match (&f.etag, &pe.etag) {
                (Some(etag), Some(petag)) => petag.as_ref() == etag.as_slice() && pe.size == f.size,
                _ => false,
            },
            (None, meta_kind::SFTP) if self.trust_sftp_mtime => {
                sftp_unchanged(pe, f.size, f.mtime_ns, self.parent_start)
            }
            _ => false,
        }
    }

    /// 快速路徑：metadata 沒變（[`Walk::unchanged`]），內容照 B2 判斷：`ct=0` 每個 chunk 都要有可信的持有者；
    /// `ct=1` 的清單是一棵子樹，只判斷它的根（沿用 parent 的子樹）。不成立就重讀。
    async fn try_reuse(
        &mut self,
        f: &FileFacts,
        parent: Option<&Entry>,
    ) -> Result<Option<(FileContent, Used)>> {
        let Some(pe) = parent else {
            return Ok(None);
        };
        if pe.t != node_type::FILE || !self.unchanged(f, pe) {
            return Ok(None);
        }
        let content = FileContent {
            size: pe.size,
            ct: pe.ct,
            chunks: pe.chunks.clone(),
        };
        // 不必取回驗證（候選是自己的或 prune 的 pack、這次寫過、沒有候選）就在 async 端判斷：每個檔案一次
        // spawn_blocking 的來回在 10 萬個沒變的檔案上是 42 萬次 futex、大半的 system time（第 7 段第十六步）。
        // 要取回驗證才換到 blocking 執行緒（`block_on` 等後端），從頭再判斷一次。
        let fast = self
            .work
            .as_mut()
            .ok_or_else(|| CoreError::Join("backup state missing".into()))?
            .reuse_file(&content, |d, id, kind| d.decide_cached(id, kind));
        let decided = match fast {
            Ok(decided) => decided,
            Err(Undecided::Failed(e)) => return Err(e),
            Err(Undecided::NeedFetch) => {
                let mut work = self.take_work()?;
                let check = content.clone();
                let (work, decided) = blocking(move || {
                    let decided = work.reuse_file(&check, |d, id, kind| d.decide(id, kind));
                    Ok((work, decided))
                })
                .await?;
                self.work = Some(work);
                decided?
            }
        };
        let Some((used, subtree)) = decided else {
            return Ok(None);
        };
        if subtree {
            self.used_subtree = true;
            self.report.subtrees += 1;
        }
        let count = match f.posix.filter(|p| p.nlink > 1) {
            Some(p) => self.counted.insert((p.dev, p.inode)),
            None => true,
        };
        if count {
            self.stats.bytes += pe.size;
        }
        Ok(Some((content, used)))
    }

    /// 切塊、照 B2 去重、打包。一輪 blocking 最多封一個 pack 就回 async 端上傳（同 v3）。讀到一半出錯回
    /// `None`（已記錄略過；已寫進 pack 的 chunk 留著無害）。
    async fn chunk_reader<R>(&mut self, reader: R, display: &str) -> Result<Option<ChunkState<R>>>
    where
        R: std::io::Read + Send + 'static,
    {
        let buf = self.chunk_bufs.pop().unwrap_or_default();
        let mut state = ChunkState {
            chunks: self.cdc.chunks_with_buf(reader, buf),
            list: Vec::new(),
            used: Used::new(),
            bytes_total: 0,
            bytes_new: 0,
            chunks_new: 0,
            chunks_reused: 0,
        };
        loop {
            let mut work = self.take_work()?;
            let keys = Arc::clone(&self.keys);
            let (work, state_back, finished, done, read_error, hard_error) = blocking(move || {
                let mut finished = None;
                let mut done = false;
                let mut read_error = None;
                let mut hard_error = None;
                loop {
                    let chunk = match state.chunks.next() {
                        None => {
                            done = true;
                            break;
                        }
                        Some(Err(e)) => {
                            read_error = Some(e.to_string());
                            done = true;
                            break;
                        }
                        Some(Ok(c)) => c,
                    };
                    let id = keys.chunk_id(&chunk);
                    let item: ItemId = id.into();
                    state.bytes_total += chunk.len() as u64;
                    state.list.push((id, chunk.len() as u64));
                    let decision = match work.dedup.decide(item, Kind::Data) {
                        Ok(d) => d,
                        Err(e) => {
                            hard_error = Some(e);
                            done = true;
                            break;
                        }
                    };
                    match decision {
                        Decision::InRun => continue,
                        Decision::Reuse(h) => {
                            let i = work.packs.intern(&h);
                            state.used.push((item, i));
                            state.chunks_reused += 1;
                            continue;
                        }
                        Decision::Write => {}
                    }
                    if let Err(e) = work.data.add(item, &chunk) {
                        hard_error = Some(e);
                        done = true;
                        break;
                    }
                    if let Err(e) = work.dedup.written.insert(item) {
                        hard_error = Some(e);
                        done = true;
                        break;
                    }
                    state.bytes_new += chunk.len() as u64;
                    state.chunks_new += 1;
                    if work.data.is_full() {
                        match work.data.finish() {
                            Ok(p) => finished = p,
                            Err(e) => {
                                hard_error = Some(e);
                                done = true;
                            }
                        }
                        break;
                    }
                }
                Ok((work, state, finished, done, read_error, hard_error))
            })
            .await?;
            self.work = Some(work);
            state = state_back;
            if let Some(p) = finished {
                self.handle_spooled(p).await?;
            }
            if let Some(e) = hard_error {
                self.chunk_bufs.push(state.chunks.take_buf());
                return Err(e);
            }
            if let Some(reason) = read_error {
                self.chunk_bufs.push(state.chunks.take_buf());
                self.skip(display, &reason);
                return Ok(None);
            }
            if done {
                self.chunk_bufs.push(state.chunks.take_buf());
                return Ok(Some(state));
            }
        }
    }

    /// B3：封好的 pack 或 mpack 交去上傳（只在不存在時建立；mpack 在 replicas=1 時先主體、後 `.r1`；parity
    /// 選配），記下來給 delta 與 B4。自己寫的 mpack 也放進記憶體與本地快取：下一次 backup 讀 parent 的樹不必
    /// 下載。
    async fn handle_spooled(&mut self, p: FinishedSpool) -> Result<()> {
        // 等到有上傳的空位才讀回記憶體（還不能直接從檔案上傳）：記憶體裡最多是在上傳的那一個
        self.wait_uploads(MAX_INFLIGHT_UPLOADS - 1).await?;
        let p = blocking(move || p.into_pack()).await?;
        self.handle_finished(p).await
    }

    async fn handle_finished(&mut self, p: FinishedPack) -> Result<()> {
        self.wait_uploads(MAX_INFLIGHT_UPLOADS - 1).await?;
        let FinishedPack {
            id,
            kind,
            bytes,
            entries,
        } = p;
        self.since_delta.push(DeltaPack {
            id,
            kind,
            size: bytes.len() as u64,
            entries,
        });
        self.own_packs.push(id);
        let replica = match kind {
            Kind::Data => {
                self.report.packs_new += 1;
                false
            }
            Kind::Meta => {
                self.report.mpacks_new += 1;
                if let Some(c) = self.repo.cache() {
                    c.put(CacheKind::Mpack, &id, &bytes);
                }
                self.reader.mpacks.insert(id, Arc::new(bytes.clone()));
                self.settings.replicas == 1
            }
        };
        let backend: Backend = self.repo.backend().clone();
        let key = pack_key(kind, self.own, id);
        let parity_key = Key::Parity {
            client: self.own,
            id,
        };
        let m = usize::from(self.parity);
        let path = key.path();
        self.uploads.spawn(async move {
            let (bytes, parity) = if m > 0 {
                blocking(move || {
                    let p = match kist_format::v4::parity::encode(&id, &bytes, m) {
                        Ok(p) => Some(p),
                        Err(e) => {
                            tracing::warn!("{path} is stored but its parity is not: {e}");
                            None
                        }
                    };
                    Ok((bytes, p))
                })
                .await?
            } else {
                (bytes, None)
            };
            // 以內容命名：「已存在」就是那份 bytes 已經在了（§12）
            let create = |k: Key, b: Vec<u8>| {
                let backend = backend.clone();
                async move {
                    match backend.put_if_absent(&k.path(), b).await {
                        Ok(()) | Err(BackendError::AlreadyExists(_)) => Ok::<_, CoreError>(()),
                        Err(e) => Err(e.into()),
                    }
                }
            };
            let replica_bytes = replica.then(|| bytes.clone());
            create(key.clone(), bytes).await?;
            if let Some(b) = replica_bytes {
                let Key::Mpack { client, id, .. } = key else {
                    return Err(CoreError::Join("mpack key expected".into()));
                };
                create(
                    Key::Mpack {
                        client,
                        id,
                        replica: true,
                    },
                    b,
                )
                .await?;
            }
            if let Some(p) = parity {
                if let Err(e) = create(parity_key.clone(), p).await {
                    tracing::warn!("{} is stored but its parity is not: {e}", parity_key.path());
                }
            }
            Ok(())
        });
        if self.since_delta.len() >= self.delta_every {
            self.write_delta().await?;
        }
        Ok(())
    }

    async fn wait_uploads(&mut self, max: usize) -> Result<()> {
        while self.uploads.len() > max {
            match self.uploads.join_next().await {
                Some(Ok(r)) => r?,
                Some(Err(e)) => return Err(CoreError::Join(e.to_string())),
                None => break,
            }
        }
        Ok(())
    }

    /// B3：等列在 delta 裡的 pack 都上傳完，再寫 delta。`E ≥ grace − M` 之後不再寫（這次 backup 到 B4 也會停）。
    async fn write_delta(&mut self) -> Result<()> {
        if self.since_delta.is_empty() {
            return Ok(());
        }
        self.wait_uploads(0).await?;
        let grace = Duration::from_secs(self.settings.grace);
        let e = elapsed(&self.started, &self.clock.now());
        if e + M >= grace {
            tracing::warn!(
                "the backup has run for {}s, within an hour of the GC grace ({}s); not writing more deltas",
                e.as_secs(),
                grace.as_secs()
            );
            self.since_delta.clear();
            return Ok(());
        }
        let packs = std::mem::take(&mut self.since_delta);
        let plain = Delta::new(self.own, packs).encode()?;
        let sealed = self.keys.seal_delta(&self.own, &plain)?;
        let id = ObjectId::of(&sealed);
        // 下一次 B1 讀 delta 時不必再下載自己剛寫的這一份
        if let Some(c) = self.repo.cache() {
            c.put(CacheKind::Delta, &id, &sealed);
        }
        let key = Key::Delta {
            client: self.own,
            id,
        };
        match self.repo.backend().put_if_absent(&key.path(), sealed).await {
            Ok(()) | Err(BackendError::AlreadyExists(_)) => {}
            Err(e) => return Err(e.into()),
        }
        self.report.deltas_new += 1;
        Ok(())
    }

    /// 走訪結束：剩下的 pack 與 mpack、等上傳完、最後一份 delta。
    async fn flush(&mut self) -> Result<()> {
        report_progress(
            self.progress.as_ref(),
            "flush",
            self.stats,
            self.report,
            None,
        );
        let mut work = self.take_work()?;
        let (work, data, meta) = blocking(move || {
            let data = work.data.finish();
            let meta = work.meta.finish();
            Ok((work, data, meta))
        })
        .await?;
        self.work = Some(work);
        if let Some(p) = data? {
            self.handle_spooled(p).await?;
        }
        if let Some(p) = meta? {
            self.handle_finished(p).await?;
        }
        self.wait_uploads(0).await?;
        self.write_delta().await
    }
}

fn dir_root(path: Vec<u8>, tree: MetaId) -> Root {
    Root {
        path,
        t: node_type::DIR,
        tree: Some(tree),
        entry: None,
    }
}

fn file_root(path: Vec<u8>, entry: Option<(Entry, Used)>, display: &str) -> Result<(Root, Used)> {
    let Some((entry, used)) = entry else {
        return Err(CoreError::Usage(format!(
            "{display}: source could not be read"
        )));
    };
    Ok((
        Root {
            path,
            t: entry.t,
            tree: None,
            entry: Some(entry),
        },
        used,
    ))
}

/// 只在不存在時建立的結果。
enum Created {
    /// 建立了，或「已存在」而 bytes 相同（自己剛寫進去的，S3 的 5xx 重試，§12）。
    Ours,
    /// 已存在而 bytes 不同：撞名。
    Collision,
}

async fn create_or_same(
    backend: &Backend,
    key: &str,
    bytes: &[u8],
) -> std::result::Result<Created, BackendError> {
    match backend.put_if_absent(key, bytes.to_vec()).await {
        Ok(()) => Ok(Created::Ours),
        Err(BackendError::AlreadyExists(_)) => {
            let existing = backend.get(key).await?;
            Ok(if existing == bytes {
                Created::Ours
            } else {
                Created::Collision
            })
        }
        Err(e) => Err(e),
    }
}

/// 除了 snapshot 之外全部寫完的 backup。拆成兩步讓測試能在中間插入維護角色的動作。
pub struct PreparedBackup {
    repo: Repo,
    reader: Reader,
    state: Arc<State>,
    client: ClientId,
    hostname: String,
    username: String,
    clock: Clock,
    started: Reading,
    roots: MetaId,
    parent: Option<String>,
    stats: SnapshotStats,
    report: BackupReport,
    used: Used,
    packs: UsedPacks,
    own_packs: Vec<ObjectId>,
    used_subtree: bool,
    settings: Settings,
    progress: Option<ProgressCallback>,
    root_summaries: Vec<RootSummary>,
    /// SFTP 來源整份讀了（沒有用快速路徑）：commit 之後在本機快取的這個檔名記下這次的開始。
    full_read: Option<String>,
    mtime_trust: Option<MtimeTrust>,
}

impl PreparedBackup {
    pub fn report(&self) -> BackupReport {
        self.report
    }

    /// B4 與 B5 的「E < min(grace(g_s), grace(g_c)) − M」。
    fn check_elapsed(&self, grace: Duration, step: &str) -> Result<()> {
        let e = elapsed(&self.started, &self.clock.now());
        if e + M >= grace {
            return Err(CoreError::NotCommitted(format!(
                "{step}: the backup has run for {}s; it must finish within the GC grace ({}s) minus one hour",
                e.as_secs(),
                grace.as_secs()
            )));
        }
        Ok(())
    }

    /// B4（緊接在寫 snapshot 之前）與 B5：檢查去重用到的每個 pack、沿用子樹時的 parent，然後寫 snapshot。
    pub async fn commit(self) -> Result<BackupSummary> {
        report_progress(
            self.progress.as_ref(),
            "verify",
            self.stats,
            self.report,
            None,
        );
        // B4
        let g_s = self.state.gen.g;
        let listing = self.repo.list_heads(Some(g_s)).await?;
        let g_c = match listing.keys().next_back() {
            Some(&g) if g >= g_s => g,
            _ => {
                return Err(CoreError::Generation(format!(
                    "generation {g_s}, which this backup started from, is no longer listed: the newest generation went backwards"
                )))
            }
        };
        let newer = if g_c == g_s {
            None
        } else {
            let gen = self.repo.read_head(g_c).await?;
            let map = self.repo.load_map(&gen).await?;
            Some((gen, map))
        };
        let (head_c, map_c) = match &newer {
            Some((gen, map)) => (&gen.head, map),
            None => (&self.state.gen.head, &self.state.map),
        };
        let grace = Duration::from_secs(self.settings.grace.min(head_c.settings.grace));
        self.check_elapsed(grace, "B4")?;
        let mut by_pack: HashMap<u32, Vec<ItemId>> = HashMap::new();
        for (id, i) in &self.used {
            by_pack.entry(*i).or_default().push(*id);
        }
        for (i, ids) in by_pack {
            let p = &self.packs.packs[i as usize];
            match map_c.record(&p.id) {
                Some(rec) => {
                    if matches!(
                        rec.status.s,
                        status::TOMBSTONE | status::UNREADABLE | status::UNKNOWN
                    ) {
                        return Err(CoreError::NotCommitted(format!(
                            "pack {} that this backup reuses is {} in generation {g_c}",
                            p.id,
                            status_name(rec.status.s)
                        )));
                    }
                    if g_c != g_s {
                        let have: HashSet<ItemId> = rec.entries.iter().map(|e| e.i).collect();
                        if let Some(missing) = ids.iter().find(|id| !have.contains(id)) {
                            return Err(CoreError::NotCommitted(format!(
                                "pack {} no longer holds {missing} in generation {g_c}",
                                p.id
                            )));
                        }
                    }
                }
                None if p.source == HolderSource::Map => {
                    return Err(CoreError::NotCommitted(format!(
                        "pack {} that this backup reuses left the map in generation {g_c}",
                        p.id
                    )))
                }
                None => {
                    let key = pack_key(p.kind, p.client, p.id);
                    let trailer = self.reader.trailer(&key).await.map_err(|e| {
                        CoreError::NotCommitted(format!(
                            "pack {} (known from a delta) cannot be checked: {e}",
                            key.path()
                        ))
                    })?;
                    let have: HashSet<ItemId> = trailer.entries.iter().map(|e| e.i).collect();
                    if let Some(missing) = ids.iter().find(|id| !have.contains(id)) {
                        return Err(CoreError::NotCommitted(format!(
                            "pack {} does not hold {missing}, which its delta claims",
                            key.path()
                        )));
                    }
                }
            }
        }
        for id in &self.own_packs {
            if map_c
                .record(id)
                .is_some_and(|r| r.status.s == status::TOMBSTONE)
            {
                return Err(CoreError::NotCommitted(format!(
                    "pack {id} that this backup wrote is a tombstone in generation {g_c}"
                )));
            }
        }
        if self.used_subtree {
            if let Some(parent) = &self.parent {
                match self.repo.backend().head(parent).await {
                    Ok(_) => {}
                    Err(BackendError::NotFound(_)) => {
                        return Err(CoreError::NotCommitted(format!(
                            "the parent snapshot {parent} is gone, and this backup reuses its subtrees"
                        )))
                    }
                    Err(e) => return Err(e.into()),
                }
            }
        }

        // B5
        report_progress(
            self.progress.as_ref(),
            "commit",
            self.stats,
            self.report,
            None,
        );
        let key = self.write_snapshot(grace).await?;
        if let Some(c) = self.repo.cache() {
            c.add_verified(&self.reader.take_new_verified());
            c.set_seen_gen(g_c);
            c.set_last_snapshot(&key);
            if let Some(name) = &self.full_read {
                let started = i64::try_from(self.started.wall.unix_timestamp_nanos())
                    .map_err(|_| CoreError::Usage("the clock is out of range".to_owned()))?;
                c.set_last_full_read(name, started);
            }
        }
        Ok(BackupSummary {
            snapshot: key,
            parent: self.parent,
            generation: g_c,
            stats: self.stats,
            report: self.report,
            roots: self.root_summaries,
            mtime_trust: self.mtime_trust,
        })
    }

    /// B5：再檢查一次 E；replicas=1 時寫 `.r1`；再檢查一次 E；寫主體＝commit。「已存在」時讀回比對：bytes
    /// 相同是自己剛寫進去的；不同就把時間戳 +1 奈秒，重新封、重寫 `.r1`（§9）。
    async fn write_snapshot(&self, grace: Duration) -> Result<String> {
        let base = i64::try_from(self.started.wall.unix_timestamp_nanos())
            .map_err(|_| CoreError::Usage("the clock is out of range".to_owned()))?;
        let backend = self.repo.backend();
        for attempt in 0..3i64 {
            let time = base + attempt;
            let ts = SnapTs::from_nanos(time)?;
            let snap = Snapshot {
                v: kist_format::v4::VERSION,
                roots: self.roots,
                time,
                host: self.hostname.clone(),
                user: self.username.clone(),
                client: self.client,
                parent: self.parent.clone(),
                stats: self.stats,
            };
            let sealed = self
                .repo
                .keys()
                .seal_snapshot(&self.client, &ts, &snap.encode()?)?;
            let body = Key::Snapshot {
                client: self.client,
                ts: ts.clone(),
                replica: false,
            }
            .path();
            self.check_elapsed(grace, "B5")?;
            if self.settings.replicas == 1 {
                let r1 = Key::Snapshot {
                    client: self.client,
                    ts,
                    replica: true,
                }
                .path();
                if let Created::Collision = create_or_same(backend, &r1, &sealed).await? {
                    continue;
                }
                self.check_elapsed(grace, "B5")?;
            }
            match create_or_same(backend, &body, &sealed).await {
                Ok(Created::Ours) => return Ok(body),
                Ok(Created::Collision) => continue,
                Err(source) => return Err(CoreError::CommitUnknown { key: body, source }),
            }
        }
        Err(CoreError::NotCommitted(
            "the snapshot key collided three times".to_owned(),
        ))
    }
}

fn status_name(s: u8) -> &'static str {
    match s {
        status::LIVE => "live",
        status::CONDEMNED => "condemned",
        status::TOMBSTONE => "a tombstone",
        status::UNREADABLE => "unreadable",
        _ => "of an unknown version",
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// [`Peeked`] 先交出取走的那一個、posix 是它的，之後照原本的列表（第一個是錯誤的條目、列表是空的也一樣）。
    #[test]
    fn peeked_yields_the_taken_item_first_with_its_posix() {
        struct Listing {
            items: VecDeque<std::result::Result<(u8, u32), String>>,
            last: Option<u32>,
        }
        impl SortedItems for Listing {
            fn next_item(&mut self) -> Option<std::result::Result<SourceItem, BackendError>> {
                let next = self.items.pop_front()?;
                self.last = next.as_ref().ok().map(|&(_, mode)| mode);
                Some(
                    next.map(|(name, _)| SourceItem {
                        name: vec![name],
                        kind: SourceItemKind::Dir,
                    })
                    .map_err(BackendError::Sftp),
                )
            }
            fn posix(&self) -> Option<kist_backend::fsmeta::PosixMeta> {
                self.last.map(|mode| kist_backend::fsmeta::PosixMeta {
                    mode,
                    uid: 0,
                    gid: 0,
                    mtime_ns: 0,
                    ctime_ns: 0,
                    inode: 0,
                    dev: 0,
                    nlink: 1,
                })
            }
        }
        let peek = |items: Vec<std::result::Result<(u8, u32), String>>| {
            let mut rest: Box<dyn SortedItems + Send> = Box::new(Listing {
                items: items.into(),
                last: None,
            });
            Peeked {
                first: Some(rest.next_item()),
                rest,
            }
        };
        let walk = |mut l: Peeked| {
            let mut seen = Vec::new();
            while let Some(item) = l.next_item() {
                seen.push(match item {
                    Ok(i) => format!("{} {:?}", i.name[0], l.posix().map(|p| p.mode)),
                    Err(e) => format!("error {e} {:?}", l.posix().map(|p| p.mode)),
                });
            }
            seen
        };
        assert_eq!(
            walk(peek(vec![Ok((1, 10)), Ok((2, 20)), Ok((3, 30))])),
            ["1 Some(10)", "2 Some(20)", "3 Some(30)"]
        );
        assert_eq!(
            walk(peek(vec![Err("gone".into()), Ok((2, 20))])),
            ["error sftp backend: gone None", "2 Some(20)"]
        );
        assert!(walk(peek(Vec::new())).is_empty());
    }

    /// §11.1：E ＝ max(牆鐘經過, 含休眠的單調時鐘經過)。
    #[test]
    fn elapsed_takes_the_larger_of_the_wall_and_boot_clocks() {
        let h = Duration::from_secs(3600);
        let t0 = time::OffsetDateTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let start = Reading {
            wall: t0,
            boot: Duration::from_secs(10),
        };
        let at = |wall: time::OffsetDateTime, boot: Duration| Reading {
            wall,
            boot: start.boot + boot,
        };
        // 休眠：單調時鐘多算了睡著的時間
        assert_eq!(elapsed(&start, &at(t0 + h, 3 * h)), 3 * h);
        // 牆鐘被往回調：單調時鐘照算
        assert_eq!(elapsed(&start, &at(t0 - 5 * h, 2 * h)), 2 * h);
        // 牆鐘往前跳（沒有計入休眠的平台上，牆鐘那一半補上）
        assert_eq!(elapsed(&start, &at(t0 + 4 * h, h)), 4 * h);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_boot_clock_moves_forward() {
        let a = boot_time();
        std::thread::sleep(Duration::from_millis(5));
        assert!(boot_time() > a);
    }
}
