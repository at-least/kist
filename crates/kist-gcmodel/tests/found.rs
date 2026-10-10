//! 隨機搜尋（tests/random.rs 的 `mutation_search`）找到的反例，縮到最短後固定下來：同一條交錯（含收尾
//! 檢查），打開突變就違反不變式，原協定就不違反。證明隨機交錯本身找得到這幾類錯誤。交錯依賴模型目前的
//! 行為（包括世界的亂數），模型改了之後若不再重現，用 `extract_minimal_traces` 重新產生。

#![allow(clippy::unwrap_used, clippy::expect_used)]

use kist_gcmodel::*;

/// no_verify_before_drop: seed 0, 6 actions, config default
mod no_verify_before_drop {
    use super::*;
    const SEED: u64 = 0;
    fn actions() -> Vec<Action> {
        vec![
            Action::StartBackup {
                client: 0,
                job: 0,
                variant: 0,
                under: 0,
            },
            Action::StartPrune { host: 0, under: 0 },
            Action::Step { pick: 19754 },
            Action::Step { pick: 0 },
            Action::Mal(Mal::GroundPack {
                items: vec![32522],
                meta: false,
                delta: false,
                genuine: 0,
            }),
            Action::Finish { pick: 4656 },
        ]
    }
    #[test]
    fn the_mutation_violates_an_invariant() {
        let cfg = Config {
            mutations: Mutations {
                no_verify_before_drop: true,
                ..Mutations::default()
            },
            ..Config::default()
        };
        let out = World::run_and_settle(cfg, SEED, &actions());
        assert!(!out.violations.is_empty(), "{}", out.report());
    }
    #[test]
    fn the_protocol_does_not() {
        let out = World::run_and_settle(Config::default(), SEED, &actions());
        assert!(out.violations.is_empty(), "{}", out.report());
    }
}

/// no_fence: seed 3468081662951432620, 43 actions, config default
mod no_fence {
    use super::*;
    const SEED: u64 = 3468081662951432620;
    fn actions() -> Vec<Action> {
        vec![
            Action::StartBackup {
                client: 0,
                job: 182,
                variant: 226,
                under: 0,
            },
            Action::Finish { pick: 0 },
            Action::Mal(Mal::JunkSnap { r1: false }),
            Action::StartBackup {
                client: 0,
                job: 161,
                variant: 145,
                under: 0,
            },
            Action::Finish { pick: 0 },
            Action::StartBackup {
                client: 79,
                job: 0,
                variant: 184,
                under: 0,
            },
            Action::StartBackup {
                client: 164,
                job: 0,
                variant: 4,
                under: 0,
            },
            Action::StartForget {
                pick: 28835,
                force: false,
            },
            Action::Step { pick: 10084 },
            Action::StartSettings {
                host: 3,
                grace_hours: 2,
            },
            Action::Finish { pick: 15934 },
            Action::Step { pick: 48811 },
            Action::Step { pick: 33049 },
            Action::StartPrune { host: 79, under: 0 },
            Action::StartBackup {
                client: 144,
                job: 0,
                variant: 0,
                under: 0,
            },
            Action::Step { pick: 1419 },
            Action::Step { pick: 26201 },
            Action::Finish { pick: 1930 },
            Action::Step { pick: 15383 },
            Action::Crash { pick: 264 },
            Action::Step { pick: 13491 },
            Action::Advance(255600),
            Action::Step { pick: 1473 },
            Action::StartPrune { host: 80, under: 0 },
            Action::Step { pick: 28126 },
            Action::CrashHeadWriter { pick: 0 },
            Action::StartBackup {
                client: 4,
                job: 0,
                variant: 55,
                under: 0,
            },
            Action::Finish { pick: 20477 },
            Action::Step { pick: 582 },
            Action::Step { pick: 14524 },
            Action::Step { pick: 1275 },
            Action::Step { pick: 50542 },
            Action::Finish { pick: 78 },
            Action::Step { pick: 0 },
            Action::Step { pick: 23863 },
            Action::Step { pick: 6289 },
            Action::Step { pick: 8221 },
            Action::StartBackup {
                client: 45,
                job: 0,
                variant: 0,
                under: 0,
            },
            Action::Step { pick: 25735 },
            Action::Finish { pick: 2958 },
            Action::StartBackup {
                client: 2,
                job: 0,
                variant: 0,
                under: 0,
            },
            Action::StartPrune { host: 0, under: 0 },
            Action::Finish { pick: 49372 },
        ]
    }
    #[test]
    fn the_mutation_violates_an_invariant() {
        let cfg = Config {
            mutations: Mutations {
                no_fence: true,
                ..Mutations::default()
            },
            ..Config::default()
        };
        let out = World::run_and_settle(cfg, SEED, &actions());
        assert!(!out.violations.is_empty(), "{}", out.report());
    }
    #[test]
    fn the_protocol_does_not() {
        let out = World::run_and_settle(Config::default(), SEED, &actions());
        assert!(out.violations.is_empty(), "{}", out.report());
    }
}

