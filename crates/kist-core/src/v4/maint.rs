//! 維護角色的共用步驟（§11.1、§11.3）與改 settings（§11.4）。prune、重建與其他維護命令都從 [`Session`]
//! 開始。
//!
//! - **檔案鎖**：同一台主機一次一個維護命令（§11.3）。鎖檔在這個 repo 的本地快取目錄裡（`maint.lock`）；
//!   沒有快取時在系統的暫存目錄，以快取目錄的名稱（repo 位置的 keyed hash，§3）區分 repo。
//! - **P0**：[`Backend::probe_with`](kist_backend::Backend::probe_with)，本機牆鐘取自命令的時鐘；記下探針的
//!   後端時間 `N_P` 與那時的本機牆鐘。
//! - **P1**（[`Session::newest`]）：讀列出來的最大世代（主體，不行讀 `.r1`）；`T(g)` 是讀到的那一份的修改時間。
//!   `T(g)` 比本機時鐘晚超過 `M`：回報，以「本機時鐘 + M」當作現在（`T(g)` 當成起點時仍用原值）。列表裡有
//!   主體、沒有 `.r1`：[`Session::repair_r1`] 以只在不存在時建立補寫，失敗就停止。
//! - **柵欄**（P8，[`Session::fence`]）：HEAD 下一代的 `.r1`（存在就是輸了），再以只在不存在時建立寫主體
//!   （「已存在」就是輸了，其他錯誤是寫了與否不明）；成功之後寫 `.r1`，失敗就停止、什麼都不刪。
//! - **兩個時鐘**（§11.1）：「自某個世代起已過一個 grace」要後端時間 `T` 與寫入者時間 `w` 都成立
//!   （[`window_passed`]）；「現在」每個時鐘取 base head 的值與 P0 的讀數中較小的（[`Session::now`]）。
//!
//! 維護命令一律讀後端、不用本地快取（[`Repo::uncached`]）：它們依讀到的內容決定刪什麼，快取裡的那一份證明
//! 不了後端還在。

use std::collections::HashMap;
use std::time::Duration;

use kist_backend::BackendError;
use kist_format::v4::index::{status, Drain, Head, Settings, Status};
use kist_format::v4::keys::{Gen, Key};

use super::backup::{elapsed, Clock, Reading, M};
use super::repo::Repo;
use super::state::{HeadCopy, HeadListing};
use crate::{CoreError, Result};

/// 維護命令共用的選項。
#[derive(Debug, Clone)]
pub struct MaintOptions {
    /// 牆鐘與含休眠的單調時鐘：P0 的本機讀數、head 的 `w`、整輪時限。測試注入假的。
    #[doc(hidden)]
    pub clock: Clock,
}

impl Default for MaintOptions {
    fn default() -> Self {
        Self {
            clock: Clock::system(),
        }
    }
}

/// 一個時間點在兩個時鐘上的值（§11.1）：後端時間 `T` 與寫入者時間 `w`，都是 Unix 奈秒。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pair {
    pub t: i64,
    pub w: i64,
}

const NANOS: i64 = 1_000_000_000;

/// 秒 → 奈秒（grace 與 age 是秒）。
pub(crate) fn secs(s: u64) -> i64 {
    i64::try_from(s)
        .unwrap_or(i64::MAX / NANOS)
        .saturating_mul(NANOS)
}

pub(crate) fn unix_ns(t: time::OffsetDateTime) -> i64 {
    i64::try_from(t.unix_timestamp_nanos()).unwrap_or(i64::MAX)
}

/// 「自 `start` 起已過 `span` 秒」：兩個時鐘都要成立（§11.1）。
pub fn window_passed(start: Pair, now: Pair, span: u64) -> bool {
    start.t.saturating_add(secs(span)) <= now.t && start.w.saturating_add(secs(span)) <= now.w
}

/// P1 讀到的最大世代。
#[derive(Debug, Clone)]
pub(crate) struct Newest {
    pub copy: HeadCopy,
    /// 當作現在的 `T(g)`：比本機時鐘晚超過 `M` 時是「本機時鐘 + M」。原值在 [`Session::times`]（`T(g)` 當成
    /// 某個世代的起點時用它）。
    pub now_t: i64,
    /// `T(g)` 比本機時鐘晚超過 `M` 時的回報。
    pub ahead: Option<String>,
}

impl Newest {
    pub fn g(&self) -> Gen {
        self.copy.gen.g
    }

    pub fn head(&self) -> &Head {
        &self.copy.gen.head
    }
}

