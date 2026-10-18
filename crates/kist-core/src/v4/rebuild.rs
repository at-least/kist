//! 重建（§11.4）：最新 head 的分片遺失或壞掉時，以柵欄寫世代 r ＝ `g_lost + 1`。照 GC 模型
//! （`crates/kist-gcmodel/src/maint.rs` 的 `Rebuild`）實作。不走訪 snapshot、不刪任何東西。
//!
//! - `g_lost` 是列出來的最大世代；`g_r` 是分片讀得全的最大世代（head 0 沒有分片，一定讀得全）。最大世代讀得全
//!   就什麼都不做。`g_r` 到 `g_lost` 的每一代 head 都要讀得了：讀不到就拒絕。
//! - 遺失的世代 `g_r+1..g_lost` 的 head 裡，`w` 的最大減最小 < `a`（`g_r` 到 `g_lost` 的 head 裡最小的 grace，
//!   drain 的舊 grace 也算）時：沿用 `Map(g_r)`，其中 condemned 的記錄標成 held；列表裡有、`Map(g_r)` 裡沒有的
//!   pack 照 P4 收錄。只看 `w`：`T` 在主體遺失改讀 `.r1`、主體被重建時都會變（§11.1）。
//! - 否則：列表裡的每個 pack 照 P4 收錄（讀不了是 unreadable、版本不認得是 unknown），讀得了的記成 condemned
//!   （`at = r`，age 是目前的 age，drain 中是舊的 grace）、held；`Map(g_r)` 的 tombstone 保留。
//! - head r 記下 `hold: { from: g_r+1, lost: g_lost, grace: 範圍內最大的 grace }`，已有 hold 時合併（`from` 取小、
//!   `lost` 與 `grace` 取大）。
//! - 以柵欄寫之前補寫 `g_lost` 缺的 `.r1`（P1），也確認仍在整輪時限內。

use std::collections::BTreeMap;

use kist_format::v4::ids::ObjectId;
use kist_format::v4::index::{shard_of, status, Hold, Record, Shard, Status, SHARDS};
use kist_format::v4::keys::{Gen, Key};

use super::maint::{window_passed, MaintOptions, Session};
use super::prune::{list_packs, newcomer};
use super::repo::Repo;
use crate::{CoreError, Result};

/// 重建的結果。
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct RebuildReport {
    /// 寫出的世代；最大世代的 map 讀得全時是 `None`（什麼都不做）。
    pub generation: Option<Gen>,
    /// 列出來的最大世代（分片遺失的那一代）與分片讀得全的最大世代。
    pub lost: Gen,
    pub readable: Gen,
    /// 沿用 `Map(readable)`（遺失的世代在 `w` 上不到一個 age），不然每個 pack 都記成 held 的 condemned。
    pub kept_map: bool,
    pub records: u64,
    pub held: u64,
    pub newcomers: u64,
    pub warnings: Vec<String>,
}