/// reuse_condemned: seed 7205275596869260574, 124 actions, config default
mod reuse_condemned {
    use super::*;
    const SEED: u64 = 7205275596869260574;
    fn actions() -> Vec<Action> {
        vec![
            Action::StartBackup {
                client: 0,
                job: 30,
                variant: 38,
                under: 0,
            },
            Action::Step { pick: 0 },
            Action::StartPrune {
                host: 235,
                under: 0,
            },
            Action::StartPrune {
                host: 118,
                under: 0,
            },
            Action::Step { pick: 0 },
            Action::Finish { pick: 0 },
            Action::Finish { pick: 0 },
            Action::StartBackup {
                client: 119,
                job: 19,
                variant: 251,
                under: 0,
            },
            Action::Step { pick: 0 },
            Action::Step { pick: 0 },
            Action::StartPrune { host: 23, under: 0 },
            Action::Step { pick: 7022 },
            Action::StartPrune { host: 3, under: 0 },
            Action::Crash { pick: 0 },
            Action::Step { pick: 0 },
            Action::Finish { pick: 0 },
            Action::StartBackup {
                client: 0,
                job: 0,
                variant: 0,
                under: 0,
            },
            Action::Finish { pick: 6854 },
            Action::Step { pick: 0 },
            Action::Step { pick: 0 },
            Action::Step { pick: 0 },
            Action::StartPrune { host: 0, under: 0 },
            Action::Finish { pick: 2005 },
            Action::Step { pick: 0 },
            Action::Step { pick: 0 },
            Action::Step { pick: 0 },
            Action::StartPrune { host: 0, under: 0 },
            Action::Step { pick: 25454 },
            Action::Step { pick: 2372 },
            Action::Advance(259200),
            Action::Step { pick: 16506 },
            Action::Finish { pick: 0 },
            Action::Mal(Mal::UnknownVersionPack),
            Action::StartReader { pick: 0 },
            Action::Step { pick: 0 },
            Action::Step { pick: 0 },
            Action::Step { pick: 0 },
            Action::Step { pick: 0 },
            Action::Step { pick: 0 },
            Action::Step { pick: 0 },
            Action::StartBackup {
                client: 1,
                job: 0,
                variant: 15,
                under: 0,
            },
            Action::StartBackup {
                client: 20,
                job: 0,
                variant: 0,
                under: 0,
            },
            Action::Step { pick: 8633 },
            Action::Step { pick: 49004 },
            Action::Step { pick: 27758 },
            Action::Step { pick: 3323 },
            Action::Step { pick: 15918 },
            Action::Step { pick: 30142 },
            Action::Step { pick: 3636 },
            Action::Step { pick: 25036 },
            Action::StartForget {
                pick: 0,
                force: true,
            },
            Action::Step { pick: 28093 },
            Action::Step { pick: 1044 },
            Action::Step { pick: 0 },
            Action::Advance(0),
            Action::Step { pick: 0 },
            Action::Step { pick: 1639 },
            Action::Step { pick: 128 },
            Action::LoseShard { pick: 92 },
            Action::Step { pick: 44025 },
            Action::StartPrune { host: 78, under: 0 },
            Action::Step { pick: 0 },
            Action::Step { pick: 0 },
            Action::Step { pick: 9966 },
            Action::Step { pick: 0 },
            Action::StartRebuild { host: 13 },
            Action::Step { pick: 0 },
            Action::Step { pick: 0 },
            Action::Step { pick: 1310 },
            Action::Step { pick: 3421 },
            Action::Step { pick: 2089 },
            Action::Step { pick: 46061 },
            Action::LoseShard { pick: 0 },
            Action::StartPrune { host: 9, under: 0 },
            Action::Step { pick: 0 },
            Action::Step { pick: 0 },
            Action::Step { pick: 0 },
            Action::Step { pick: 0 },
            Action::Step { pick: 0 },
            Action::Step { pick: 0 },
            Action::StartForget {
                pick: 0,
                force: false,
            },
            Action::Step { pick: 0 },
            Action::StartBackup {
                client: 113,
                job: 110,
                variant: 48,
                under: 0,
            },
            Action::Step { pick: 0 },
            Action::Step { pick: 0 },
            Action::Step { pick: 0 },
            Action::Step { pick: 0 },
            Action::Step { pick: 0 },
            Action::Step { pick: 0 },
            Action::Step { pick: 0 },
            Action::Step { pick: 0 },
            Action::Step { pick: 0 },
            Action::Step { pick: 0 },
            Action::Step { pick: 0 },
            Action::LoseHeadBody { pick: 0 },
            Action::Advance(43200),
            Action::Advance(216000),
            Action::Step { pick: 0 },
            Action::StartPrune { host: 32, under: 0 },
            Action::StartBackup {
                client: 32,
                job: 188,
                variant: 1,
                under: 0,
            },
            Action::Step { pick: 7333 },
            Action::StartPrune {
                host: 204,
                under: 0,
            },
            Action::StartPrune { host: 79, under: 0 },
            Action::Step { pick: 30133 },
            Action::Step { pick: 0 },
            Action::StartForget {
                pick: 2544,
                force: false,
            },
            Action::Advance(0),
            Action::Step { pick: 31206 },
            Action::StartForget {
                pick: 23234,
                force: false,
            },
            Action::LoseHeadBody { pick: 0 },
            Action::StartBackup {
                client: 3,
                job: 0,
                variant: 0,
                under: 0,
            },
            Action::Crash { pick: 48673 },
            Action::Step { pick: 1 },
            Action::Finish { pick: 512 },
            Action::Finish { pick: 21407 },
            Action::Step { pick: 0 },
            Action::StartBackup {
                client: 19,
                job: 0,
                variant: 0,
                under: 0,
            },
            Action::Step { pick: 5195 },
            Action::Step { pick: 0 },
            Action::Step { pick: 0 },
            Action::Step { pick: 0 },
            Action::Step { pick: 16241 },
            Action::Finish { pick: 26788 },
            Action::Finish { pick: 10776 },
        ]
    }
    #[test]
    fn the_mutation_violates_an_invariant() {
        let cfg = Config {
            mutations: Mutations {
                reuse_condemned: true,
                ..Mutations::default()
            },
            ..Config::default()
        };
        let out = World::run_and_settle(cfg, SEED, &actions());
        assert!(!out.violations.is_empty(), "{}", out.report());
    }
    #[test]
    fn the_protocol_does_not() {
        let out = World::run_and_settle(Config::default(), SEED, &actions());
        assert!(out.violations.is_empty(), "{}", out.report());
    }
}

