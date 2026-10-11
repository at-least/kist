//! v4 的維護角色共用步驟與改 settings（format-v4.md §11.1、§11.3 的 P0、P1、P8 的柵欄，§11.4）。
//!
//! 時間一律走虛擬時鐘（[`VClock`]）：後端的修改時間與命令的牆鐘都從它讀，`advance` 讓兩邊一起前進，兩邊也能
//! 各自偏差。「兩個時鐘都要成立」的規則各有一個只騙過一個時鐘的測試。

mod v4_common;

use std::sync::Arc;

use kist_core::v4::{Repo, SettingsChange};
use kist_core::CoreError;
use kist_format::v4::ids::ObjectId;
use kist_format::v4::index::{Drain, Head, DEFAULT_GRACE};
use kist_format::v4::keys::{Gen, Key};
use v4_common::*;

const HOUR: u64 = 3600;

async fn setup() -> (Arc<VClock>, HookedRepo, Repo) {
    let clock = VClock::new();
    let t = HookedRepo::with_clock(init_options(1), Some(Arc::clone(&clock))).await;
    let repo = t.open_uncached().await;
    (clock, t, repo)
}

fn grace(h: u64) -> SettingsChange {
    SettingsChange {
        grace: Some(h * HOUR),
        ..Default::default()
    }
}

async fn newest(repo: &Repo) -> Gen {
    *repo
        .list_heads(None)
        .await
        .unwrap()
        .keys()
        .next_back()
        .unwrap()
}

async fn head(repo: &Repo, g: Gen) -> Head {
    repo.read_head(g).await.unwrap().head
}

async fn exists(repo: &Repo, key: Key) -> bool {
    repo.backend().exists(&key.path()).await.unwrap()
}

#[tokio::test]
async fn raising_the_grace_takes_effect_at_once_in_a_fenced_generation() {
    let (clock, _t, repo) = setup().await;
    let before = repo.read_head(0).await.unwrap();
    let r = repo
        .change_settings(grace(100), clock.maint())
        .await
        .unwrap();
    assert_eq!(r.generation, Some(1));
    assert_eq!(r.drain, None);
    let h = head(&repo, 1).await;
    assert_eq!(h.settings.grace, 100 * HOUR);
    assert_eq!(h.drain, None);
    assert_eq!(
        h.parent,
        Some(before.hash),
        "parent is the base head's hash"
    );
    assert_eq!(h.shards, before.head.shards, "the map is unchanged");
    assert_eq!(h.w, ns(clock.writer_now()), "w is the writer's clock");
    // 柵欄：主體與 .r1 都在、bytes 相同
    let body = repo
        .backend()
        .get(
            &Key::Head {
                g: 1,
                replica: false,
            }
            .path(),
        )
        .await
        .unwrap();
    let r1 = repo
        .backend()
        .get(
            &Key::Head {
                g: 1,
                replica: true,
            }
            .path(),
        )
        .await
        .unwrap();
    assert_eq!(body, r1);
    assert!(
        repo.backend()
            .list("maint/probe-")
            .await
            .unwrap()
            .is_empty(),
        "P0 deletes its probe"
    );
}

#[tokio::test]
async fn lowering_the_grace_starts_a_drain_from_the_new_generation() {
    let (clock, _t, repo) = setup().await;
    let r = repo
        .change_settings(grace(10), clock.maint())
        .await
        .unwrap();
    let want = Drain {
        from: 1,
        old_grace: DEFAULT_GRACE,
    };
    assert_eq!(r.drain, Some(want));
    assert_eq!(head(&repo, 1).await.drain, Some(want));
}

#[tokio::test]
async fn the_grace_cannot_change_during_a_drain_but_other_settings_can() {
    let (clock, _t, repo) = setup().await;
    repo.change_settings(grace(10), clock.maint())
        .await
        .unwrap();
    clock.advance_hours(71);
    let err = repo
        .change_settings(grace(20), clock.maint())
        .await
        .unwrap_err();
    assert!(
        matches!(&err, CoreError::Usage(m) if m.contains("drain")),
        "{err}"
    );
    assert_eq!(newest(&repo).await, 1, "nothing written");
    // replicas 照改，drain 留著
    let r = repo
        .change_settings(
            SettingsChange {
                replicas: Some(0),
                ..Default::default()
            },
            clock.maint(),
        )
        .await
        .unwrap();
    assert_eq!(r.generation, Some(2));
    let h = head(&repo, 2).await;
    assert_eq!(h.settings.replicas, 0);
    assert_eq!(h.settings.grace, 10 * HOUR);
    assert_eq!(
        h.drain,
        Some(Drain {
            from: 1,
            old_grace: DEFAULT_GRACE
        })
    );
}

