//! 刻意競態（PLAN M3 驗收）：proptest 產生 backup（拆成 prepare / commit）、forget、prune、
//! 時鐘推進的隨機交錯，兩台 client 共用一個 repo。每一步之後 repo 都必須一致：
//! - `check --read-data` 沒有錯誤（index 指到的 pack 都在、每個 snapshot 引用的 chunk 都讀得到）；
//! - commit 只能成功或**安全失敗**（PackMissing / TreeMarked，且沒寫 snapshot）；
//! - 最後每個 snapshot 都能還原成當初備份的內容。
//!
//! 「永遠不會刪到活的 chunk」就是由這三條合起來證明的。
//!
//! 時鐘是注入的（從真實時間 + 1 年起算，只往前推）。本機後端的物件「修改時間」是檔案 mtime，
//! 每一步之後把剛寫出的檔案（mtime 還是真實時間的）蓋成目前的時鐘，讓 prune 看到的年齡與
//! snapshot 的時間在同一條時間軸上。

mod common;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use common::*;
use kist_core::{
    BackupOptions, CheckOptions, CoreError, PreparedBackup, PruneOptions, PrunePlan, Repository,
};
use proptest::prelude::*;
use time::{Duration, OffsetDateTime};

const H: std::time::Duration = std::time::Duration::from_secs(3600);
const GRACE_HOURS: i64 = 72;

#[derive(Debug, Clone)]
enum Op {
    /// 一次做完的 backup。
    Backup {
        client: u8,
        variant: u8,
    },
    /// 只做到 index 為止，snapshot 留到 Commit。
    Prepare {
        client: u8,
        variant: u8,
    },
    Commit {
        client: u8,
    },
    /// 刪掉第 n 個（mod 現有數量）snapshot。
    Forget {
        nth: u8,
    },
    Prune,
    /// prune 拆成兩半：先讀與決定，之後才寫與刪——中間可以插 backup 的 commit（1-A 的視窗）。
    PrunePlan,
    PruneExecute,
    /// rebuild-index 也是 index 的寫入者，插在 prune 中間會製造幽靈 pack。
    RebuildIndex,
    Advance {
        hours: u8,
    },
}

fn op_strategy() -> impl Strategy<Value = Op> {
    prop_oneof![
        3 => (0u8..2, 1u8..16).prop_map(|(client, variant)| Op::Backup { client, variant }),
        2 => (0u8..2, 1u8..16).prop_map(|(client, variant)| Op::Prepare { client, variant }),
        2 => (0u8..2).prop_map(|client| Op::Commit { client }),
        2 => (0u8..8).prop_map(|nth| Op::Forget { nth }),
        2 => Just(Op::Prune),
        2 => Just(Op::PrunePlan),
        2 => Just(Op::PruneExecute),
        1 => Just(Op::RebuildIndex),
        3 => (1u8..120).prop_map(|hours| Op::Advance { hours }),
    ]
}

/// variant 的每個 bit 對應一個檔案；內容固定（seed）所以兩台 client 的資料能去重。
fn write_variant(dir: &Path, variant: u8) {
    std::fs::create_dir_all(dir.join("sub")).unwrap();
    for bit in 0..4u8 {
        let path = if bit % 2 == 0 {
            dir.join(format!("f{bit}.bin"))
        } else {
            dir.join("sub").join(format!("f{bit}.bin"))
        };
        if variant & (1 << bit) != 0 {
            if !path.exists() {
                std::fs::write(&path, random_bytes(100 + u64::from(bit), 200 * 1024)).unwrap();
            }
        } else {
            let _ = std::fs::remove_file(&path);
        }
    }
}

fn expected_files(variant: u8) -> BTreeMap<String, Vec<u8>> {
    (0..4u8)
        .filter(|bit| variant & (1 << bit) != 0)
        .map(|bit| {
            let name = if bit % 2 == 0 {
                format!("f{bit}.bin")
            } else {
                format!("sub/f{bit}.bin")
            };
            (name, random_bytes(100 + u64::from(bit), 200 * 1024))
        })
        .collect()
}

fn opts(client: u8, now: OffsetDateTime) -> BackupOptions {
    BackupOptions {
        client_id: [client + 1; 16],
        hostname: format!("host{client}"),
        username: "tester".to_owned(),
        now: Some(now),
        gc_grace: 72 * H,
    }
}

