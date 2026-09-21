//! 排程器：依設定檔裡各節的 cron 在對的時間跑對的工作，一次只跑一件（backup、forget、prune 串行，
//! 不會跟自己重疊；跟別台機器的重疊由 M3 的 GC 設計負責）。
//!
//! 除了排程，也接受手動觸發（Web UI 的「Run backup now」）：`DaemonHandle::trigger` 把工作丟進
//! 容量 1 的 channel，主迴圈閒著就立刻跑；正在跑別的工作時最多只排一件（再多回 `Queued`）。
//! 沒有任何排程也能常駐（只等觸發與關機），這是 UI-only 的 `serve` 需要的。
//! 進行中的工作、排隊中的工作、最近的結果放在 `DaemonState`（記憶體，重啟歸零），給 UI 讀。

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

use kist_core::{BackupProgress, ProgressCallback};
use serde::Serialize;
use time::OffsetDateTime;
use tokio::sync::{mpsc, watch};

use crate::config::Config;
use crate::jobs::{run_job, JobKind, JobOutcome, JobStatus};
use crate::jobstate;
use crate::metrics::unix_now;
use crate::notify::Notifier;
use crate::schedule::Schedule;
use crate::{AppError, Result};

/// history 最多留幾筆（記憶體裡，給 UI 的「Recent jobs」）。
const HISTORY_MAX: usize = 20;

/// 正在跑的工作。
#[derive(Debug, Clone, Serialize)]
pub struct RunningJob {
    pub job: JobKind,
    /// RFC 3339。
    pub started: String,
    /// Unix 秒（算「已經跑了多久」用）。
    pub started_unix: f64,
    /// 只有 backup 有；每處理完一個項目更新一次。
    pub progress: Option<BackupProgress>,
}

/// daemon 的即時狀態（UI 讀的）。
#[derive(Debug, Clone, Default, Serialize)]
pub struct DaemonState {
    pub running: Option<RunningJob>,
    /// 排隊中（觸發了但主迴圈還在跑別的工作）。channel 容量 1，所以最多一件。
    pub queued: Vec<JobKind>,
    /// 最近的結果，新的在前，最多 `HISTORY_MAX` 筆。
    pub history: Vec<JobOutcome>,
}

#[derive(Debug, thiserror::Error)]
pub enum TriggerError {
    #[error("a job is already queued")]
    Queued,
    #[error("{0} is not configured")]
    NotConfigured(&'static str),
    #[error("daemon has stopped")]
    Stopped,
}

/// 給別的 task（HTTP handler）用的把手：讀設定與狀態、看排程、觸發工作。可以隨便 clone。
#[derive(Clone)]
pub struct DaemonHandle {
    cfg: Arc<Config>,
    state: Arc<Mutex<DaemonState>>,
    trigger_tx: mpsc::Sender<JobKind>,
    /// 依工作名稱排序（backup、forget、prune）。
    schedules: Vec<(JobKind, Schedule)>,
}

impl DaemonHandle {
    pub fn config(&self) -> &Config {
        &self.cfg
    }

    /// 目前狀態的快照（在鎖裡 clone 出來）。
    pub fn state(&self) -> DaemonState {
        lock_state(&self.state).clone()
    }

    /// 有排程的工作與各自的下一次時間。
    pub fn next_runs(&self, after: OffsetDateTime) -> Vec<(JobKind, Option<OffsetDateTime>)> {
        self.schedules
            .iter()
            .map(|(k, s)| (*k, s.next_after(after)))
            .collect()
    }

    /// 有排程的工作與各自的 cron 字串。
    pub fn schedules(&self) -> Vec<(JobKind, String)> {
        self.schedules
            .iter()
            .map(|(k, s)| (*k, s.expr().to_owned()))
            .collect()
    }

