//! 隨機搜尋（tests/random.rs 的 `mutation_search`）找到的反例，縮到最短後固定下來：同一條交錯，
//! 打開突變就違反不變式，原協定就不違反。證明隨機交錯本身找得到這幾類錯誤。交錯依賴模型目前的
//! 行為（包括世界的亂數），模型改了之後若不再重現，用 `extract_minimal_traces` 重新產生。

#![allow(clippy::unwrap_used, clippy::expect_used)]

use kist_gcmodel::*;

fn mutated(m: Mutations) -> Config {
    Config {
        mutations: m,
        ..Config::default()
    }
}

/// no_elapsed_check: seed 13419682308134130894, 54 actions
mod no_elapsed_check {
    use super::*;

    const SEED: u64 = 13419682308134130894;
    fn actions() -> Vec<Action> {
        vec![
            Action::StartBackup {
                client: 0,
                job: 20,
                variant: 144,
                under: 0,
            },
            Action::Step { pick: 0 },
            Action::Step { pick: 0 },
            Action::Step { pick: 0 },
            Action::Step { pick: 0 },
            Action::Step { pick: 0 },
            Action::Step { pick: 0 },
            Action::Crash { pick: 0 },
            Action::Advance(262800),
            Action::StartPrune { host: 0, under: 0 },
            Action::Finish { pick: 0 },
            Action::StartPrune { host: 0, under: 0 },
            Action::StartBackup {
                client: 3,
                job: 20,
                variant: 0,
                under: 0,
            },
            Action::Step { pick: 0 },
            Action::Step { pick: 0 },
            Action::Step { pick: 0 },
            Action::Step { pick: 0 },
            Action::StartRebuild { host: 0 },
            Action::Step { pick: 0 },
            Action::Step { pick: 0 },
            Action::StartPrune { host: 1, under: 0 },
            Action::StartBackup {
                client: 119,
                job: 0,
                variant: 4,
                under: 0,
            },
            Action::Step { pick: 28 },
            Action::Advance(0),
            Action::Step { pick: 33509 },
            Action::StartBackup {
                client: 67,
                job: 127,
                variant: 0,
                under: 0,
            },
            Action::Step { pick: 0 },
            Action::Finish { pick: 21497 },
            Action::Step { pick: 0 },
            Action::Step { pick: 1 },
            Action::Step { pick: 5393 },
            Action::Step { pick: 3093 },
            Action::Step { pick: 3761 },
            Action::Step { pick: 469 },
            Action::Step { pick: 0 },
            Action::StartForget {
                pick: 0,
                force: false,
            },
            Action::StartForget {
                pick: 0,
                force: false,
            },
            Action::Advance(259200),
            Action::Finish { pick: 40988 },
            Action::Step { pick: 0 },
            Action::StartReader { pick: 0 },
            Action::StartPrune { host: 0, under: 0 },
            Action::Finish { pick: 16085 },
            Action::Finish { pick: 52 },
            Action::Finish { pick: 943 },
            Action::StartPrune { host: 0, under: 0 },
            Action::Step { pick: 7585 },
            Action::Step { pick: 17279 },
            Action::Step { pick: 7355 },
            Action::Step { pick: 0 },
            Action::Step { pick: 0 },
            Action::Step { pick: 0 },
            Action::Step { pick: 0 },
            Action::Finish { pick: 0 },
        ]
    }

    #[test]
    fn the_mutation_violates_an_invariant() {
        let out = World::run(
            mutated(Mutations {
                no_elapsed_check: true,
                ..Mutations::default()
            }),
            SEED,
            &actions(),
        );
        assert!(!out.violations.is_empty(), "{}", out.report());
    }

    #[test]
    fn the_protocol_does_not() {
        let out = World::run(Config::default(), SEED, &actions());
        assert!(out.violations.is_empty(), "{}", out.report());
    }
}

/// no_fence: seed 0, 29 actions
mod no_fence {
    use super::*;