struct World {
    t: TestRepo,
    repo: Repository,
    clock: OffsetDateTime,
    srcs: [PathBuf; 2],
    /// 進行中的 backup 與它備份的 variant。
    pending: [Option<(PreparedBackup, u8)>; 2],
    pending_prune: Option<PrunePlan>,
    /// 已寫出的 snapshot → 它備份的 (client, variant)。
    snapshots: BTreeMap<String, (u8, u8)>,
    log: Vec<String>,
    /// mtime 早於這個時間的檔案是這一步剛寫出的（真實時間），要蓋成時鐘。
    real_horizon: std::time::SystemTime,
}

impl World {
    async fn new() -> Self {
        let t = TestRepo::new().await;
        let repo = t.open().await;
        let srcs = [t.dir.path().join("src-a"), t.dir.path().join("src-b")];
        Self {
            t,
            repo,
            clock: OffsetDateTime::now_utc() + Duration::days(365),
            srcs,
            pending: [None, None],
            pending_prune: None,
            snapshots: BTreeMap::new(),
            log: Vec::new(),
            real_horizon: std::time::SystemTime::now()
                + std::time::Duration::from_secs(180 * 86_400),
        }
    }

    /// 把這一步剛寫出的物件（mtime 仍是真實時間）蓋成目前的時鐘。
    fn stamp_new_objects(&self) {
        let clock = std::time::SystemTime::UNIX_EPOCH
            + std::time::Duration::from_nanos(self.clock.unix_timestamp_nanos() as u64);
        for path in walk_files(&self.t.repo_path()) {
            let meta = std::fs::symlink_metadata(&path).unwrap();
            if meta.is_file() && meta.modified().unwrap() < self.real_horizon {
                filetime::set_file_mtime(&path, filetime::FileTime::from_system_time(clock))
                    .unwrap();
            }
        }
    }

    async fn step(&mut self, op: &Op) {
        match op {
            Op::Backup { client, variant } => {
                let c = *client as usize;
                if self.pending[c].is_some() {
                    return; // 同一台 client 一次只跑一個 backup（CLI 的鎖）
                }
                write_variant(&self.srcs[c], *variant);
                let r = self
                    .repo
                    .backup(
                        std::slice::from_ref(&self.srcs[c]),
                        opts(*client, self.clock),
                    )
                    .await;
                self.record_commit(*client, *variant, r);
            }
            Op::Prepare { client, variant } => {
                let c = *client as usize;
                if self.pending[c].is_some() {
                    return; // 同一台 client 一次只跑一個 backup（CLI 的鎖）
                }
                write_variant(&self.srcs[c], *variant);
                let p = self
                    .repo
                    .backup_prepare(
                        std::slice::from_ref(&self.srcs[c]),
                        opts(*client, self.clock),
                    )
                    .await
                    .unwrap();
                self.pending[c] = Some((p, *variant));
                self.log
                    .push(format!("prepare client {client} variant {variant}"));
            }
            Op::Commit { client } => {
                let c = *client as usize;
                if let Some((p, variant)) = self.pending[c].take() {
                    let r = p.commit_at(self.clock).await;
                    self.record_commit(*client, variant, r);
                }
            }
            Op::Forget { nth } => {
                let keys: Vec<String> = self.snapshots.keys().cloned().collect();
                if keys.is_empty() {
                    return;
                }
                let key = keys[*nth as usize % keys.len()].clone();
                self.repo
                    .forget(kist_core::ForgetOptions {
                        snapshots: vec![key.clone()],
                        policy: Default::default(),
                        dry_run: false,
                    })
                    .await
                    .unwrap();
                self.snapshots.remove(&key);
                self.log.push(format!("forget {key}"));
            }
            Op::Prune => {
                let r = self
                    .repo
                    .prune(PruneOptions {
                        grace: H * (GRACE_HOURS as u32),
                        inactive_after: 30 * 24 * H,
                        repack_below_percent: 50,
                        dry_run: false,
                        now: Some(self.clock),
                    })
                    .await
                    .unwrap();
                assert!(r.skipped.is_empty(), "{r:?}");
                self.log.push(format!("prune → {r:?}"));
            }
            Op::PrunePlan => {
                if self.pending_prune.is_some() {
                    return;
                }
                let plan = self
                    .repo
                    .prune_plan(PruneOptions {
                        grace: H * (GRACE_HOURS as u32),
                        inactive_after: 30 * 24 * H,
                        repack_below_percent: 50,
                        dry_run: false,
                        now: Some(self.clock),
                    })
                    .await
                    .unwrap();
                self.log.push(format!("prune plan → {:?}", plan.report()));
                self.pending_prune = Some(plan);
            }
            Op::RebuildIndex => {
                let r = self.repo.rebuild_index().await.unwrap();
                self.log.push(format!("rebuild-index → {r:?}"));
            }
            Op::PruneExecute => {
                if let Some(plan) = self.pending_prune.take() {
                    let r = plan.execute().await.unwrap();
                    assert!(r.skipped.is_empty(), "{r:?}");
                    self.log.push(format!("prune execute → {r:?}"));
                }
            }
            Op::Advance { hours } => {
                self.clock += Duration::hours(i64::from(*hours));
                self.log.push(format!("advance {hours}h"));
            }
        }
        self.stamp_new_objects();
        // 每一步至少過一秒：同一個注入奈秒連續 backup 會撞 snapshot key（現實不會）
        self.clock += Duration::seconds(1);
    }

