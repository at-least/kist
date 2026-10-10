//! 情境測試的共用工具。

#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use kist_gcmodel::*;
use proptest::collection::vec;
use proptest::prelude::*;

pub fn quiet() -> Config {
    Config {
        repack: RepackPolicy::Never,
        eps_max: 0,
        under_max: 0,
        ..Config::default()
    }
}

pub fn assert_ok(w: &World) {
    let v = w.check();
    assert!(v.is_empty(), "{v:#?}\nlog:\n{}", w.core.log.join("\n"));
}

/// 跑完一次 backup，回傳 commit 的 snapshot。
pub fn backup(w: &mut World, client: ClientId, job: u8, variant: u8) -> SnapKey {
    let id = w
        .start_backup(client, job, variant, 0)
        .expect("client busy");
    let r = w.finish(id);
    assert_eq!(r, Some(StepResult::Done), "log:\n{}", w.core.log.join("\n"));
    w.newest(client, job).expect("no snapshot")
}

pub fn prune(w: &mut World, host: u8) -> StepResult {
    let id = w.start_prune(host, 0).expect("host busy");
    w.finish(id).expect("prune vanished")
}

pub fn forget(w: &mut World, key: SnapKey) {
    let id = w.start_forget_key(key);
    assert_eq!(w.finish(id), Some(StepResult::Done));
}

/// 每次推進 `hours` 再跑一次 prune，共 `rounds` 輪。
pub fn age_and_prune(w: &mut World, rounds: usize, hours: Time) {
    for _ in 0..rounds {
        w.advance(hours * HOUR);
        let r = prune(w, 0);
        assert!(
            matches!(r, StepResult::Done),
            "prune: {r:?}\nlog:\n{}",
            w.core.log.join("\n")
        );
        assert_ok(w);
    }
}

pub fn pack_count(w: &World) -> usize {
    w.core
        .backend
        .keys()
        .filter(|k| matches!(k, Key::Pack { .. }))
        .count()
}

// ---- 隨機交錯的 strategy（random.rs 與診斷共用） ----

pub fn advance() -> impl Strategy<Value = Action> {
    prop_oneof![
        10 => (0i64..600).prop_map(Action::Advance),
        4 => (1i64..12).prop_map(|h| Action::Advance(h * HOUR)),
        2 => (12i64..96).prop_map(|h| Action::Advance(h * HOUR)),
    ]
}

pub fn mal() -> impl Strategy<Value = Mal> {
    prop_oneof![
        3 => (vec(any::<u16>(), 1..4), any::<bool>(), any::<bool>())
            .prop_map(|(items, meta, delta)| Mal::FalsePack { items, meta, delta }),
        2 => (any::<u16>(), vec(any::<u16>(), 1..4))
            .prop_map(|(pack_pick, items)| Mal::ForeignDelta { pack_pick, items }),
        1 => any::<bool>().prop_map(|r1| Mal::JunkSnap { r1 }),
        1 => Just(Mal::JunkPack),
        1 => any::<u16>().prop_map(|item| Mal::ForgedSnap { item }),
        1 => Just(Mal::UnknownVersionPack),
        3 => (vec(any::<u16>(), 1..4), any::<bool>(), any::<bool>(), any::<u8>())
            .prop_map(|(items, meta, delta, genuine)| Mal::GroundPack { items, meta, delta, genuine }),
        1 => any::<u16>().prop_map(|item| Mal::FutureSnap { item }),
    ]
}

/// 共同的 action；`clock` 決定時鐘故障的種類。
pub fn action(clock: Clock) -> BoxedStrategy<Action> {
    let skew: BoxedStrategy<Action> = match clock {
        // 後端時鐘誠實，prune 主機的時鐘可以任意出錯（包括 P0 之後才跳）。
        // 一半以上的時鐘動作把偏差歸零，否則 P0 的拒絕會蓋過其他一切（外部審查的量測）。
        Clock::Hosts => (
            any::<u8>(),
            prop_oneof![2 => Just(0i32), 2 => -600i32..600, 1 => -720_000i32..720_000],
        )
            .prop_map(|(host, skew)| Action::SetSkew { host, skew })
            .boxed(),
        // prune 主機的時鐘誠實，後端時鐘可以出錯，偶爾差到 100 小時。
        Clock::Backend => prop_oneof![
            3 => Just(0i32),
            2 => -600i32..600,
            1 => -7_200i32..7_200,
            1 => -360_000i32..360_000,
        ]
        .prop_map(Action::BackendSkew)
        .boxed(),
    };
    prop_oneof![
        120 => any::<u16>().prop_map(|pick| Action::Step { pick }),
        24 => any::<u16>().prop_map(|pick| Action::Finish { pick }),
        48 => advance(),
        24 => (any::<u8>(), any::<u8>(), any::<u8>(), any::<u16>())
            .prop_map(|(client, job, variant, under)| Action::StartBackup { client, job, variant, under }),
        18 => (any::<u8>(), any::<u16>()).prop_map(|(host, under)| Action::StartPrune { host, under }),
        9 => (any::<u16>(), prop::bool::weighted(0.2)).prop_map(|(pick, force)| Action::StartForget { pick, force }),
        6 => any::<u16>().prop_map(|pick| Action::StartReader { pick }),
        3 => (any::<u8>(), 2u8..120).prop_map(|(host, grace_hours)| Action::StartSettings { host, grace_hours }),
        6 => any::<u8>().prop_map(|host| Action::StartRebuild { host }),
        3 => any::<u16>().prop_map(|pick| Action::Crash { pick }),
        1 => any::<u8>().prop_map(|pick| Action::LoseShard { pick }),
        1 => any::<u16>().prop_map(|pick| Action::LoseHeadBody { pick }),
        4 => any::<u16>().prop_map(|pick| Action::CrashHeadWriter { pick }),
        6 => any::<u16>().prop_map(|pick| Action::LoseLoneHeadBody { pick }),
        6 => skew,
        3 => Just(Action::FailReadback),
        2 => Just(Action::FailHeadR1Put),
        6 => mal().prop_map(Action::Mal),
    ]
    .boxed()
}

#[derive(Clone, Copy, Debug)]
pub enum Clock {
    Hosts,
    Backend,
}

pub fn input(clock: Clock) -> impl Strategy<Value = (u64, Vec<Action>)> {
    (any::<u64>(), vec(action(clock), 1..400))
}