fn replicas(r: u8) -> SettingsChange {
    SettingsChange {
        replicas: Some(r),
        ..Default::default()
    }
}

/// 「現在」是最新的 head 的時間與 P0 的讀數取小（§11.1）：調低之後沒有更新的世代，drain 就不會結束；過了一個
/// 舊的 grace 之後寫成的世代（平常是 prune 寫的）讓它結束。
#[tokio::test]
async fn a_drain_ends_once_a_generation_one_old_grace_later_exists() {
    let (clock, _t, repo) = setup().await;
    repo.change_settings(grace(10), clock.maint())
        .await
        .unwrap();
    clock.advance_hours(73);
    let err = repo
        .change_settings(grace(20), clock.maint())
        .await
        .unwrap_err();
    assert!(
        matches!(&err, CoreError::Usage(m) if m.contains("drain")),
        "the newest generation is still the lowering one: {err}"
    );
    let r = repo
        .change_settings(replicas(0), clock.maint())
        .await
        .unwrap();
    assert_eq!(r.generation, Some(2));
    assert!(r.drain.is_some(), "measured at generation 1, not over yet");
    let r = repo
        .change_settings(grace(20), clock.maint())
        .await
        .unwrap();
    assert_eq!(r.generation, Some(3));
    assert_eq!(r.drain, None, "raising after the drain clears it");
    assert_eq!(head(&repo, 3).await.drain, None);
}

/// 同上，但比最新的 head 早了一點：72 小時減一秒還不到。
#[tokio::test]
async fn a_drain_does_not_end_a_second_early() {
    let (clock, _t, repo) = setup().await;
    repo.change_settings(grace(10), clock.maint())
        .await
        .unwrap();
    clock.advance(72 * 3600 - 1);
    repo.change_settings(replicas(0), clock.maint())
        .await
        .unwrap();
    clock.advance(10);
    let err = repo
        .change_settings(grace(20), clock.maint())
        .await
        .unwrap_err();
    assert!(
        matches!(&err, CoreError::Usage(m) if m.contains("drain")),
        "{err}"
    );
}

#[tokio::test]
async fn lowering_again_after_a_drain_starts_a_new_one() {
    let (clock, _t, repo) = setup().await;
    repo.change_settings(grace(10), clock.maint())
        .await
        .unwrap();
    clock.advance_hours(73);
    repo.change_settings(replicas(0), clock.maint())
        .await
        .unwrap();
    let r = repo.change_settings(grace(5), clock.maint()).await.unwrap();
    assert_eq!(
        r.drain,
        Some(Drain {
            from: 3,
            old_grace: 10 * HOUR
        })
    );
}

/// 調低的那一代寫成時後端時鐘慢了 100 小時（`T` 早了）：只看 `T` 的話一小時後寫成的世代已經在 drain 之後，
/// `w` 擋住。
#[tokio::test]
async fn a_slow_backend_clock_at_the_lowering_head_does_not_end_the_drain() {
    let (clock, _t, repo) = setup().await;
    clock.set_backend_skew(-100 * 3600);
    let w = ns(clock.writer_now());
    write_head_with(&repo, false, |h| {
        h.w = w;
        h.settings.grace = 10 * HOUR;
        h.drain = Some(Drain {
            from: 1,
            old_grace: DEFAULT_GRACE,
        });
    })
    .await;
    clock.set_backend_skew(0);
    clock.advance_hours(1);
    repo.change_settings(replicas(0), clock.maint())
        .await
        .unwrap();
    let err = repo
        .change_settings(grace(20), clock.maint())
        .await
        .unwrap_err();
    assert!(
        matches!(&err, CoreError::Usage(m) if m.contains("drain")),
        "{err}"
    );
}

/// 調低的那一代寫成時寫入者的時鐘慢了 100 小時（`w` 早了）：`T` 擋住。
#[tokio::test]
async fn a_slow_writer_clock_at_the_lowering_head_does_not_end_the_drain() {
    let (clock, _t, repo) = setup().await;
    let w = ns(clock.writer_now()) - 100 * 3600 * 1_000_000_000;
    write_head_with(&repo, false, |h| {
        h.w = w;
        h.settings.grace = 10 * HOUR;
        h.drain = Some(Drain {
            from: 1,
            old_grace: DEFAULT_GRACE,
        });
    })
    .await;
    clock.advance_hours(1);
    repo.change_settings(replicas(0), clock.maint())
        .await
        .unwrap();
    let err = repo
        .change_settings(grace(20), clock.maint())
        .await
        .unwrap_err();
    assert!(
        matches!(&err, CoreError::Usage(m) if m.contains("drain")),
        "{err}"
    );
}

