//! commit gate 解析 chunk 時，必須與 backup 去重用同一條正本 rank（規格 §10：
//! 未標記優先，其次名稱最小；ADR 019 C1）。
//!
//! backup 開始時用有 rank 的 index：被標記的舊 pack A 不拿來去重，chunk 去重到未標記
//! 的新持有者 N。修正前 commit gate 改用 `load_index` 重新解析，它不看標記：無快取時
//! 取名稱最小者，快取重建時取先出現者（index blob 名稱排序）。A 排在 N 前面時，gate
//! 解析到 A——被標記、已過 grace——於是以 PackMissing 拒絕 commit。方向是安全失敗
//! （不寫 snapshot），但重跑不一定有用：
//! - 真 prune repack、無快取：要等下一輪 prune 刪掉 A 才自癒；
//! - 同上但 client 離線過（最後的 snapshot 早於標記）：活躍 client 規則擋住下一輪
//!   prune，A 一直不刪，重跑一直失敗，直到該 client 超過 inactive_after（預設 30 天）；
//! - 快取重建：快取記住的是歷史選擇，同一個快取重跑一樣解析到 A。
//!
//! pack 與 index blob 的名稱是加密後 bytes 的雜湊，每次執行都不同，A 是否排在 N 前面
//! 是隨機的（約一半）。所以每個情境最多跑 `MAX_ROUNDS` 回，遇到「不看標記的 loader
//! 把沿用的 chunk 解析到被標記 pack」的回合（修正前必定誤拒的那種）就停；每一回都
//! 要求 backup 成功。
//!
//! 只驗放行不夠：`deleting_the_old_pack_*` 驗資料安全的一半——放行之後讓 prune 真的
//! 刪掉被標記的舊 pack，放行的 snapshot 仍要逐 byte 還原、check 讀資料無錯。

mod common;

use std::collections::{BTreeSet, HashSet};
use std::future::Future;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use common::*;
use kist_core::{BackupOptions, CheckOptions, ForgetOptions, PruneOptions, Repository};
use kist_format::{keys, ChunkId, ObjectId};
use time::OffsetDateTime;

/// 每個情境最多跑幾回。會誤拒的排列每回約一半機率出現，24 回都沒遇到的機率約 6e-8。
const MAX_ROUNDS: u64 = 24;

const DAY: Duration = Duration::from_secs(24 * 3600);

fn client(id: u8, now: Option<OffsetDateTime>) -> BackupOptions {
    BackupOptions {
        client_id: [id; 16],
        hostname: format!("host{id}"),
        username: "tester".to_owned(),
        now,
        gc_grace: Duration::from_secs(72 * 3600),
        parity: 0,
        progress: None,
        source: kist_core::SourceSpec::default(),
    }
}

/// 某個 prefix 底下所有物件的 id。
fn ids_under(t: &TestRepo, prefix: &str) -> BTreeSet<ObjectId> {
    let dir = t.repo_path().join(prefix);
    if !dir.is_dir() {
        return BTreeSet::new();
    }
    walk_files(&dir)
        .iter()
        .map(|p| keys::object_id_from_key(&p.file_name().unwrap().to_string_lossy()).unwrap())
        .collect()
}

/// 把某個 prefix 底下所有物件的 mtime 往前撥 `age`：模擬時間流逝（prune 以後端 mtime 計齡）。
fn age_all(t: &TestRepo, prefix: &str, age: Duration) {
    let when = SystemTime::now() - age;
    for p in walk_files(&t.repo_path().join(prefix)) {
        filetime::set_file_mtime(&p, filetime::FileTime::from_system_time(when)).unwrap();
    }
}

/// 開一個用全新快取目錄的 repo：第一次 `load_index` 會從所有 index blob 重建快取。
async fn open_with_fresh_cache(t: &TestRepo) -> (Repository, tempfile::TempDir) {
    let cache = tempfile::tempdir().unwrap();
    let repo = Repository::open_with_cache(
        t.backend.clone(),
        PASSWORD.as_bytes(),
        Some(cache.path().to_path_buf()),
    )
    .await
    .unwrap();
    (repo, cache)
}

