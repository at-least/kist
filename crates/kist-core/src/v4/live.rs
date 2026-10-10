//! restore、mount、check 的讀取端（§10）：
//! - 列出來的最大世代讀不了（head 的主體與 `.r1`、或它的分片）就退回較舊的世代並警告；backup 與 prune 在這時
//!   停止（B1、P1），讀取端不必：較舊的 map 仍指得到大部分持有者。
//! - 一項的持有者全部失敗（[`CoreError::Unavailable`]）→ 重新載入一次（重新列出世代、載入最大的那一代的 map
//!   與當下列表裡的 delta）再試，仍失敗才報錯。主要的情況是開始得比 prune 早的 restore：prune 把 item 搬進
//!   新的 pack、記在下一代的 map 裡，舊的 pack 過了 grace 之後被刪。
//!
//! 重新載入最多每 [`RELOAD_INTERVAL`] 一次（同 v3 的 restore）：真的壞掉的 repo 不會每一項都重新列一次。別的
//! 工作已經在這一項開始之後重新載入過，就直接用新的再試。

use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use super::read::{Mpacks, Reader, MPACK_MEMORY};
use super::repo::Repo;
use super::state::State;
use crate::{CoreError, Result};

/// 持有者全部失敗時，兩次重新載入之間至少隔這麼久。
pub const RELOAD_INTERVAL: Duration = Duration::from_secs(60);

/// 會重新載入的讀取端。`epoch` 每重新載入一次加一。
pub struct Live {
    repo: Repo,
    current: RwLock<(u64, Reader)>,
    last_reload: tokio::sync::Mutex<Option<Instant>>,
    interval: Duration,
    mpacks: Arc<Mpacks>,
}

impl Repo {
    /// 讀取端的狀態（§10）：列出來的世代由大到小，第一個讀得了的（head 與它的全部分片）；跳過的世代逐一警告，
    /// 原因也回傳（check 回報它們）。
    pub async fn usable_state(&self) -> Result<(State, Vec<String>)> {
        let listing = self.list_heads(None).await?;
        let mut skipped = Vec::new();
        for &g in listing.keys().rev() {
            let loaded = async {
                let gen = self.read_head(g).await?;
                let map = self.load_map(&gen).await?;
                Ok::<_, CoreError>((gen, map))
            }
            .await;
            match loaded {
                Ok((gen, map)) => {
                    let deltas = self.load_deltas(&map).await?;
                    return Ok((State { gen, map, deltas }, skipped));
                }
                Err(e) => {
                    tracing::warn!(
                        "generation {g} is unusable ({e}); falling back to an older generation"
                    );
                    skipped.push(format!("generation {g}: {e}"));
                }
            }
        }
        Err(CoreError::Generation(if skipped.is_empty() {
            "no generation head under maint/gen/: not a v4 repository, or its init did not finish"
                .to_owned()
        } else {
            format!("no usable generation: {}", skipped.join("; "))
        }))
    }

    /// restore、mount、check 的讀取端。
    pub async fn live(&self) -> Result<Live> {
        let (state, _) = self.usable_state().await?;
        Ok(Live::new(self.clone(), state, RELOAD_INTERVAL))
    }
}

impl Live {
    pub fn new(repo: Repo, state: State, interval: Duration) -> Self {
        let mpacks = Arc::new(Mpacks::new(MPACK_MEMORY));
        let reader =
            Reader::new(repo.clone(), Arc::new(state), None).with_mpacks(Arc::clone(&mpacks));
        Self {
            repo,
            current: RwLock::new((0, reader)),
            last_reload: tokio::sync::Mutex::new(None),
            interval,
            mpacks,
        }
    }

    pub fn repo(&self) -> &Repo {
        &self.repo
    }

    /// 現在的讀取端與它的 epoch。
    pub fn current(&self) -> (u64, Reader) {
        let guard = self.current.read().unwrap_or_else(|e| e.into_inner());
        (guard.0, guard.1.clone())
    }

    pub fn reader(&self) -> Reader {
        self.current().1
    }

    /// 以現在的讀取端執行 `op`；它回 [`CoreError::Unavailable`] 時重新載入一次再執行（§10）。重新載入被限流
    /// 擋下（距離上次因失敗而載入不到 interval、也沒有別人在這之後載入過）就回原本的錯誤。
    pub async fn run<T>(&self, op: impl AsyncFn(Reader) -> Result<T>) -> Result<T> {
        let (epoch, reader) = self.current();
        match op(reader).await {
            Err(e @ CoreError::Unavailable { .. }) => {
                if self.reload_after_failure(epoch, &e).await {
                    op(self.reader()).await
                } else {
                    Err(e)
                }
            }
            other => other,
        }
    }

    /// 持有者全部失敗之後的重新載入。`seen` 是失敗的那一次用的 epoch：之後已經有人載入過就不必再載入（回
    /// true，用新的再試）；否則距離上次因失敗而載入不到 interval 就不載入（回 false）。
    async fn reload_after_failure(&self, seen: u64, why: &CoreError) -> bool {
        let mut last = self.last_reload.lock().await;
        if seen != self.current().0 {
            return true;
        }
        if last.is_some_and(|t| t.elapsed() < self.interval) {
            return false;
        }
        *last = Some(Instant::now());
        self.load(&format!("{why}; a prune may have moved it"))
            .await
    }

    /// 不限流的重新載入（mount 看到新的 snapshot 時；限流由呼叫端做）。不影響持有者失敗時的限流。
    pub async fn refresh(&self, why: &str) -> bool {
        let _serial = self.last_reload.lock().await;
        self.load(why).await
    }

    /// 載入列出來的最大世代，換掉現在的讀取端。失敗只警告（回 false）：呼叫端回報它原本的錯誤。
    async fn load(&self, why: &str) -> bool {
        let g = self.reader().state.gen.g;
        tracing::warn!("{why}; reloading generation {g}");
        match self.repo.usable_state().await {
            Ok((state, _)) => {
                tracing::info!("reloaded the generation: {g} → {}", state.gen.g);
                let reader = Reader::new(self.repo.clone(), Arc::new(state), None)
                    .with_mpacks(Arc::clone(&self.mpacks));
                let mut guard = self.current.write().unwrap_or_else(|e| e.into_inner());
                *guard = (guard.0 + 1, reader);
                true
            }
            Err(e) => {
                tracing::warn!("reloading the generation failed: {e}");
                false
            }
        }
    }
}