/// 第二輪外部審查 C1–C3：最新的 head 是兩個時鐘都快了 45 分鐘的主機寫的（不到 `M`，P1 不調整它的 `T`）。
/// 「現在」只用它的值的話 drain 在真實的 71.6 小時就結束了；與這一輪 P0 的讀數取小之後沒有。
#[tokio::test]
async fn a_newest_head_written_with_fast_clocks_does_not_end_a_drain_early() {
    let (clock, _t, repo) = setup().await;
    repo.change_settings(grace(10), clock.maint())
        .await
        .unwrap();
    clock.advance(71 * 3600 + 30 * 60);
    clock.set_backend_skew(45 * 60);
    let w = ns(clock.writer_now()) + 45 * 60 * 1_000_000_000;
    write_head_with(&repo, false, |h| {
        h.w = w;
        h.settings.replicas = 0;
    })
    .await;
    clock.set_backend_skew(0);
    clock.advance(5 * 60);
    let err = repo
        .change_settings(grace(20), clock.maint())
        .await
        .unwrap_err();
    assert!(
        matches!(&err, CoreError::Usage(m) if m.contains("drain")),
        "{err}"
    );
}

#[tokio::test]
async fn no_change_writes_no_generation() {
    let (clock, _t, repo) = setup().await;
    let r = repo
        .change_settings(grace(72), clock.maint())
        .await
        .unwrap();
    assert_eq!(r.generation, None);
    assert_eq!(newest(&repo).await, 0);
}

#[tokio::test]
async fn settings_out_of_range_are_refused() {
    let (clock, _t, repo) = setup().await;
    for change in [
        grace(1),
        SettingsChange {
            replicas: Some(2),
            ..Default::default()
        },
        SettingsChange {
            pack_target: Some(1024),
            ..Default::default()
        },
        SettingsChange {
            mpack_target: Some(5 * 1024 * 1024 * 1024),
            ..Default::default()
        },
    ] {
        let err = repo
            .change_settings(change, clock.maint())
            .await
            .unwrap_err();
        assert!(matches!(err, CoreError::Usage(_)), "{change:?}: {err}");
    }
    assert_eq!(newest(&repo).await, 0);
}

/// P0：後端時鐘與本機牆鐘相差超過 5 分鐘就拒絕，什麼都不寫。
#[tokio::test]
async fn p0_refuses_a_clock_difference_over_five_minutes() {
    let (clock, _t, repo) = setup().await;
    clock.set_writer_skew(10 * 60);
    let err = repo
        .change_settings(grace(100), clock.maint())
        .await
        .unwrap_err();
    assert!(err.to_string().contains("probe"), "{err}");
    assert_eq!(newest(&repo).await, 0);
    assert!(repo
        .backend()
        .list("maint/probe-")
        .await
        .unwrap()
        .is_empty());
}

/// P1：最大世代只有主體（寫入者在兩次 PUT 之間當掉）→ 先補寫 `.r1`（主體的 bytes），再往下做。
#[tokio::test]
async fn p1_writes_the_missing_r1_of_the_newest_head_before_building_on_it() {
    let (clock, _t, repo) = setup().await;
    let w = ns(clock.writer_now());
    write_head_with(&repo, true, |h| h.w = w).await;
    assert!(
        !exists(
            &repo,
            Key::Head {
                g: 1,
                replica: true
            }
        )
        .await
    );
    repo.change_settings(grace(100), clock.maint())
        .await
        .unwrap();
    let body = repo
        .backend()
        .get(
            &Key::Head {
                g: 1,
                replica: false,
            }
            .path(),
        )
        .await
        .unwrap();
    let r1 = repo
        .backend()
        .get(
            &Key::Head {
                g: 1,
                replica: true,
            }
            .path(),
        )
        .await
        .unwrap();
    assert_eq!(body, r1);
    assert_eq!(newest(&repo).await, 2);
}

/// P1 的補寫失敗就停止：不在只有一份的 head 上建下一代。
#[tokio::test]
async fn a_failed_r1_repair_stops_before_the_next_generation() {
    let (clock, t, repo) = setup().await;
    let w = ns(clock.writer_now());
    write_head_with(&repo, true, |h| h.w = w).await;
    t.store.fail.lock().unwrap().push(
        Key::Head {
            g: 1,
            replica: true,
        }
        .path(),
    );
    let err = repo
        .change_settings(grace(100), clock.maint())
        .await
        .unwrap_err();
    assert!(err.to_string().contains("one copy"), "{err}");
    assert_eq!(newest(&repo).await, 1);
}