    /// 排進佇列（容量 1）。已有排隊中的工作 → `Err(Queued)`；設定裡沒有這種工作 →
    /// `Err(NotConfigured)`；daemon 已結束 → `Err(Stopped)`。
    ///
    /// `try_send` 與 `queued.push` 在同一次鎖裡做：主迴圈收到後也是在鎖裡把它從 `queued`
    /// 拿掉，這樣不會出現「收到了、拿掉了、然後才 push」的殘留。
    pub fn trigger(&self, kind: JobKind) -> std::result::Result<(), TriggerError> {
        if !configured(&self.cfg, kind) {
            return Err(TriggerError::NotConfigured(kind.name()));
        }
        let mut st = lock_state(&self.state);
        match self.trigger_tx.try_send(kind) {
            Ok(()) => {
                st.queued.push(kind);
                Ok(())
            }
            Err(mpsc::error::TrySendError::Full(_)) => Err(TriggerError::Queued),
            Err(mpsc::error::TrySendError::Closed(_)) => Err(TriggerError::Stopped),
        }
    }
}

pub struct Daemon {
    cfg: Arc<Config>,
    schedules: HashMap<JobKind, Schedule>,
    notifier: Option<Notifier>,
    metrics: Arc<crate::metrics::Metrics>,
    state: Arc<Mutex<DaemonState>>,
    trigger_tx: mpsc::Sender<JobKind>,
    /// `run` 會把它拿走；第二次 `run` 就拿不到（= 程式寫錯）。
    trigger_rx: Option<mpsc::Receiver<JobKind>>,
}

impl Daemon {
    pub fn new(cfg: Config) -> Result<Self> {
        cfg.validate()?;
        let mut schedules = HashMap::new();
        if let Some(b) = &cfg.backup {
            if let Some(s) = cfg.schedule_of(b.schedule.as_deref(), "[backup]")? {
                schedules.insert(JobKind::Backup, s);
            }
        }
        if let Some(f) = &cfg.forget {
            if let Some(s) = cfg.schedule_of(f.schedule.as_deref(), "[forget]")? {
                schedules.insert(JobKind::Forget, s);
            }
        }
        if let Some(p) = &cfg.prune {
            if let Some(s) = cfg.schedule_of(p.schedule.as_deref(), "[prune]")? {
                schedules.insert(JobKind::Prune, s);
            }
        }
        let notifier = cfg
            .notify
            .as_ref()
            .map(Notifier::new)
            .transpose()
            .map_err(AppError::Config)?;
        let metrics = Arc::new(crate::metrics::Metrics::new());
        if let Some(dir) = &cfg.cache_dir {
            for kind in [JobKind::Backup, JobKind::Forget, JobKind::Prune] {
                if let Some(at) = jobstate::read(dir, &cfg.repo, kind) {
                    metrics.seed_last_success(kind, at);
                }
            }
        }
        let (trigger_tx, trigger_rx) = mpsc::channel(1);
        Ok(Self {
            cfg: Arc::new(cfg),
            schedules,
            notifier,
            metrics,
            state: Arc::new(Mutex::new(DaemonState::default())),
            trigger_tx,
            trigger_rx: Some(trigger_rx),
        })
    }

    pub fn config(&self) -> &Config {
        &self.cfg
    }

    /// `/metrics` 用。
    pub fn metrics(&self) -> Arc<crate::metrics::Metrics> {
        Arc::clone(&self.metrics)
    }

    /// 給 HTTP server 用的把手。
    pub fn handle(&self) -> DaemonHandle {
        let mut schedules: Vec<(JobKind, Schedule)> = self
            .schedules
            .iter()
            .map(|(k, s)| (*k, s.clone()))
            .collect();
        schedules.sort_by_key(|(k, _)| k.name());
        DaemonHandle {
            cfg: Arc::clone(&self.cfg),
            state: Arc::clone(&self.state),
            trigger_tx: self.trigger_tx.clone(),
            schedules,
        }
    }