/// `chunks` 裡有沒有哪個被不看標記的 `load_index` 解析到 `marked` 裡的 pack。
async fn resolves_to_marked(
    repo: &Repository,
    chunks: &[ChunkId],
    marked: &BTreeSet<ObjectId>,
) -> bool {
    let unranked = repo.load_index().await.unwrap();
    chunks.iter().any(|c| {
        unranked
            .get(c)
            .is_some_and(|loc| marked.contains(&loc.pack))
    })
}

struct Round {
    /// 修正前的 gate 必定誤拒的排列：不看標記的 loader 把沿用的 chunk 解析到被標記的 pack。
    marked_sorts_first: bool,
    /// backup 失敗時的說明；成功是 `None`。
    rejected: Option<String>,
}

/// 真 prune repack 情境做到 b3 之前的狀態（見 [`repack_setup`]）。
struct RepackSetup {
    t: TestRepo,
    src: PathBuf,
    /// 第一輪 prune 標記的物件：被 repack 的舊 pack A，以及只裝 drop 的 pack。
    marked: BTreeSet<ObjectId>,
    marked_sorts_first: bool,
}

/// 真 prune repack 的情境（無快取），不手放標記：
/// b1（keep＋drop）→ 刪掉 drop 再 b2 → forget b1 → pack 放老 → prune 把 keep 的 chunk
/// 從半死的舊 pack A 搬進新 pack N、標記 A → 標記超過 grace，還沒有 prune 來刪 A。
/// 下一步是 b3，由呼叫端做。
/// `offline`：b1、b2 在 6 天前與 5 天前（早於標記），最後再跑第二輪 prune，
/// 確認它被活躍 client 規則擋住、A 還在。
async fn repack_setup(offline: bool, seed: u64) -> RepackSetup {
    let t = TestRepo::new().await;
    let src = t.dir.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    // keep 排在前面，和一大段 drop 同在第一顆 pack：刪掉 drop 之後那顆 pack 的存活
    // 比例遠低於 repack 門檻（50%），prune 一定會把 keep 搬走。
    std::fs::write(src.join("keep.bin"), random_bytes(seed + 1000, 60 * 1024)).unwrap();
    std::fs::write(src.join("zz-drop.bin"), random_bytes(seed, 900 * 1024)).unwrap();

    let repo = t.open().await;
    let (t1, t2) = if offline {
        let r = OffsetDateTime::now_utc();
        (
            Some(r - time::Duration::days(6)),
            Some(r - time::Duration::days(5)),
        )
    } else {
        (None, None)
    };
    // 注入過去的時間時也在那個時間 commit，否則 commit 以真實時鐘算出「跑了好幾天」而拒絕。
    let b1 = repo
        .backup_prepare(std::slice::from_ref(&src), client(1, t1))
        .await
        .unwrap();
    let b1 = match t1 {
        Some(at) => b1.commit_at(at).await.unwrap(),
        None => b1.commit().await.unwrap(),
    };
    std::fs::remove_file(src.join("zz-drop.bin")).unwrap();
    let b2 = repo
        .backup_prepare(std::slice::from_ref(&src), client(1, t2))
        .await
        .unwrap();
    match t2 {
        Some(at) => b2.commit_at(at).await.unwrap(),
        None => b2.commit().await.unwrap(),
    };
    repo.forget(ForgetOptions {
        snapshots: vec![b1.snapshot_key],
        policy: Default::default(),
        dry_run: false,
    })
    .await
    .unwrap();

    // 時間過去：pack 比 grace 老，prune 才會 repack／標記它們
    age_all(&t, keys::PACKS_PREFIX, 8 * DAY);
    let before = ids_under(&t, keys::PACKS_PREFIX);
    let p1 = t.open().await.prune(PruneOptions::default()).await.unwrap();
    assert!(p1.repacked_packs >= 1, "{p1:?}");
    let new_packs: HashSet<ObjectId> = ids_under(&t, keys::PACKS_PREFIX)
        .difference(&before)
        .copied()
        .collect();
    let marked = ids_under(&t, keys::GC_PREFIX);
    // 又過了 4 天、沒有 prune 來刪：標記超過 72 小時的 grace，A 還在
    age_all(&t, keys::GC_PREFIX, 4 * DAY);

    if offline {
        // 第二輪 prune：client 在標記之後沒有新的 snapshot，又還沒超過 inactive_after
        // → 擋住刪除。A 不會自己消失，修正前這之後的 b3 怎麼重跑都失敗。
        let p2 = t.open().await.prune(PruneOptions::default()).await.unwrap();
        assert_eq!(p2.deleted, 0, "{p2:?}");
        assert!(p2.blocked > 0, "{p2:?}");
    }

    // 搬進 N 的 chunk，就是 b3 會沿用的那些
    let mut errors = Vec::new();
    let blobs = t.open().await.load_index_blobs(&mut errors).await.unwrap();
    assert!(errors.is_empty(), "{errors:?}");
    let moved: Vec<ChunkId> = blobs
        .effective
        .iter()
        .flat_map(|(_, blob)| blob.packs.iter())
        .filter(|p| new_packs.contains(&p.pack))
        .flat_map(|p| p.entries.iter().map(|e| e.id))
        .collect();
    assert!(!moved.is_empty());
    let marked_sorts_first = resolves_to_marked(&t.open().await, &moved, &marked).await;
    RepackSetup {
        t,
        src,
        marked,
        marked_sorts_first,
    }
}

