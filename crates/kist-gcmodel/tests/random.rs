//! 隨機交錯：proptest 產生 action 清單，每一步之後檢查不變式。失敗時 proptest 縮小清單，報告的
//! 是最短的交錯與事件記錄。

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::collections::BTreeMap;

use common::{input, Clock};
use kist_gcmodel::*;
use proptest::prelude::*;
use proptest::strategy::ValueTree;
use proptest::test_runner::{Config as PtConfig, RngAlgorithm, TestRng, TestRunner};

fn runner(cases: u32, seed: u8) -> TestRunner {
    runner_with(cases, seed, 20_000)
}

fn runner_with(cases: u32, seed: u8, shrink: u32) -> TestRunner {
    let cfg = PtConfig {
        cases,
        max_shrink_iters: shrink,
        failure_persistence: None,
        ..PtConfig::default()
    };
    TestRunner::new_with_rng(cfg, TestRng::from_seed(RngAlgorithm::ChaCha, &[seed; 32]))
}

fn run_random(cfg: Config, clock: Clock, cases: u32, seed: u8) -> Result<Stats, String> {
    run_random_with(cfg, clock, cases, seed, 20_000)
}

fn run_random_with(
    cfg: Config,
    clock: Clock,
    cases: u32,
    seed: u8,
    shrink: u32,
) -> Result<Stats, String> {
    let total = std::cell::RefCell::new(Stats::default());
    let mut runner = runner_with(cases, seed, shrink);
    let result = runner.run(&input(clock), |(s, actions)| {
        let out = World::run(cfg.clone(), s, &actions);
        add(&mut total.borrow_mut(), &out.stats);
        prop_assert!(
            out.violations.is_empty(),
            "{}\nactions: {:?}",
            out.report(),
            actions
        );
        Ok(())
    });
    result.map_err(|e| e.to_string())?;
    Ok(total.into_inner())
}

fn add(t: &mut Stats, s: &Stats) {
    t.commits += s.commits;
    t.backup_aborts += s.backup_aborts;
    t.generations += s.generations;
    t.fence_losses += s.fence_losses;
    t.prune_refusals += s.prune_refusals;
    t.condemned += s.condemned;
    t.revived += s.revived;
    t.tombstoned += s.tombstoned;
    t.repacked += s.repacked;
    t.packs_deleted += s.packs_deleted;
    t.false_claims_removed += s.false_claims_removed;
    t.rebuilds += s.rebuilds;
    t.reads_ok += s.reads_ok;
}

/// 隨機測試本身不能空轉：這些路徑在整批 case 裡都要走到過。
fn assert_covered(stats: &Stats) {
    println!("stats: {stats:?}");
    assert!(stats.commits > 0, "no commits");
    assert!(stats.condemned > 0, "nothing condemned");
    assert!(stats.revived > 0, "nothing revived");
    assert!(stats.tombstoned > 0, "nothing tombstoned");
    assert!(stats.packs_deleted > 0, "nothing deleted");
    assert!(stats.repacked > 0, "nothing repacked");
    assert!(stats.false_claims_removed > 0, "no false claim caught");
    assert!(stats.fence_losses > 0, "no prune lost the fence");
    assert!(stats.rebuilds > 0, "no rebuild");
    assert!(stats.reads_ok > 0, "no restore finished");
}

#[test]
fn random_interleavings_with_bad_host_clocks() {
    match run_random(Config::default(), Clock::Hosts, 512, 1) {
        Ok(stats) => assert_covered(&stats),
        Err(e) => panic!("{e}"),
    }
}

#[test]
fn random_interleavings_with_a_bad_backend_clock() {
    match run_random(Config::default(), Clock::Backend, 512, 2) {
        Ok(stats) => assert_covered(&stats),
        Err(e) => panic!("{e}"),
    }
}