    /// 有沒有任何一節寫了 schedule。沒有的話 `run` 只會等觸發（`serve` 可以，`run` 沒意義）。
    pub fn has_schedules(&self) -> bool {
        !self.schedules.is_empty()
    }

    /// 有排程的工作與各自的下一次時間。
    pub fn next_runs(&self, after: OffsetDateTime) -> Vec<(JobKind, Option<OffsetDateTime>)> {
        let mut v: Vec<_> = self
            .schedules
            .iter()
            .map(|(k, s)| (*k, s.next_after(after)))
            .collect();
        v.sort_by_key(|(k, _)| k.name());
        v
    }

    /// 設定裡有的工作各跑一次（backup → forget → prune），不看排程。給 `run --once` 與外部 cron 用。
    /// 跑每一件已排程的工作各一次。`shutdown` 一旦被傳訊（Ctrl-C），剩餘
    /// 的不再開跑——與 `run` 同款「停在本輪工作後」的約定；工作本身做完，
    /// 不中途取消。
    pub async fn run_once(
        &self,
        mut shutdown: Option<watch::Receiver<bool>>,
        mut on_outcome: impl FnMut(&JobOutcome),
    ) -> Vec<JobOutcome> {
        let mut out = Vec::new();
        for kind in [JobKind::Backup, JobKind::Forget, JobKind::Prune] {
            if !configured(&self.cfg, kind) {
                continue;
            }
            if shutdown.as_mut().is_some_and(|rx| *rx.borrow_and_update()) {
                break;
            }
            let o = self.execute(kind).await;
            on_outcome(&o);
            out.push(o);
        }
        out
    }

    /// 常駐：直到 `shutdown` 變成 true（或 sender 消失）。每件工作結束呼叫 `on_outcome`。
    /// 排程到期、或收到觸發，就跑那件工作；跑的時候不收觸發（channel 容量 1 = 最多排一件）。
    /// 沒有排程時只等觸發與關機。
    pub async fn run(
        &mut self,
        mut shutdown: watch::Receiver<bool>,
        mut on_outcome: impl FnMut(&JobOutcome),
    ) -> Result<()> {
        let mut trigger_rx = self
            .trigger_rx
            .take()
            .ok_or_else(|| AppError::Config("daemon already running".to_owned()))?;
        loop {
            let now = OffsetDateTime::now_utc();
            // 下一件：時間最早的；同一時間依 backup → forget → prune
            let mut due: Vec<(OffsetDateTime, JobKind)> = self
                .schedules
                .iter()
                .filter_map(|(k, s)| s.next_after(now).map(|t| (t, *k)))
                .collect();
            due.sort_by_key(|(t, k)| (*t, order(*k)));
            let next = due.first().copied();
            if next.is_none() && !self.schedules.is_empty() {
                return Err(AppError::Config(
                    "no schedule has a next occurrence".to_owned(),
                ));
            }
            let wait = next.map(|(at, kind)| {
                let wait = (at - now).max(time::Duration::ZERO);
                tracing::info!("next job: {} at {at} (in {wait})", kind.name());
                std::time::Duration::try_from(wait).unwrap_or_default()
            });
            if wait.is_none() {
                tracing::debug!("no schedules; waiting for a trigger");
            }
            tokio::select! {
                _ = async {
                    match wait {
                        Some(d) => tokio::time::sleep(d).await,
                        None => std::future::pending::<()>().await,
                    }
                } => {
                    let Some((at, _)) = next else { continue };
                    // 同一秒到期的其他工作也一起跑（依序）
                    let mut batch: Vec<JobKind> = due
                        .iter()
                        .filter(|(t, _)| *t == at)
                        .map(|(_, k)| *k)
                        .collect();
                    batch.sort_by_key(|k| order(*k));
                    for kind in batch {
                        let o = self.execute(kind).await;
                        on_outcome(&o);
                        if *shutdown.borrow() {
                            return Ok(());
                        }
                    }
                }
                changed = shutdown.changed() => {
                    // sender 消失也當關機：沒有人能再叫我們停，繼續轉只會空跑。
                    if changed.is_err() || *shutdown.borrow() {
                        return Ok(());
                    }
                }
                received = trigger_rx.recv() => {
                    // `None` = 所有 sender 都沒了；self 自己就握著一個，所以不會發生，但發生了就結束。
                    let Some(kind) = received else { return Ok(()) };
                    {
                        let mut st = lock_state(&self.state);
                        if let Some(pos) = st.queued.iter().position(|k| *k == kind) {
                            st.queued.remove(pos);
                        }
                    }
                    tracing::info!("{}: triggered", kind.name());
                    let o = self.execute(kind).await;
                    on_outcome(&o);
                    if *shutdown.borrow() {
                        return Ok(());
                    }
                }
            }
        }
    }

