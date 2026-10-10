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
        let out = World::run_and_settle(cfg.clone(), s, &actions);
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
    t.lone_head_losses += s.lone_head_losses;
    t.head_repairs += s.head_repairs;
    t.head_r1_failures += s.head_r1_failures;
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
    assert!(stats.lone_head_losses > 0, "no lone head body lost");
    assert!(stats.head_repairs > 0, "no head .r1 repaired");
    assert!(stats.head_r1_failures > 0, "no head .r1 PUT failed");
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
        "one_clock_windows" => m.one_clock_windows = true,
        "no_superseded_check" => m.no_superseded_check = true,
        "refuse_future_head" => m.refuse_future_head = true,
        "fence_ignores_r1" => m.fence_ignores_r1 = true,
        "no_head_repair" => m.no_head_repair = true,
        "backup_trusts_lone_head" => m.backup_trusts_lone_head = true,
        "keep_rule_t_clause" => m.keep_rule_t_clause = true,
        "now_from_base_head_only" => m.now_from_base_head_only = true,
        "p9_ignores_head_r1_failure" => m.p9_ignores_head_r1_failure = true,
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
    "one_clock_windows",
    "no_superseded_check",
    "refuse_future_head",
    "fence_ignores_r1",
    "no_head_repair",
    "backup_trusts_lone_head",
    "keep_rule_t_clause",
    "now_from_base_head_only",
    "p9_ignores_head_r1_failure",
];

/// 探索用（不在 gate 裡）：每個突變各跑一次固定種子的隨機搜尋，印出有沒有找到反例。
#[test]
#[ignore]
fn mutation_search() {
    search_mutations(Config::default());
}

/// 同上，grace 6 小時、replicas 0：一個 case 裡有更多輪 GC。
#[test]
#[ignore]
fn mutation_search_short_grace() {
    search_mutations(Config {
        grace: 6 * HOUR,
        replicas: 0,
        ..Config::default()
    });
}

fn search_mutations(base: Config) {
    for name in MUTATIONS {
        let cfg = Config {
            mutations: mutation_named(name),
            ..base.clone()
        };
        for clock in [Clock::Hosts, Clock::Backend] {
            let r = run_random_with(cfg.clone(), clock, 2000, 7, 300);
            let found = match &r {
                Ok(_) => "no counterexample".to_string(),
                Err(e) => {
                    let first = e
                        .lines()
                        .find(|l| {
                            [
                                "R1",
                                "findability",
                                "restore of",
                                "liveness",
                                "false data-loss",
                                "newest head cannot",
                            ]
                            .iter()
                            .any(|p| l.contains(p))
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
        Action::CrashHeadWriter { pick } => format!("Action::CrashHeadWriter {{ pick: {pick} }}"),
        Action::LoseShard { pick } => format!("Action::LoseShard {{ pick: {pick} }}"),
        Action::SetSkew { host, skew } => format!("Action::SetSkew {{ host: {host}, skew: {skew} }}"),
        Action::BackendSkew(s) => format!("Action::BackendSkew({s})"),
        Action::FailReadback => "Action::FailReadback".into(),
        Action::FailHeadR1Put => "Action::FailHeadR1Put".into(),
        Action::LoseHeadBody { pick } => format!("Action::LoseHeadBody {{ pick: {pick} }}"),
        Action::LoseLoneHeadBody { pick } => format!("Action::LoseLoneHeadBody {{ pick: {pick} }}"),
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
                Mal::GroundPack {
                    items,
                    meta,
                    delta,
                    genuine,
                } => format!(
                    "Mal::GroundPack {{ items: vec!{items:?}, meta: {meta}, delta: {delta}, genuine: {genuine} }}"
                ),
                Mal::FutureSnap { item } => format!("Mal::FutureSnap {{ item: {item} }}"),
            }
        ),
    }
}

/// 工具（不在 gate 裡）：把隨機搜尋找到的反例縮到最短，印成 Rust，給 tests/found.rs 用。
#[test]
#[ignore]
fn extract_minimal_traces() {
    for name in [
        "no_verify_before_drop",
        "no_fence",
        "reuse_condemned",
        "trust_all",
        "no_head_repair",
    ] {
        let cfg = Config {
            mutations: mutation_named(name),
            ..Config::default()
        };
        let mut runner = runner_with(2000, 7, 50_000);
        let result = runner.run(&input(Clock::Hosts), |(s, actions)| {
            let out = World::run_and_settle(cfg.clone(), s, &actions);
            prop_assert!(out.violations.is_empty());
            Ok(())
        });
        let Err(proptest::test_runner::TestError::Fail(_, (s, actions))) = result else {
            println!("// {name}: no counterexample");
            continue;
        };
        println!(
            "/// {name}: seed {s}, {} actions, config default",
            actions.len()
        );
        println!("mod {name} {{");
        println!("    use super::*;");
        println!("    const SEED: u64 = {s};");
        println!("    fn actions() -> Vec<Action> {{ vec![");
        for a in &actions {
            println!("        {},", lit(a));
        }
        println!("    ] }}");
        println!("    #[test]");
        println!("    fn the_mutation_violates_an_invariant() {{");
        println!("        let cfg = Config {{");
        println!("            mutations: Mutations {{ {name}: true, ..Mutations::default() }},");
        println!("            ..Config::default()");
        println!("        }};");
        println!("        let out = World::run_and_settle(cfg, SEED, &actions());");
        println!("        assert!(!out.violations.is_empty(), \"{{}}\", out.report());");
        println!("    }}");
        println!("    #[test]");
        println!("    fn the_protocol_does_not() {{");
        println!("        let out = World::run_and_settle(Config::default(), SEED, &actions());");
        println!("        assert!(out.violations.is_empty(), \"{{}}\", out.report());");
        println!("    }}");
        println!("}}");
        println!();
    }
}