/// 放行的一半：[`repack_setup`] 之後的 b3 必須成功。
async fn repack_round(offline: bool, seed: u64) -> Round {
    let s = repack_setup(offline, seed).await;
    let rejected =
        s.t.open()
            .await
            .backup(std::slice::from_ref(&s.src), client(1, None))
            .await
            .err()
            .map(|e| format!("repack offline={offline} seed={seed}: {e}"));
    Round {
        marked_sorts_first: s.marked_sorts_first,
        rejected,
    }
}

/// 資料安全的一半：gate 放行的 b3，在被標記的舊 pack 真的刪掉之後仍然還原得出來。
/// [`repack_setup`] 之後：b3 必須成功，而且一個新 chunk 都沒寫（backup 不拿被標記的
/// pack 去重，所以沿用的 chunk 全落在未標記的新持有者 N）→ 跑 prune（標記已超過 grace）：
/// b3 晚於標記，活躍 client 規則不再擋，第一輪標記的 pack（含 A）必須全數刪掉 →
/// 還原 b3 逐 byte 比對來源、check 讀資料無錯。
/// A 排在 N 前面的排列（`marked_sorts_first`）正是不看標記的 loader（restore 用的
/// `load_index`）會先挑 A 的那種：A 刪掉之後 index 必須已經不指它。
async fn repack_then_delete_round(offline: bool, seed: u64) -> Round {
    let s = repack_setup(offline, seed).await;
    let t = &s.t;
    let b3 = match t
        .open()
        .await
        .backup(std::slice::from_ref(&s.src), client(1, None))
        .await
    {
        Ok(b3) => b3,
        Err(e) => {
            return Round {
                marked_sorts_first: s.marked_sorts_first,
                rejected: Some(format!("repack offline={offline} seed={seed}: {e}")),
            }
        }
    };
    assert_eq!(b3.report.chunks_new, 0, "b3 沒有完全去重：{:?}", b3.report);

    // 標記在 setup 裡已老到 4 天（超過 72 小時的 grace）；b3 晚於標記，這一輪就該刪。
    // 不再把標記撥得更老：刪除前 prune 會比 pack 與標記的 mtime，標記不比 pack 新
    // （同一秒算重寫過）就當作標記後被重寫而復活——pack 在 setup 裡撥成 8 天前，
    // 標記也撥到 8 天前會落在同一秒。
    let p3 = t.open().await.prune(PruneOptions::default()).await.unwrap();
    assert_eq!(p3.blocked, 0, "{p3:?}");
    assert!(p3.deleted > 0, "{p3:?}");
    let left: Vec<ObjectId> = ids_under(t, keys::PACKS_PREFIX)
        .intersection(&s.marked)
        .copied()
        .collect();
    assert!(left.is_empty(), "被標記的舊 pack 還在：{left:?}\n{p3:?}");

    // 還原 b3：來源此時只剩 keep.bin，逐 byte（連 mtime）相同
    let fresh = t.open().await;
    let out = t.dir.path().join("out");
    let r = fresh
        .restore(&b3.snapshot_key, &out, Default::default())
        .await
        .unwrap();
    assert!(r.errors.is_empty(), "還原失敗：{:?}", r.errors);
    assert_same_tree(&s.src, &out.join(s.src.strip_prefix("/").unwrap_or(&s.src)));
    let report = fresh
        .check(CheckOptions {
            read_data: true,
            repair: false,
        })
        .await
        .unwrap();
    assert!(report.errors.is_empty(), "check：{:?}", report.errors);
    Round {
        marked_sorts_first: s.marked_sorts_first,
        rejected: None,
    }
}