    /// 跑一件工作並把結果記到 metrics、jobstate、通知、狀態（running → history）。
    async fn execute(&self, kind: JobKind) -> JobOutcome {
        tracing::info!("{}: starting", kind.name());
        {
            let started = OffsetDateTime::now_utc();
            let mut st = lock_state(&self.state);
            st.running = Some(RunningJob {
                job: kind,
                started: started
                    .format(&time::format_description::well_known::Rfc3339)
                    .unwrap_or_default(),
                started_unix: unix_now(),
                progress: None,
            });
        }
        // 進度只有 backup 有；callback 在 backup 的 task 上同步呼叫，鎖一下就放。
        let progress = if kind == JobKind::Backup {
            let state = Arc::clone(&self.state);
            Some(ProgressCallback(Arc::new(move |p: &BackupProgress| {
                let mut st = lock_state(&state);
                if let Some(r) = st.running.as_mut() {
                    r.progress = Some(p.clone());
                }
            })))
        } else {
            None
        };
        let o = run_job(&self.cfg, kind, progress).await;
        {
            let mut st = lock_state(&self.state);
            st.running = None;
            st.history.insert(0, o.clone());
            st.history.truncate(HISTORY_MAX);
        }
        self.metrics.record(&o);
        // 只有 Success 算「成功」：incomplete（有略過/刪不掉）不更新 last_success，
        // 否則「永遠 incomplete」的 backup 不會觸發「多久沒成功」的告警。
        if o.status == JobStatus::Success {
            if let Some(dir) = &self.cfg.cache_dir {
                jobstate::write_last_success(dir, &self.cfg.repo, o.job, unix_now());
            }
        }
        match &o.error {
            Some(e) => tracing::error!("{}: {}: {e}", kind.name(), o.status.name()),
            None => tracing::info!(
                "{}: {} in {:.1}s",
                kind.name(),
                o.status.name(),
                o.duration_secs
            ),
        }
        if let Some(n) = &self.notifier {
            match n.send(&o).await {
                Ok(true) => tracing::info!("{}: webhook sent", kind.name()),
                Ok(false) => {}
                Err(e) => tracing::warn!("{}: webhook failed: {e}", kind.name()),
            }
        }
        o
    }
}

/// 設定裡有沒有這種工作的那一節（server 的排程表也用）。
pub(crate) fn configured(cfg: &Config, kind: JobKind) -> bool {
    match kind {
        JobKind::Backup => cfg.backup.is_some(),
        JobKind::Forget => cfg.forget.is_some(),
        JobKind::Prune => cfg.prune.is_some(),
    }
}

/// 拿狀態鎖。裡面只有普通資料，別的 task panic 時留下的 poison 不會讓資料壞掉，直接用。
fn lock_state(state: &Mutex<DaemonState>) -> MutexGuard<'_, DaemonState> {
    state.lock().unwrap_or_else(|e| e.into_inner())
}

fn order(k: JobKind) -> u8 {
    match k {
        JobKind::Backup => 0,
        JobKind::Forget => 1,
        JobKind::Prune => 2,
    }
}