    fn record_commit(
        &mut self,
        client: u8,
        variant: u8,
        r: kist_core::Result<kist_core::BackupSummary>,
    ) {
        match r {
            Ok(s) => {
                self.snapshots
                    .insert(s.snapshot_key.clone(), (client, variant));
                self.log.push(format!(
                    "commit client {client} variant {variant} → {} (new chunks {})",
                    s.snapshot_key, s.stats.chunks_new
                ));
            }
            Err(e) => {
                assert!(
                    matches!(
                        e,
                        CoreError::PackMissing { .. }
                            | CoreError::TreeMarked(_)
                            | CoreError::BackupTooLong { .. }
                    ),
                    "commit failed unsafely: {e}\nlog:\n{}",
                    self.log.join("\n")
                );
                self.log
                    .push(format!("commit client {client} failed safely: {e}"));
            }
        }
    }

    /// 每一步之後的不變量。
    async fn check_invariants(&self) {
        let fresh = self.t.open().await;
        let report = fresh.check(CheckOptions { read_data: true }).await.unwrap();
        assert!(
            report.errors.is_empty(),
            "repo inconsistent: {:?}\nlog:\n{}",
            report.errors,
            self.log.join("\n")
        );
        let mut keys = fresh.list_snapshot_keys().await.unwrap();
        keys.sort();
        let mine: Vec<String> = self.snapshots.keys().cloned().collect();
        assert_eq!(keys, mine, "snapshot 清單對不上");
    }

    /// 最後：每個 snapshot 還原出來的內容必須等於當初的 variant。
    async fn check_restores(&self) {
        let fresh = self.t.open().await;
        for (key, (client, variant)) in &self.snapshots {
            let out = self.t.dir.path().join("out");
            let _ = std::fs::remove_dir_all(&out);
            let r = fresh.restore(key, &out, Default::default()).await.unwrap();
            assert!(
                r.errors.is_empty(),
                "{key}: {:?}\nlog:\n{}",
                r.errors,
                self.log.join("\n")
            );
            let root = out.join(self.srcs[*client as usize].strip_prefix("/").unwrap());
            let mut got = BTreeMap::new();
            for (rel, content) in snapshot_dir(&root) {
                if content != b"<dir>" {
                    got.insert(rel.to_string_lossy().into_owned(), content);
                }
            }
            assert_eq!(
                got,
                expected_files(*variant),
                "{key} 還原內容不對\nlog:\n{}",
                self.log.join("\n")
            );
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 24,
        max_shrink_iters: 200,
        failure_persistence: None,
        .. ProptestConfig::default()
    })]

    #[test]
    fn interleaved_backup_forget_prune_never_loses_live_data(ops in prop::collection::vec(op_strategy(), 4..16)) {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let mut w = World::new().await;
            for op in &ops {
                w.step(op).await;
                w.check_invariants().await;
            }
            // 把還沒 commit 的都 commit 掉（可能安全失敗）、還沒執行的 prune 執行掉，再跑一次 prune 與收尾檢查
            for client in 0..2u8 {
                w.step(&Op::Commit { client }).await;
                w.check_invariants().await;
            }
            w.step(&Op::PruneExecute).await;
            w.check_invariants().await;
            w.step(&Op::Prune).await;
            w.check_invariants().await;
            w.check_restores().await;
        });
    }
}