/// 一個維護命令：持有檔案鎖、通過了 P0。
pub(crate) struct Session {
    /// 不用本地快取的 repo。
    pub repo: Repo,
    clock: Clock,
    /// P0 開始的時間：整輪時限從這裡算（§11.1）。
    started: Reading,
    _lock: std::fs::File,
    /// P0 的讀數：探針的後端時間 `N_P` 與那時的本機牆鐘。
    pub probe: Pair,
    /// P1 的列表（之後讀較舊的 head 時，`T` 取自它）。
    listing: HeadListing,
    times: HashMap<Gen, Pair>,
}

/// 同一台主機一次一個維護命令（§11.3）：鎖檔在這個 repo 的本地快取目錄裡；沒有快取時在系統的暫存目錄，以
/// 快取目錄的名稱區分 repo。程序結束或 drop 時釋放。
pub(crate) fn lock(repo: &Repo) -> Result<std::fs::File> {
    let path = match repo.cache() {
        Some(c) => c.dir().join("maint.lock"),
        None => std::env::temp_dir().join(format!(
            "kist-maint-{}.lock",
            repo.keys()
                .cache_dir_name(&repo.backend().location().to_string())
        )),
    };
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| CoreError::io(dir, e))?;
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .map_err(|e| CoreError::io(&path, e))?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(std::fs::TryLockError::WouldBlock) => Err(CoreError::Usage(format!(
            "another maintenance command (prune, rebuild, settings, forget, key) is running on \
             this host for this repository (lock file {})",
            path.display()
        ))),
        Err(std::fs::TryLockError::Error(e)) => Err(CoreError::io(&path, e)),
    }
}

impl Session {
    /// 取得檔案鎖、執行 P0。
    pub async fn begin(repo: &Repo, clock: Clock) -> Result<Self> {
        let lock = lock(repo)?;
        let started = clock.now();
        let wall = {
            let clock = clock.clone();
            move || clock.now().wall
        };
        let p = repo.backend().probe_with(&wall).await?;
        Ok(Self {
            repo: repo.uncached(),
            clock,
            started,
            _lock: lock,
            probe: Pair {
                t: unix_ns(p.backend),
                w: unix_ns(p.local),
            },
            listing: HeadListing::new(),
            times: HashMap::new(),
        })
    }

    /// 本機牆鐘（Unix 奈秒）：head 的 `w`。
    pub fn wall(&self) -> i64 {
        unix_ns(self.clock.now().wall)
    }

    /// 整輪時限（§11.1）：從 P0 起已過 `grace − M`（經過時間取牆鐘與含休眠的單調時鐘中較大的）。
    pub fn out_of_time(&self, grace: u64) -> bool {
        self.elapsed() + M >= Duration::from_secs(grace)
    }

    /// 從 P0 起經過的時間。
    pub fn elapsed(&self) -> Duration {
        elapsed(&self.started, &self.clock.now())
    }

    /// 列 `maint/gen/`，存起來給之後的 `T` 用。
    pub async fn list(&mut self) -> Result<&HeadListing> {
        self.listing = self.repo.list_heads(None).await?;
        Ok(&self.listing)
    }

    /// 列表裡 head g 的那一份的修改時間。
    fn modified(&self, g: Gen, replica: bool) -> Result<i64> {
        let c = self.listing.get(&g).copied().unwrap_or_default();
        let t = if replica { c.replica } else { c.body };
        t.map(unix_ns).ok_or_else(|| {
            CoreError::Generation(format!(
                "generation head {g}{} was read but is not in the listing",
                if replica { ".r1" } else { "" }
            ))
        })
    }

    /// P1：列出來的最大世代。讀不了、版本不認得就是錯誤（呼叫端停止）；`T(g)` 比本機時鐘晚超過 `M` 時回報，
    /// 以「本機時鐘 + M」當作現在。
    pub async fn newest(&mut self) -> Result<Newest> {
        self.list().await?;
        let Some(&g) = self.listing.keys().next_back() else {
            return Err(CoreError::Generation(
                "no generation head under maint/gen/: not a v4 repository, or its init did not finish"
                    .to_owned(),
            ));
        };
        let copy = self.repo.read_head_copy(g).await?;
        let t = self.modified(g, copy.replica)?;
        self.times.insert(
            g,
            Pair {
                t,
                w: copy.gen.head.w,
            },
        );
        let local = self.wall();
        let limit = local.saturating_add(secs(M.as_secs()));
        let (now_t, ahead) = if t > limit {
            let msg = format!(
                "generation head {g} has backend time {}, more than an hour ahead of the local \
                 clock; using the local clock plus one hour as now",
                fmt_ns(t)
            );
            tracing::warn!("{msg}");
            (limit, Some(msg))
        } else {
            (t, None)
        };
        Ok(Newest { copy, now_t, ahead })
    }