/// 快取重建的情境（手放標記）：client 1 備份 → 標記其中一顆 pack V（還年輕）→
/// client 2 備份，不拿 V 去重，把 V 的 chunk 重寫進新 pack → 標記超過 grace，V 還在 →
/// client 1 用全新的快取目錄再備份（有 parent，內容沿用）。
async fn rebuilt_cache_round() -> Round {
    let t = TestRepo::new().await;
    let src = t.dir.path().join("src");
    make_source(&src);
    t.open()
        .await
        .backup(std::slice::from_ref(&src), client(1, None))
        .await
        .unwrap();
    let victim = *ids_under(&t, keys::PACKS_PREFIX).first().unwrap();
    let victim_chunks: Vec<ChunkId> = t
        .open()
        .await
        .load_index()
        .await
        .unwrap()
        .chunks()
        .into_iter()
        .filter(|(_, loc)| loc.pack == victim)
        .map(|(id, _)| id)
        .collect();
    assert!(!victim_chunks.is_empty());

    // 標記的內容格式由 prune 定義；backup 只看 key 與 mtime
    let mark_path = t.repo_path().join(keys::gc(&victim));
    std::fs::create_dir_all(mark_path.parent().unwrap()).unwrap();
    std::fs::write(&mark_path, keys::GC_MARK_MAGIC).unwrap();
    let s2 = t
        .open()
        .await
        .backup(std::slice::from_ref(&src), client(2, None))
        .await
        .unwrap();
    assert_eq!(s2.report.chunks_new, victim_chunks.len() as u64);
    // 標記超過 72 小時的 grace，還沒有 prune 來刪 V
    age_all(&t, keys::GC_PREFIX, 4 * DAY);

    let marked = BTreeSet::from([victim]);
    let (probe, _probe_cache) = open_with_fresh_cache(&t).await;
    let marked_sorts_first = resolves_to_marked(&probe, &victim_chunks, &marked).await;

    let (repo, _cache) = open_with_fresh_cache(&t).await;
    let rejected = repo
        .backup(std::slice::from_ref(&src), client(1, None))
        .await
        .err()
        .map(|e| format!("rebuilt cache: {e}"));
    Round {
        marked_sorts_first,
        rejected,
    }
}

/// 跑到遇上會誤拒的排列為止，每一回 backup 都必須成功。
async fn run_until_marked_sorts_first<F, Fut>(round: F)
where
    F: Fn(u64) -> Fut,
    Fut: Future<Output = Round>,
{
    let mut rejected = Vec::new();
    let mut hit = false;
    for seed in 0..MAX_ROUNDS {
        let r = round(seed).await;
        rejected.extend(r.rejected);
        if r.marked_sorts_first {
            hit = true;
            break;
        }
    }
    assert!(rejected.is_empty(), "commit gate 誤拒：{rejected:#?}");
    assert!(
        hit,
        "{MAX_ROUNDS} 回都沒遇到被標記 pack 排在前面的排列，這個測試沒有驗到東西"
    );
}

#[tokio::test]
async fn gate_accepts_dedup_to_the_repacked_holder() {
    run_until_marked_sorts_first(|seed| repack_round(false, seed)).await;
}

#[tokio::test]
async fn gate_accepts_dedup_to_the_repacked_holder_for_an_offline_client() {
    run_until_marked_sorts_first(|seed| repack_round(true, seed)).await;
}

#[tokio::test]
async fn deleting_the_old_pack_keeps_the_accepted_snapshot_restorable() {
    run_until_marked_sorts_first(|seed| repack_then_delete_round(false, seed)).await;
}

#[tokio::test]
async fn deleting_the_old_pack_keeps_the_accepted_snapshot_restorable_for_an_offline_client() {
    run_until_marked_sorts_first(|seed| repack_then_delete_round(true, seed)).await;
}

#[tokio::test]
async fn gate_accepts_dedup_to_the_rewritten_holder_with_a_rebuilt_cache() {
    run_until_marked_sorts_first(|_| rebuilt_cache_round()).await;
}