/// S3 的預設（replicas 0）配上短的 grace（6 小時），一個 case 裡會有更多輪 GC。
#[test]
fn random_interleavings_with_a_short_grace_and_no_replicas() {
    let cfg = Config {
        grace: 6 * HOUR,
        replicas: 0,
        ..Config::default()
    };
    match run_random(cfg, Clock::Hosts, 512, 3) {
        Ok(stats) => assert_covered(&stats),
        Err(e) => panic!("{e}"),
    }
}

/// 較長的版本（不在 gate 裡）：三種設定各 5000 個 case。
#[test]
#[ignore]
fn random_interleavings_deep() {
    let short = Config {
        grace: 6 * HOUR,
        replicas: 0,
        ..Config::default()
    };
    for (cfg, clock, seed) in [
        (Config::default(), Clock::Hosts, 11),
        (Config::default(), Clock::Backend, 12),
        (short, Clock::Hosts, 13),
    ] {
        match run_random(cfg, clock, 5000, seed) {
            Ok(stats) => assert_covered(&stats),
            Err(e) => panic!("{e}"),
        }
    }
}

/// 診斷用（不在 gate 裡）：actor 為什麼停下來，依訊息開頭統計。
#[test]
#[ignore]
fn stop_reasons() {
    let mut runner = runner(64, 1);
    let mut hist: BTreeMap<String, usize> = BTreeMap::new();
    for _ in 0..64 {
        let (s, actions) = input(Clock::Hosts).new_tree(&mut runner).unwrap().current();
        let out = World::run(Config::default(), s, &actions);
        for line in &out.log {
            if let Some(pos) = line.find("stopped: ") {
                let key: String = line[pos + 9..].chars().take(32).collect();
                *hist.entry(key).or_default() += 1;
            }
        }
    }
    let mut v: Vec<_> = hist.into_iter().collect();
    v.sort_by_key(|e| std::cmp::Reverse(e.1));
    for (k, n) in v.iter().take(30) {
        println!("{n:6} {k}");
    }
}

fn mutation_named(name: &str) -> Mutations {
    let mut m = Mutations::default();
    match name {
        "no_verify_before_drop" => m.no_verify_before_drop = true,
        "only_writer_clock" => m.only_writer_clock = true,
        "only_backend_clock" => m.only_backend_clock = true,
        "no_elapsed_check" => m.no_elapsed_check = true,
        "no_fence" => m.no_fence = true,
        "reuse_condemned" => m.reuse_condemned = true,
        "newcomer_first" => m.newcomer_first = true,
        "no_parent_head" => m.no_parent_head = true,
        "no_delta_prefix_rule" => m.no_delta_prefix_rule = true,
        "trust_all" => m.trust_all = true,
        "no_b4_entries_check" => m.no_b4_entries_check = true,
        "no_hold" => m.no_hold = true,
        "no_run_bound" => m.no_run_bound = true,
        "no_drain" => m.no_drain = true,
        "no_prefix_and_no_entries" => {
            m.no_delta_prefix_rule = true;
            m.no_b4_entries_check = true;
        }
        other => panic!("unknown mutation {other}"),
    }
    m
}

pub const MUTATIONS: &[&str] = &[
    "no_verify_before_drop",
    "only_writer_clock",
    "only_backend_clock",
    "no_elapsed_check",
    "no_fence",
    "reuse_condemned",
    "newcomer_first",
    "no_parent_head",
    "no_delta_prefix_rule",
    "trust_all",
    "no_b4_entries_check",
    "no_hold",
    "no_run_bound",
    "no_drain",
    "no_prefix_and_no_entries",
];

/// 探索用（不在 gate 裡）：每個突變各跑一次固定種子的隨機搜尋，印出有沒有找到反例。
#[test]
#[ignore]
fn mutation_search() {
    for name in MUTATIONS {
        let cfg = Config {
            mutations: mutation_named(name),
            ..Config::default()
        };
        for clock in [Clock::Hosts, Clock::Backend] {
            let r = run_random_with(cfg.clone(), clock, 2000, 7, 300);
            let found = match &r {
                Ok(_) => "no counterexample".to_string(),
                Err(e) => {
                    let first = e
                        .lines()
                        .find(|l| {
                            l.contains("R1")
                                || l.contains("findability")
                                || l.contains("restore")
                                || l.contains("newest head")
                        })
                        .unwrap_or("?");
                    format!("FOUND: {}", first.trim())
                }
            };
            println!("{name:28} {clock:?}: {found}");
        }
    }
}