    const SEED: u64 = 0;
    fn actions() -> Vec<Action> {
        vec![
            Action::Step { pick: 0 },
            Action::StartBackup {
                client: 1,
                job: 0,
                variant: 0,
                under: 0,
            },
            Action::StartBackup {
                client: 62,
                job: 0,
                variant: 0,
                under: 0,
            },
            Action::Step { pick: 0 },
            Action::Finish { pick: 28657 },
            Action::Step { pick: 0 },
            Action::StartBackup {
                client: 80,
                job: 0,
                variant: 0,
                under: 0,
            },
            Action::Step { pick: 0 },
            Action::Step { pick: 0 },
            Action::Step { pick: 0 },
            Action::Finish { pick: 14971 },
            Action::Mal(Mal::ForgedSnap { item: 0 }),
            Action::StartBackup {
                client: 2,
                job: 0,
                variant: 0,
                under: 0,
            },
            Action::StartForget {
                pick: 0,
                force: false,
            },
            Action::Step { pick: 0 },
            Action::Finish { pick: 0 },
            Action::StartForget {
                pick: 19326,
                force: false,
            },
            Action::StartSettings {
                host: 0,
                grace_hours: 2,
            },
            Action::StartBackup {
                client: 9,
                job: 0,
                variant: 0,
                under: 0,
            },
            Action::StartPrune { host: 25, under: 0 },
            Action::Step { pick: 2683 },
            Action::Step { pick: 0 },
            Action::Step { pick: 46542 },
            Action::Step { pick: 56159 },
            Action::Step { pick: 9434 },
            Action::Step { pick: 1167 },
            Action::Step { pick: 65082 },
            Action::Crash { pick: 1204 },
            Action::Finish { pick: 23444 },
        ]
    }

    #[test]
    fn the_mutation_violates_an_invariant() {
        let out = World::run(
            mutated(Mutations {
                no_fence: true,
                ..Mutations::default()
            }),
            SEED,
            &actions(),
        );
        assert!(!out.violations.is_empty(), "{}", out.report());
    }

    #[test]
    fn the_protocol_does_not() {
        let out = World::run(Config::default(), SEED, &actions());
        assert!(out.violations.is_empty(), "{}", out.report());
    }
}

/// trust_all: seed 0, 24 actions
mod trust_all {
    use super::*;

    const SEED: u64 = 0;
    fn actions() -> Vec<Action> {
        vec![
            Action::Crash { pick: 0 },
            Action::StartBackup {
                client: 0,
                job: 0,
                variant: 56,
                under: 0,
            },
            Action::Finish { pick: 0 },
            Action::StartBackup {
                client: 3,
                job: 151,
                variant: 16,
                under: 0,
            },
            Action::StartBackup {
                client: 1,
                job: 0,
                variant: 41,
                under: 0,
            },
            Action::Step { pick: 23149 },
            Action::Crash { pick: 10925 },
            Action::Step { pick: 0 },
            Action::StartBackup {
                client: 7,
                job: 0,
                variant: 2,
                under: 0,
            },
            Action::StartPrune {
                host: 125,
                under: 0,
            },
            Action::Crash { pick: 21189 },
            Action::StartBackup {
                client: 83,
                job: 0,
                variant: 7,
                under: 0,
            },
            Action::Finish { pick: 4001 },
            Action::Step { pick: 39890 },
            Action::StartPrune { host: 0, under: 0 },
            Action::Mal(Mal::FalsePack {
                items: vec![14458],
                meta: true,
                delta: false,
            }),
            Action::StartBackup {
                client: 234,
                job: 0,
                variant: 0,
                under: 0,
            },
            Action::Finish { pick: 8680 },
            Action::Finish { pick: 975 },
            Action::StartBackup {
                client: 31,
                job: 0,
                variant: 109,
                under: 0,
            },
            Action::Step { pick: 0 },
            Action::Advance(0),
            Action::Step { pick: 0 },
            Action::Finish { pick: 18617 },
        ]
    }

    #[test]
    fn the_mutation_violates_an_invariant() {
        let out = World::run(
            mutated(Mutations {
                trust_all: true,
                ..Mutations::default()
            }),
            SEED,
            &actions(),
        );
        assert!(!out.violations.is_empty(), "{}", out.report());
    }

    #[test]
    fn the_protocol_does_not() {
        let out = World::run(Config::default(), SEED, &actions());
        assert!(out.violations.is_empty(), "{}", out.report());
    }
}