    /// head g 的 `T` 與 `w`（較舊的世代：起點、drain、hold）。head 永不刪除，讀不到就是錯誤。
    pub async fn times(&mut self, g: Gen) -> Result<Pair> {
        if let Some(p) = self.times.get(&g) {
            return Ok(*p);
        }
        let copy = self.repo.read_head_copy(g).await?;
        let p = Pair {
            t: self.modified(g, copy.replica)?,
            w: copy.gen.head.w,
        };
        self.times.insert(g, p);
        Ok(p)
    }

    /// 「已過一個 grace」的現在（§11.1）：每個時鐘取 base head 的值（`T` 是可能被調整過的 `now_t`）與 P0 的
    /// 讀數中較小的。
    pub fn now(&self, base: &Newest) -> Pair {
        Pair {
            t: base.now_t.min(self.probe.t),
            w: base.head().w.min(self.probe.w),
        }
    }

    /// P1：列表裡有主體、沒有 `.r1` 的最大世代（寫入者在兩次 PUT 之間當掉）→ 以只在不存在時建立補寫 `.r1`
    /// （讀到的主體的 bytes）。「已存在」是別人先補了；其他錯誤就停止：不在只有一份的 head 上建下一代。
    pub async fn repair_r1(&self, newest: &Newest) -> Result<()> {
        let g = newest.g();
        let copies = self.listing.get(&g).copied().unwrap_or_default();
        if copies.replica.is_some() || newest.copy.replica {
            return Ok(());
        }
        let key = Key::Head { g, replica: true }.path();
        match self
            .repo
            .backend()
            .put_if_absent(&key, newest.copy.bytes.clone())
            .await
        {
            Ok(()) => {
                tracing::warn!(
                    "generation head {g} had no .r1 (its writer stopped between its two writes); \
                     wrote it"
                );
                Ok(())
            }
            Err(BackendError::AlreadyExists(_)) => Ok(()),
            Err(e) => Err(CoreError::Generation(format!(
                "generation head {g} has no .r1 and writing it failed: {e}; not building the next \
                 generation on a head with one copy"
            ))),
        }
    }

    /// 柵欄（§11.3 P8）：以 `base` 為基礎寫 head `g+1`（`parent`、`w` 在這裡填）。HEAD 它的 `.r1`：存在就是
    /// 輸了；再以只在不存在時建立寫主體：「已存在」是輸了，其他錯誤是寫了與否不明——兩者都停止、什麼都不刪。
    /// 成功之後才寫 `.r1`：失敗（不是「已存在」）也停止、什麼都不刪，主體留著，下一次維護補寫（P1）。
    pub async fn fence(&self, base: &Newest, head: Head) -> Result<Gen> {
        let g = base.g() + 1;
        let head = Head {
            g,
            w: self.wall(),
            parent: Some(base.copy.gen.hash),
            ..head
        };
        let sealed = self.repo.keys().seal_head(g, &head.encode()?)?;
        let backend = self.repo.backend();
        let r1 = Key::Head { g, replica: true }.path();
        if backend.exists(&r1).await? {
            return Err(CoreError::Superseded(format!(
                "generation head {g} already exists (its .r1 is there): another maintenance run won"
            )));
        }
        let body = Key::Head { g, replica: false }.path();
        match backend.put_if_absent(&body, sealed.clone()).await {
            Ok(()) => {}
            Err(BackendError::AlreadyExists(_)) => {
                return Err(CoreError::Superseded(format!(
                    "generation head {g} already exists: another maintenance run won"
                )))
            }
            Err(e) => {
                return Err(CoreError::Generation(format!(
                    "writing generation head {g} failed, so whether it was written is unknown: \
                     {e}; nothing was deleted (if it was written, the next maintenance run \
                     writes its .r1)"
                )))
            }
        }
        match backend.put_if_absent(&r1, sealed).await {
            Ok(()) | Err(BackendError::AlreadyExists(_)) => Ok(g),
            Err(e) => Err(CoreError::Generation(format!(
                "generation head {g} is written but its .r1 is not: {e}; nothing was deleted, \
                 the next maintenance run writes the .r1"
            ))),
        }
    }
}

/// Unix 奈秒 → RFC 3339（訊息用）。
pub(crate) fn fmt_ns(ns: i64) -> String {
    time::OffsetDateTime::from_unix_timestamp_nanos(i128::from(ns))
        .ok()
        .and_then(|t| {
            t.format(&time::format_description::well_known::Rfc3339)
                .ok()
        })
        .unwrap_or_else(|| ns.to_string())
}

/// 改 settings：沒給的欄位不變。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SettingsChange {
    /// 秒。
    pub grace: Option<u64>,
    pub replicas: Option<u8>,
    pub pack_target: Option<u64>,
    pub mpack_target: Option<u64>,
}

impl SettingsChange {
    fn apply(&self, s: Settings) -> Settings {
        Settings {
            grace: self.grace.unwrap_or(s.grace),
            replicas: self.replicas.unwrap_or(s.replicas),
            pack_target: self.pack_target.unwrap_or(s.pack_target),
            mpack_target: self.mpack_target.unwrap_or(s.mpack_target),
        }
    }
}