/// 柵欄先看 `.r1`：下一代的 `.r1` 已經在（主體遺失、`.r1` 還在的世代）就是輸了，不寫主體。`.r1` 在這一輪
/// P1 讀完 head 之後才出現（別的維護命令在這時寫成）。
#[tokio::test]
async fn the_fence_is_lost_when_the_next_generation_has_an_r1() {
    let (clock, t, repo) = setup().await;
    t.store.after_get.lock().unwrap().push((
        Key::Head {
            g: 0,
            replica: false,
        }
        .path(),
        Key::Head {
            g: 1,
            replica: true,
        }
        .path(),
        b"x".to_vec(),
    ));
    let err = repo
        .change_settings(grace(100), clock.maint())
        .await
        .unwrap_err();
    assert!(matches!(err, CoreError::Superseded(_)), "{err}");
    assert!(
        !exists(
            &repo,
            Key::Head {
                g: 1,
                replica: false
            }
        )
        .await
    );
}

#[tokio::test]
async fn the_fence_is_lost_when_the_next_body_already_exists() {
    let (clock, t, repo) = setup().await;
    // 另一個維護命令在這一輪的 P1 之後、柵欄之前寫了 head 1：用「已存在」模擬
    t.store.echo.lock().unwrap().push(
        Key::Head {
            g: 1,
            replica: false,
        }
        .path(),
    );
    let err = repo
        .change_settings(grace(100), clock.maint())
        .await
        .unwrap_err();
    assert!(matches!(err, CoreError::Superseded(_)), "{err}");
    assert!(
        !exists(
            &repo,
            Key::Head {
                g: 1,
                replica: true
            }
        )
        .await,
        "the loser writes no .r1"
    );
}

/// 主體寫成、`.r1` 寫入失敗：回錯誤，下一次維護補寫 `.r1`（P1）。
#[tokio::test]
async fn a_failed_head_r1_write_is_an_error_and_the_next_run_repairs_it() {
    let (clock, t, repo) = setup().await;
    t.store.fail.lock().unwrap().push(
        Key::Head {
            g: 1,
            replica: true,
        }
        .path(),
    );
    let err = repo
        .change_settings(grace(100), clock.maint())
        .await
        .unwrap_err();
    assert!(err.to_string().contains(".r1 is not"), "{err}");
    assert!(
        exists(
            &repo,
            Key::Head {
                g: 1,
                replica: false
            }
        )
        .await
    );
    assert!(
        !exists(
            &repo,
            Key::Head {
                g: 1,
                replica: true
            }
        )
        .await
    );
    repo.change_settings(grace(120), clock.maint())
        .await
        .unwrap();
    assert!(
        exists(
            &repo,
            Key::Head {
                g: 1,
                replica: true
            }
        )
        .await
    );
    assert_eq!(head(&repo, 2).await.settings.grace, 120 * HOUR);
}

/// 同一台主機一次一個維護命令（§11.3）。
#[tokio::test]
async fn a_second_maintenance_command_on_the_same_host_is_refused() {
    let (clock, _t, repo) = setup().await;
    let name = repo
        .keys()
        .cache_dir_name(&repo.backend().location().to_string());
    let path = std::env::temp_dir().join(format!("kist-maint-{name}.lock"));
    let held = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .unwrap();
    held.try_lock().unwrap();
    let err = repo
        .change_settings(grace(100), clock.maint())
        .await
        .unwrap_err();
    assert!(
        matches!(&err, CoreError::Usage(m) if m.contains("another maintenance")),
        "{err}"
    );
    drop(held);
    repo.change_settings(grace(100), clock.maint())
        .await
        .unwrap();
}

/// 改 settings 的新世代：shards 與 bad、hold 照抄 base head。
#[tokio::test]
async fn a_settings_generation_keeps_the_rest_of_the_head() {
    let (clock, _t, repo) = setup().await;
    let shard = ObjectId::from_bytes([7; 32]);
    let w = ns(clock.writer_now());
    write_head_with(&repo, false, |h| {
        h.w = w;
        h.shards[3] = Some(shard);
        h.bad = vec![kist_format::v4::index::Bad {
            key: format!("snapshots/{}/20260101T000000000000000Z", client(1)),
            first: 1,
        }];
    })
    .await;
    repo.change_settings(
        SettingsChange {
            pack_target: Some(128 * 1024),
            ..Default::default()
        },
        clock.maint(),
    )
    .await
    .unwrap();
    let h = head(&repo, 2).await;
    assert_eq!(h.settings.pack_target, 128 * 1024);
    assert_eq!(h.shards[3], Some(shard));
    assert_eq!(h.bad.len(), 1);
}