/// trust_all: seed 0, 26 actions, config default
mod trust_all {
    use super::*;
    const SEED: u64 = 0;
    fn actions() -> Vec<Action> {
        vec![
            Action::Crash { pick: 0 },
            Action::Crash { pick: 0 },
            Action::StartPrune { host: 0, under: 0 },
            Action::StartPrune { host: 0, under: 0 },
            Action::Step { pick: 0 },
            Action::Crash { pick: 0 },
            Action::Step { pick: 0 },
            Action::StartRebuild { host: 235 },
            Action::StartPrune { host: 0, under: 0 },
            Action::Step { pick: 0 },
            Action::StartPrune {
                host: 107,
                under: 0,
            },
            Action::Step { pick: 0 },
            Action::Step { pick: 0 },
            Action::Step { pick: 0 },
            Action::Step { pick: 0 },
            Action::StartPrune {
                host: 122,
                under: 0,
            },
            Action::Step { pick: 0 },
            Action::Step { pick: 0 },
            Action::Step { pick: 0 },
            Action::StartBackup {
                client: 112,
                job: 0,
                variant: 104,
                under: 0,
            },
            Action::Step { pick: 39314 },
            Action::StartBackup {
                client: 107,
                job: 0,
                variant: 0,
                under: 0,
            },
            Action::StartBackup {
                client: 144,
                job: 0,
                variant: 8,
                under: 0,
            },
            Action::Mal(Mal::GroundPack {
                items: vec![10379],
                meta: false,
                delta: true,
                genuine: 0,
            }),
            Action::Finish { pick: 88 },
            Action::Finish { pick: 51175 },
        ]
    }
    #[test]
    fn the_mutation_violates_an_invariant() {
        let cfg = Config {
            mutations: Mutations {
                trust_all: true,
                ..Mutations::default()
            },
            ..Config::default()
        };
        let out = World::run_and_settle(cfg, SEED, &actions());
        assert!(!out.violations.is_empty(), "{}", out.report());
    }
    #[test]
    fn the_protocol_does_not() {
        let out = World::run_and_settle(Config::default(), SEED, &actions());
        assert!(out.violations.is_empty(), "{}", out.report());
    }
}