/// 改 settings 的結果。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct SettingsReport {
    /// 寫出的世代；沒有改變時是 `None`（不寫 head）。
    pub generation: Option<Gen>,
    pub settings: Settings,
    /// 寫出的 head 的 drain（調低 grace 時開始，§11.4）。
    pub drain: Option<Drain>,
}

impl Repo {
    /// 改 settings（§11.4）：P0、P1 之後直接以柵欄寫 head g+1（分片不變），不刪任何東西。grace 調高立刻生效；
    /// 調低開始 drain（`from` 是新的世代、`old_grace` 是原本的 grace）。drain 在兩個時鐘上都過了一個舊的 grace
    /// 才結束，期間不接受 grace 的變更（其他 settings 照改）；結束之後的下一次改動拿掉它。
    pub async fn change_settings(
        &self,
        change: SettingsChange,
        opts: MaintOptions,
    ) -> Result<SettingsReport> {
        let mut session = Session::begin(self, opts.clock).await?;
        let newest = session.newest().await?;
        let cur = newest.head().clone();
        let settings = change.apply(cur.settings);
        settings
            .check()
            .map_err(|e| CoreError::Usage(e.to_string()))?;
        if settings == cur.settings {
            return Ok(SettingsReport {
                generation: None,
                settings,
                drain: cur.drain,
            });
        }
        let mut drain = cur.drain;
        if let Some(d) = drain {
            let from = session.times(d.from).await?;
            if window_passed(from, session.now(&newest), d.old_grace) {
                drain = None;
            } else if settings.grace != cur.settings.grace {
                return Err(CoreError::Usage(format!(
                    "the grace was lowered from {}s at generation {}; until that drain ends (one old \
                     grace after generation {} on both clocks) the grace cannot change",
                    d.old_grace, d.from, d.from
                )));
            }
        }
        if settings.grace < cur.settings.grace {
            drain = Some(Drain {
                from: newest.g() + 1,
                old_grace: cur.settings.grace,
            });
        }
        session.repair_r1(&newest).await?;
        let g = session
            .fence(
                &newest,
                Head {
                    settings,
                    drain,
                    ..cur
                },
            )
            .await?;
        Ok(SettingsReport {
            generation: Some(g),
            settings,
            drain,
        })
    }
}

/// 移除版本不認得的 pack 的結果。
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct UnknownReport {
    /// 寫出的世代；map 裡沒有 unknown 的記錄時是 `None`（不寫 head）。
    pub generation: Option<Gen>,
    /// 改成 condemned 的 pack（key）。
    pub packs: Vec<String>,
}

impl Repo {
    /// 移除版本不認得的 pack（ADR 026 負責人決定 10）：prune 把它們記成 unknown、不使用也不刪除；這個命令由維護
    /// 角色明確下令，以柵欄寫一代把它們改成 condemned（`at` 是新的世代，age 是目前的 age，drain 中是舊的 grace），
    /// 其他不變。之後照一般的兩階段刪除：過了 age 的 prune 標成 tombstone、P9 刪除——不另外多一個刪除者。unknown
    /// 的記錄沒有項目，不會是任何 ID 的正本；較新的 kist 的 snapshot 若引用它們，prune 在 P5 就停住（版本不認得、
    /// 或沒有可讀的持有者），走不到刪除。
    pub async fn condemn_unknown(&self, opts: MaintOptions) -> Result<UnknownReport> {
        let mut session = Session::begin(self, opts.clock).await?;
        let newest = session.newest().await?;
        let base: std::collections::BTreeMap<_, _> = session
            .repo
            .load_map(&newest.copy.gen)
            .await?
            .records()
            .iter()
            .map(|r| (r.id, r.clone()))
            .collect();
        let g = newest.g() + 1;
        let head = newest.head().clone();
        let mut age = head.settings.grace;
        if let Some(d) = head.drain {
            let from = session.times(d.from).await?;
            if !window_passed(from, session.now(&newest), d.old_grace) {
                age = d.old_grace;
            }
        }
        let mut map = base.clone();
        let mut report = UnknownReport::default();
        for r in map.values_mut() {
            if r.status.s == status::UNKNOWN {
                r.status = Status {
                    s: status::CONDEMNED,
                    at: g,
                    age,
                };
                report
                    .packs
                    .push(super::state::pack_key(r.kind, r.client, r.id).path());
            }
        }
        if report.packs.is_empty() {
            return Ok(report);
        }
        session.repair_r1(&newest).await?;
        let mut head = head;
        super::prune::put_changed_shards(&session.repo, &base, &map, &mut head).await?;
        report.generation = Some(session.fence(&newest, head).await?);
        Ok(report)
    }
}