fn lit(a: &Action) -> String {
    match a {
        Action::Advance(t) => format!("Action::Advance({t})"),
        Action::StartBackup {
            client,
            job,
            variant,
            under,
        } => format!(
            "Action::StartBackup {{ client: {client}, job: {job}, variant: {variant}, under: {under} }}"
        ),
        Action::StartPrune { host, under } => format!("Action::StartPrune {{ host: {host}, under: {under} }}"),
        Action::StartForget { pick, force } => format!("Action::StartForget {{ pick: {pick}, force: {force} }}"),
        Action::StartReader { pick } => format!("Action::StartReader {{ pick: {pick} }}"),
        Action::StartSettings { host, grace_hours } => {
            format!("Action::StartSettings {{ host: {host}, grace_hours: {grace_hours} }}")
        }
        Action::StartRebuild { host } => format!("Action::StartRebuild {{ host: {host} }}"),
        Action::Step { pick } => format!("Action::Step {{ pick: {pick} }}"),
        Action::Finish { pick } => format!("Action::Finish {{ pick: {pick} }}"),
        Action::Crash { pick } => format!("Action::Crash {{ pick: {pick} }}"),
        Action::LoseShard { pick } => format!("Action::LoseShard {{ pick: {pick} }}"),
        Action::SetSkew { host, skew } => format!("Action::SetSkew {{ host: {host}, skew: {skew} }}"),
        Action::BackendSkew(s) => format!("Action::BackendSkew({s})"),
        Action::FailReadback => "Action::FailReadback".into(),
        Action::Mal(m) => format!(
            "Action::Mal({})",
            match m {
                Mal::FalsePack { items, meta, delta } => {
                    format!("Mal::FalsePack {{ items: vec!{items:?}, meta: {meta}, delta: {delta} }}")
                }
                Mal::ForeignDelta { pack_pick, items } => {
                    format!("Mal::ForeignDelta {{ pack_pick: {pack_pick}, items: vec!{items:?} }}")
                }
                Mal::JunkSnap { r1 } => format!("Mal::JunkSnap {{ r1: {r1} }}"),
                Mal::JunkPack => "Mal::JunkPack".into(),
                Mal::ForgedSnap { item } => format!("Mal::ForgedSnap {{ item: {item} }}"),
                Mal::UnknownVersionPack => "Mal::UnknownVersionPack".into(),
            }
        ),
    }
}

/// 工具（不在 gate 裡）：把隨機搜尋找到的反例縮到最短，印成 Rust，給 tests/found.rs 用。
#[test]
#[ignore]
fn extract_minimal_traces() {
    for (name, clock) in [
        ("no_elapsed_check", Clock::Hosts),
        ("no_fence", Clock::Hosts),
        ("trust_all", Clock::Hosts),
    ] {
        let cfg = Config {
            mutations: mutation_named(name),
            ..Config::default()
        };
        let mut runner = runner_with(2000, 7, 50_000);
        let result = runner.run(&input(clock), |(s, actions)| {
            let out = World::run(cfg.clone(), s, &actions);
            prop_assert!(out.violations.is_empty());
            Ok(())
        });
        if let Err(proptest::test_runner::TestError::Fail(_, (s, actions))) = result {
            println!("// {name}: seed {s}, {} actions", actions.len());
            println!("const SEED: u64 = {s};");
            println!("fn actions() -> Vec<Action> {{ vec![");
            for a in &actions {
                println!("    {},", lit(a));
            }
            println!("] }}");
        } else {
            println!("// {name}: no counterexample");
        }
    }
}
