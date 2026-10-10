//! 隨機搜尋（tests/random.rs 的 `mutation_search`）找到的反例，縮到最短後固定下來：同一條交錯（含收尾
//! 檢查），打開突變就違反不變式，原協定就不違反。證明隨機交錯本身找得到這幾類錯誤。交錯依賴模型目前的
//! 行為（包括世界的亂數），模型改了之後若不再重現，用 `extract_minimal_traces` 重新產生。

#![allow(clippy::unwrap_used, clippy::expect_used)]

use kist_gcmodel::*;

/// no_verify_before_drop: seed 1582756170227120508, 10 actions, config default
mod no_verify_before_drop {
    use super::*;
    const SEED: u64 = 1582756170227120508;
    fn actions() -> Vec<Action> {
        vec![
            Action::Step { pick: 0 },
            Action::StartBackup {
                client: 0,
                job: 0,
                variant: 0,
                under: 0,
            },
            Action::Step { pick: 0 },
            Action::Mal(Mal::GroundPack {
                items: vec![11319],
                meta: false,
                delta: false,
                genuine: 0,
            }),
            Action::Finish { pick: 0 },
            Action::Step { pick: 0 },
            Action::Step { pick: 0 },
            Action::Step { pick: 0 },
            Action::StartBackup {
                client: 0,
                job: 0,
                variant: 1,
                under: 0,
            },
            Action::Finish { pick: 0 },
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

/// no_fence: seed 0, 25 actions, config default
mod no_fence {
    use super::*;
    const SEED: u64 = 0;
    fn actions() -> Vec<Action> {
        vec![
            Action::StartBackup {
                client: 1,
                job: 0,
                variant: 0,
                under: 0,
            },
            Action::StartBackup {
                client: 32,
                job: 0,
                variant: 0,
                under: 0,
            },
            Action::StartPrune { host: 3, under: 0 },
            Action::Finish { pick: 10224 },
            Action::StartBackup {
                client: 3,
                job: 0,
                variant: 0,
                under: 0,
            },
            Action::Finish { pick: 684 },
            Action::StartBackup {
                client: 11,
                job: 0,
                variant: 0,
                under: 0,
            },
            Action::Finish { pick: 39583 },
            Action::Step { pick: 0 },
            Action::Step { pick: 19879 },
            Action::StartPrune { host: 7, under: 0 },
            Action::StartBackup {
                client: 42,
                job: 0,
                variant: 0,
                under: 0,
            },
            Action::Step { pick: 0 },
            Action::StartBackup {
                client: 109,
                job: 0,
                variant: 0,
                under: 0,
            },
            Action::Finish { pick: 47001 },
            Action::Step { pick: 0 },
            Action::Finish { pick: 20149 },
            Action::Advance(0),
            Action::Step { pick: 0 },
            Action::Step { pick: 16 },
            Action::Step { pick: 1780 },
            Action::StartBackup {
                client: 80,
                job: 0,
                variant: 1,
                under: 0,
            },
            Action::LoseHeadBody { pick: 0 },
            Action::Finish { pick: 53387 },
            Action::Finish { pick: 16206 },
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

/// reuse_condemned: seed 3624691916862913152, 67 actions, config default
mod reuse_condemned {
    use super::*;
    const SEED: u64 = 3624691916862913152;
    fn actions() -> Vec<Action> {
        vec![
            Action::StartBackup {
                client: 0,
                job: 5,
                variant: 144,
                under: 0,
            },
            Action::Finish { pick: 0 },
            Action::Mal(Mal::FalsePack {
                items: vec![358],
                meta: false,
                delta: false,
            }),
            Action::Crash { pick: 0 },
            Action::StartBackup {
                client: 77,
                job: 127,
                variant: 22,
                under: 0,
            },
            Action::Step { pick: 0 },
            Action::Step { pick: 0 },
            Action::StartPrune { host: 2, under: 0 },
            Action::Step { pick: 16148 },
            Action::StartBackup {
                client: 232,
                job: 0,
                variant: 0,
                under: 0,
            },
            Action::Step { pick: 28065 },
            Action::Step { pick: 10419 },
            Action::Step { pick: 60579 },
            Action::Step { pick: 11973 },
            Action::Step { pick: 8046 },
            Action::Step { pick: 5490 },
            Action::LoseShard { pick: 0 },
            Action::Step { pick: 4854 },
            Action::Step { pick: 0 },
            Action::Step { pick: 0 },
            Action::StartBackup {
                client: 128,
                job: 32,
                variant: 84,
                under: 0,
            },
            Action::Step { pick: 31 },
            Action::StartBackup {
                client: 255,
                job: 0,
                variant: 0,
                under: 0,
            },
            Action::Advance(0),
            Action::Step { pick: 0 },
            Action::Step { pick: 0 },
            Action::StartForget {
                pick: 21547,
                force: true,
            },
            Action::Step { pick: 19784 },
            Action::Advance(126000),
            Action::Step { pick: 55894 },
            Action::StartSettings {
                host: 96,
                grace_hours: 2,
            },
            Action::Finish { pick: 20130 },
            Action::LoseHeadBody { pick: 0 },
            Action::Finish { pick: 20216 },
            Action::StartBackup {
                client: 100,
                job: 0,
                variant: 12,
                under: 0,
            },
            Action::Finish { pick: 17798 },
            Action::LoseHeadBody { pick: 0 },
            Action::StartPrune { host: 0, under: 0 },
            Action::Finish { pick: 27882 },
            Action::Finish { pick: 4901 },
            Action::Finish { pick: 240 },
            Action::Advance(259200),
            Action::LoseHeadBody { pick: 0 },
            Action::StartPrune { host: 8, under: 0 },
            Action::StartPrune { host: 0, under: 0 },
            Action::StartBackup {
                client: 32,
                job: 0,
                variant: 0,
                under: 0,
            },
            Action::Finish { pick: 7301 },
            Action::StartReader { pick: 0 },
            Action::StartReader { pick: 0 },
            Action::Finish { pick: 332 },
            Action::StartRebuild { host: 13 },
            Action::StartForget {
                pick: 15821,
                force: false,
            },
            Action::StartBackup {
                client: 4,
                job: 0,
                variant: 174,
                under: 0,
            },
            Action::Step { pick: 25768 },
            Action::Step { pick: 10735 },
            Action::Step { pick: 53397 },
            Action::Step { pick: 23381 },
            Action::Finish { pick: 53669 },
            Action::Step { pick: 36390 },
            Action::Step { pick: 0 },
            Action::StartForget {
                pick: 367,
                force: true,
            },
            Action::StartReader { pick: 0 },
            Action::Finish { pick: 9929 },
            Action::Finish { pick: 711 },
            Action::StartPrune { host: 93, under: 0 },
            Action::Finish { pick: 0 },
            Action::Step { pick: 0 },
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

/// trust_all: seed 0, 31 actions, config default
mod trust_all {
    use super::*;
    const SEED: u64 = 0;
    fn actions() -> Vec<Action> {
        vec![
            Action::StartBackup {
                client: 7,
                job: 188,
                variant: 59,
                under: 0,
            },
            Action::StartBackup {
                client: 20,
                job: 8,
                variant: 232,
                under: 0,
            },
            Action::Step { pick: 554 },
            Action::StartPrune { host: 63, under: 0 },
            Action::StartSettings {
                host: 55,
                grace_hours: 2,
            },
            Action::Finish { pick: 18922 },
            Action::Step { pick: 9448 },
            Action::Crash { pick: 4201 },
            Action::Step { pick: 0 },
            Action::Step { pick: 0 },
            Action::Step { pick: 0 },
            Action::Step { pick: 0 },
            Action::Step { pick: 0 },
            Action::StartPrune { host: 0, under: 0 },
            Action::Step { pick: 0 },
            Action::Step { pick: 0 },
            Action::StartBackup {
                client: 224,
                job: 0,
                variant: 4,
                under: 0,
            },
            Action::LoseShard { pick: 0 },
            Action::Mal(Mal::FalsePack {
                items: vec![13494],
                meta: true,
                delta: false,
            }),
            Action::Finish { pick: 36006 },
            Action::Step { pick: 0 },
            Action::Finish { pick: 0 },
            Action::StartBackup {
                client: 0,
                job: 0,
                variant: 0,
                under: 0,
            },
            Action::Step { pick: 0 },
            Action::Step { pick: 0 },
            Action::Step { pick: 220 },
            Action::Step { pick: 0 },
            Action::Step { pick: 0 },
            Action::Step { pick: 0 },
            Action::Step { pick: 0 },
            Action::Finish { pick: 0 },
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

/// no_head_repair: seed 0, 44 actions, config default
mod no_head_repair {
    use super::*;
    const SEED: u64 = 0;
    fn actions() -> Vec<Action> {
        vec![
            Action::StartPrune { host: 13, under: 0 },
            Action::Step { pick: 0 },
            Action::Step { pick: 0 },
            Action::Step { pick: 0 },
            Action::StartRebuild { host: 44 },
            Action::Step { pick: 0 },
            Action::StartBackup {
                client: 0,
                job: 0,
                variant: 0,
                under: 0,
            },
            Action::Step { pick: 9 },
            Action::Step { pick: 2907 },
            Action::Step { pick: 27 },
            Action::Step { pick: 880 },
            Action::Step { pick: 0 },
            Action::StartPrune { host: 9, under: 0 },
            Action::Step { pick: 10138 },
            Action::Step { pick: 29358 },
            Action::Step { pick: 781 },
            Action::Step { pick: 5026 },
            Action::Step { pick: 38166 },
            Action::Step { pick: 5268 },
            Action::Step { pick: 14157 },
            Action::Advance(0),
            Action::Advance(0),
            Action::Step { pick: 10288 },
            Action::Step { pick: 418 },
            Action::Finish { pick: 63103 },
            Action::StartForget {
                pick: 0,
                force: true,
            },
            Action::Step { pick: 10427 },
            Action::Step { pick: 2300 },
            Action::Crash { pick: 9806 },
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
            Action::Step { pick: 0 },
            Action::Step { pick: 0 },
            Action::LoseLoneHeadBody { pick: 0 },
            Action::LoseShard { pick: 0 },
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