/// no_head_repair: seed 2316213327837385124, 20 actions, config default
mod no_head_repair {
    use super::*;
    const SEED: u64 = 2316213327837385124;
    fn actions() -> Vec<Action> {
        vec![
            Action::StartBackup {
                client: 27,
                job: 0,
                variant: 0,
                under: 0,
            },
            Action::StartBackup {
                client: 1,
                job: 0,
                variant: 0,
                under: 0,
            },
            Action::Step { pick: 0 },
            Action::StartSettings {
                host: 0,
                grace_hours: 2,
            },
            Action::Step { pick: 0 },
            Action::Advance(0),
            Action::Step { pick: 0 },
            Action::Step { pick: 0 },
            Action::FailHeadR1Put,
            Action::StartForget {
                pick: 0,
                force: false,
            },
            Action::Step { pick: 0 },
            Action::Step { pick: 0 },
            Action::Finish { pick: 12206 },
            Action::Step { pick: 0 },
            Action::Step { pick: 337 },
            Action::Step { pick: 0 },
            Action::StartPrune { host: 0, under: 0 },
            Action::Step { pick: 0 },
            Action::Finish { pick: 6109 },
            Action::LoseLoneHeadBody { pick: 0 },
        ]
    }
    #[test]
    fn the_mutation_violates_an_invariant() {
        let cfg = Config {
            mutations: Mutations {
                no_head_repair: true,
                ..Mutations::default()
            },
            ..Config::default()
        };
        let out = World::run_and_settle(cfg, SEED, &actions());
        assert!(!out.violations.is_empty(), "{}", out.report());
    }
    #[test]
    fn the_protocol_does_not() {
        let out = World::run_and_settle(Config::default(), SEED, &actions());
        assert!(out.violations.is_empty(), "{}", out.report());
    }
}