impl Repo {
    /// 重建（§11.4）。見模組說明。
    pub async fn rebuild(&self, opts: MaintOptions) -> Result<RebuildReport> {
        let mut session = Session::begin(self, opts.clock).await?;
        let newest = session.newest().await?;
        let g_lost = newest.g();
        let mut report = RebuildReport {
            lost: g_lost,
            readable: g_lost,
            ..Default::default()
        };
        if let Some(w) = newest.ahead.clone() {
            report.warnings.push(w);
        }
        let repo = session.repo.clone();
        let mut lost_reason = match repo.load_map(&newest.copy.gen).await {
            Ok(_) => return Ok(report),
            Err(e) => e.to_string(),
        };
        // g_r：由大到小第一個 head 與分片都讀得了的世代
        let mut g_r = None;
        let mut base = BTreeMap::new();
        for g in (0..g_lost).rev() {
            let loaded = async {
                let gen = repo.read_head(g).await?;
                repo.load_map(&gen).await
            }
            .await;
            match loaded {
                Ok(map) => {
                    g_r = Some(g);
                    base = map
                        .into_records()
                        .into_iter()
                        .map(|r| (r.id, r))
                        .collect::<BTreeMap<ObjectId, Record>>();
                    break;
                }
                Err(e) => lost_reason = format!("{lost_reason}; generation {g}: {e}"),
            }
        }
        let Some(g_r) = g_r else {
            return Err(CoreError::Generation(format!(
                "no generation has a readable map: {lost_reason}"
            )));
        };
        report.readable = g_r;
        tracing::warn!(
            "the map of generation {g_lost} is unreadable ({lost_reason}); rebuilding from \
             generation {g_r}"
        );

        // a：g_r 到 g_lost 的 head 裡最小的 grace（drain 的舊 grace 也算）；hold 的 grace 取最大。遺失的世代
        // g_r+1..g_lost 各自的 w 取最小、最大。
        let mut min_age = u64::MAX;
        let mut max_grace = 0;
        let (mut w_lo, mut w_hi) = (i64::MAX, i64::MIN);
        for g in g_r..=g_lost {
            let head = repo.read_head(g).await?.head;
            max_grace = max_grace.max(head.settings.grace);
            min_age = min_age.min(head.settings.grace);
            if let Some(d) = head.drain {
                min_age = min_age.min(d.old_grace);
            }
            if g > g_r {
                w_lo = w_lo.min(head.w);
                w_hi = w_hi.max(head.w);
            }
        }
        let keep = w_hi.saturating_sub(w_lo) < super::maint::secs(min_age);
        report.kept_map = keep;

        let mut head = newest.head().clone();
        let r = g_lost + 1;
        // 案例 B 的 age：drain 還沒結束（兩個時鐘）就用舊的 grace
        let mut age = head.settings.grace;
        if let Some(d) = head.drain {
            let from = session.times(d.from).await?;
            if !window_passed(from, session.now(&newest), d.old_grace) {
                age = d.old_grace;
            }
        }
        let new_hold = Hold {
            from: g_r + 1,
            lost: g_lost,
            grace: max_grace,
        };
        head.hold = Some(match head.hold {
            Some(old) => Hold {
                from: old.from.min(new_hold.from),
                lost: old.lost.max(new_hold.lost),
                grace: old.grace.max(new_hold.grace),
            },
            None => new_hold,
        });

        let mut map: BTreeMap<ObjectId, Record> = BTreeMap::new();
        for (id, rec) in base {
            if keep {
                let mut rec = rec;
                if rec.status.s == status::CONDEMNED {
                    rec.held = true;
                }
                map.insert(id, rec);
            } else if rec.status.s == status::TOMBSTONE {
                map.insert(id, rec);
            }
        }
        let (listed, warnings) = list_packs(repo.backend()).await?;
        report.warnings.extend(warnings);
        for (id, l) in listed {
            if map.contains_key(&id) {
                continue;
            }
            let Some((mut rec, warning)) = newcomer(&repo, id, &l, r).await? else {
                continue;
            };
            report.warnings.extend(warning);
            if !keep && rec.status.s == status::LIVE {
                rec.status = Status {
                    s: status::CONDEMNED,
                    at: r,
                    age,
                };
                rec.held = true;
            }
            map.insert(id, rec);
            report.newcomers += 1;
        }
        report.records = map.len() as u64;
        report.held = map.values().filter(|r| r.held).count() as u64;

        session.repair_r1(&newest).await?;
        // 分片：全部重寫（replicas=1 時先主體、再 .r1）
        let mut by_shard: BTreeMap<u8, Vec<Record>> = BTreeMap::new();
        for rec in map.into_values() {
            by_shard.entry(shard_of(&rec.id)).or_default().push(rec);
        }
        let mut shards = vec![None; SHARDS];
        for (i, recs) in by_shard {
            let sealed = repo.keys().seal_shard(&Shard::new(i, recs).encode()?)?;
            let id = ObjectId::of(&sealed);
            for replica in [false, true] {
                if replica && head.settings.replicas == 0 {
                    continue;
                }
                match repo
                    .backend()
                    .put_if_absent(&Key::Shard { id, replica }.path(), sealed.clone())
                    .await
                {
                    Ok(()) | Err(kist_backend::BackendError::AlreadyExists(_)) => {}
                    Err(e) => return Err(e.into()),
                }
            }
            shards[i as usize] = Some(id);
        }
        head.shards = shards;
        if session.out_of_time(head.settings.grace) {
            return Err(CoreError::Unsafe(format!(
                "this rebuild has run for {}s, past the GC grace ({}s) minus one hour; not writing \
                 generation {r}",
                session.elapsed().as_secs(),
                head.settings.grace
            )));
        }
        let written = session.fence(&newest, head).await?;
        report.generation = Some(written);
        Ok(report)
    }
}
