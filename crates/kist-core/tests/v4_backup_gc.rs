//! v4 backup 面對 GC 的規則（format-v4.md §11.2 的 B1–B5）：每一條都有一個「規則不成立就不 commit」的情境，
//! 多數另有一個成立時照常 commit 的對照。維護角色的動作（寫 head 與分片）由測試扮演
//! （`v4_common::write_generation`），prune 本身在第 6 段。

#[path = "v4_common/mod.rs"]
mod v4_common;

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use kist_core::v4::{Clock, Reading};
use kist_core::CoreError;
use kist_format::v4::ids::ObjectId;
use kist_format::v4::index::{status, Delta, DeltaPack, Head, Status};
use kist_format::v4::keys::{Key, SnapTs};
use kist_format::v4::pack::{self, Algorithm, Kind, PackEntry, PackTrailer};
use v4_common::*;

fn src(t: &TestRepo, name: &str) -> std::path::PathBuf {
    let p = t.dir.path().join(name);
    sample_source(&p);
    p
}

fn data_packs_of(records: &[kist_format::v4::index::Record], c: u8) -> Vec<ObjectId> {
    records
        .iter()
        .filter(|r| r.kind == Kind::Data && r.client == client(c))
        .map(|r| r.id)
        .collect()
}

#[tokio::test]
async fn b1_refuses_a_lone_head_and_proceeds_once_its_r1_is_there() {
    let t = TestRepo::new().await;
    let s = src(&t, "src");
    let repo = t.open().await;
    let g = write_generation(&repo, Vec::new(), true).await;
    let err = repo
        .backup(std::slice::from_ref(&s), opts(1))
        .await
        .unwrap_err();
    assert!(
        matches!(&err, CoreError::Generation(m) if m.contains(".r1")),
        "{err}"
    );
    for prefix in ["packs", "mpacks", "deltas", "snapshots"] {
        assert!(
            t.keys(prefix).await.is_empty(),
            "{prefix}: nothing is written"
        );
    }
    // 補上 `.r1`（維護角色的 P1）之後照常
    let body = Key::Head { g, replica: false }.path();
    let bytes = t.backend.get(&body).await.unwrap();
    t.backend
        .put_if_absent(&Key::Head { g, replica: true }.path(), bytes)
        .await
        .unwrap();
    let ok = repo.backup(&[s], opts(1)).await.unwrap();
    assert_eq!(ok.generation, g);
}

#[tokio::test]
async fn b1_refuses_an_unreadable_head_or_one_of_an_unknown_version() {
    let t = TestRepo::new().await;
    let s = src(&t, "src");
    let repo = t.open().await;
    for replica in [false, true] {
        t.backend
            .put_if_absent(&Key::Head { g: 1, replica }.path(), b"garbage".to_vec())
            .await
            .unwrap();
    }
    let err = repo
        .backup(std::slice::from_ref(&s), opts(1))
        .await
        .unwrap_err();
    assert!(
        matches!(&err, CoreError::Generation(m) if m.contains("cannot read generation head 1")),
        "{err}"
    );

    let t = TestRepo::new().await;
    let s = src(&t, "src");
    let repo = t.open().await;
    let base = repo.read_head(0).await.unwrap();
    let future = Head {
        v: 5,
        g: 1,
        parent: Some(base.hash),
        ..base.head
    };
    let sealed = repo
        .keys()
        .seal_head(1, &kist_format::cbor::encode(&future).unwrap())
        .unwrap();
    for replica in [false, true] {
        t.backend
            .put_if_absent(&Key::Head { g: 1, replica }.path(), sealed.clone())
            .await
            .unwrap();
    }
    let err = repo.backup(&[s], opts(1)).await.unwrap_err();
    assert!(err.to_string().contains("version 5"), "{err}");
}

/// B2：backup 不沿用 condemned 的持有者（§11.4）；同一份內容在 live 的持有者上就取回驗證、沿用。
#[tokio::test]
async fn b2_rewrites_what_only_a_condemned_pack_holds() {
    for condemned in [true, false] {
        let t = TestRepo::new().await;
        let one = src(&t, "one");
        let repo = t.open_as("c1").await;
        repo.backup(&[one], opts(1)).await.unwrap();
        let records = index_everything(&repo, &BTreeMap::new()).await;
        let marks: BTreeMap<ObjectId, Status> = if condemned {
            data_packs_of(&records, 1)
                .into_iter()
                .map(|id| {
                    (
                        id,
                        Status {
                            s: status::CONDEMNED,
                            at: 1,
                            age: 7200,
                        },
                    )
                })
                .collect()
        } else {
            BTreeMap::new()
        };
        let records = index_everything(&repo, &marks).await;
        write_generation(&repo, records, false).await;

        // 另一台 client 備同樣的內容（不同的路徑，沒有 parent）
        let two = src(&t, "two");
        let other = t.open_as("c2").await;
        let s = other
            .backup(std::slice::from_ref(&two), opts(2))
            .await
            .unwrap();
        let chunks = s.report.chunks_new + s.report.chunks_reused;
        if condemned {
            assert_eq!(s.report.chunks_reused, 0, "{:?}", s.report);
            assert!(s.report.chunks_new > 0);
        } else {
            assert_eq!(s.report.chunks_new, 0, "{:?}", s.report);
            assert!(s.report.verified >= chunks, "{:?}", s.report);
        }
        let back = read_tree(&other, &s.snapshot).await;
        assert!(back
            .values()
            .any(|n| *n == Node::File(data("big", 300 * 1024))));
    }
}

/// B4：去重用到的 pack 在 `Map(g_c)` 是 tombstone → 不 commit；只是多了新進者的 `g_c` 照常 commit。
#[tokio::test]
async fn b4_refuses_when_a_reused_pack_became_a_tombstone() {
    for tombstone in [true, false] {
        let t = TestRepo::new().await;
        let repo = t.open_as("c1").await;
        repo.backup(&[src(&t, "one")], opts(1)).await.unwrap();
        let records = index_everything(&repo, &BTreeMap::new()).await;
        let reused = data_packs_of(&records, 1);
        write_generation(&repo, records, false).await;

        let other = t.open_as("c2").await;
        let prepared = other
            .backup_prepare(&[src(&t, "two")], opts(2))
            .await
            .unwrap();
        assert!(prepared.report().chunks_reused > 0);
        let marks: BTreeMap<ObjectId, Status> = if tombstone {
            reused
                .iter()
                .map(|id| {
                    (
                        *id,
                        Status {
                            s: status::TOMBSTONE,
                            at: 2,
                            age: 0,
                        },
                    )
                })
                .collect()
        } else {
            BTreeMap::new()
        };
        let records = index_everything(&repo, &marks).await;
        write_generation(&repo, records, false).await;
        let result = prepared.commit().await;
        let theirs = t.keys(&format!("snapshots/{}", client(2))).await;
        if tombstone {
            let err = result.unwrap_err();
            assert!(
                matches!(&err, CoreError::NotCommitted(m) if m.contains("tombstone")),
                "{err}"
            );
            assert!(theirs.is_empty());
        } else {
            assert_eq!(result.unwrap().generation, 2);
            assert_eq!(theirs.len(), 1);
        }
    }
}

#[tokio::test]
async fn b4_refuses_when_its_own_pack_is_a_tombstone() {
    let t = TestRepo::new().await;
    let repo = t.open().await;
    let prepared = repo
        .backup_prepare(&[src(&t, "src")], opts(1))
        .await
        .unwrap();
    let mut marks = BTreeMap::new();
    for r in index_everything(&repo, &BTreeMap::new()).await {
        marks.insert(
            r.id,
            Status {
                s: status::TOMBSTONE,
                at: 1,
                age: 0,
            },
        );
    }
    let records = index_everything(&repo, &marks).await;
    write_generation(&repo, records, false).await;
    let err = prepared.commit().await.unwrap_err();
    assert!(
        matches!(&err, CoreError::NotCommitted(m) if m.contains("this backup wrote is a tombstone")),
        "{err}"
    );
}

#[tokio::test]
async fn b4_refuses_when_the_generation_went_backwards() {
    let t = TestRepo::new().await;
    let repo = t.open().await;
    let g = write_generation(&repo, Vec::new(), false).await;
    let prepared = repo
        .backup_prepare(&[src(&t, "src")], opts(1))
        .await
        .unwrap();
    for replica in [false, true] {
        t.backend
            .delete(&Key::Head { g, replica }.path())
            .await
            .unwrap();
    }
    let err = prepared.commit().await.unwrap_err();
    assert!(
        matches!(&err, CoreError::Generation(m) if m.contains("went backwards")),
        "{err}"
    );
}

/// B4：沿用了 parent 的子樹時 HEAD parent，不在就停止（§9：子樹裡的 item 不查持有者，靠 parent 一直存在）。
#[tokio::test]
async fn b4_refuses_when_the_parent_of_a_reused_subtree_is_gone() {
    for gone in [true, false] {
        let t = TestRepo::new().await;
        let s = src(&t, "src");
        let repo = t.open().await;
        let first = repo
            .backup(std::slice::from_ref(&s), opts(1))
            .await
            .unwrap();
        let prepared = repo.backup_prepare(&[s], opts(1)).await.unwrap();
        assert!(prepared.report().subtrees >= 1);
        if gone {
            t.backend.delete(&first.snapshot).await.unwrap();
        }
        let result = prepared.commit().await;
        if gone {
            let err = result.unwrap_err();
            assert!(
                matches!(&err, CoreError::NotCommitted(m) if m.contains("parent")),
                "{err}"
            );
        } else {
            assert_eq!(result.unwrap().parent, Some(first.snapshot));
        }
    }
}

/// B4：只從 delta 得知的持有者，commit 前讀它的 trailer：pack 不在了就停止。
#[tokio::test]
async fn b4_refuses_when_a_pack_known_only_from_a_delta_is_gone() {
    let t = TestRepo::new().await;
    let theirs = t.open_as("c2").await;
    theirs.backup(&[src(&t, "two")], opts(2)).await.unwrap();
    let repo = t.open_as("c1").await;
    let prepared = repo
        .backup_prepare(&[src(&t, "one")], opts(1))
        .await
        .unwrap();
    assert!(
        prepared.report().chunks_reused > 0,
        "{:?}",
        prepared.report()
    );
    for k in t.keys(&format!("packs/{}", client(2))).await {
        t.backend.delete(&k).await.unwrap();
    }
    let err = prepared.commit().await.unwrap_err();
    assert!(
        matches!(&err, CoreError::NotCommitted(m) if m.contains("known from a delta")),
        "{err}"
    );
}

/// B4：被入侵的主機寫的 delta 宣稱某個 pack 有 X，pack 在那個位置也真的放著 X 的密文（取回驗證會過），可是
/// pack 自己的 trailer 沒有 X——prune 照 trailer 收錄，X 就沒有持有者。commit 前讀 trailer 擋下。
#[tokio::test]
async fn b4_refuses_when_a_pack_does_not_hold_what_its_delta_claims() {
    let t = TestRepo::new().await;
    let repo = t.open_as("c1").await;
    let content = b"one small chunk".to_vec();
    let keys = repo.keys();
    let x = keys.chunk_id(&content);
    let y = keys.chunk_id(b"something else");
    let sealed = keys
        .seal_chunk(&x, &pack::frame(Algorithm::Raw, &content))
        .unwrap();
    let l = sealed.len() as u64;
    let mut bytes = pack::begin();
    bytes.extend_from_slice(&sealed);
    let trailer = PackTrailer::new(
        Kind::Data,
        vec![PackEntry {
            i: y.into(),
            o: 8,
            l,
            r: content.len() as u64,
        }],
    )
    .encode(8 + l, 64 * 1024)
    .unwrap();
    let bytes = pack::finish(bytes, &keys.seal_trailer(&trailer).unwrap());
    let id = ObjectId::of(&bytes);
    let liar = client(9);
    t.backend
        .put_if_absent(&Key::Pack { client: liar, id }.path(), bytes.clone())
        .await
        .unwrap();
    let delta = Delta::new(
        liar,
        vec![DeltaPack {
            id,
            kind: Kind::Data,
            size: bytes.len() as u64,
            entries: vec![PackEntry {
                i: x.into(),
                o: 8,
                l,
                r: content.len() as u64,
            }],
        }],
    );
    let sealed = keys.seal_delta(&liar, &delta.encode().unwrap()).unwrap();
    t.backend
        .put_if_absent(
            &Key::Delta {
                client: liar,
                id: ObjectId::of(&sealed),
            }
            .path(),
            sealed,
        )
        .await
        .unwrap();

    let dir = t.dir.path().join("one");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("x"), &content).unwrap();
    let prepared = repo.backup_prepare(&[dir], opts(1)).await.unwrap();
    assert_eq!(prepared.report().chunks_reused, 1, "the copy verifies");
    let err = prepared.commit().await.unwrap_err();
    assert!(
        matches!(&err, CoreError::NotCommitted(m) if m.contains("which its delta claims")),
        "{err}"
    );
}

/// 第一次讀數之後每一次都晚 `later`。
fn jumping_clock(later: Duration) -> Clock {
    let t0 = time::OffsetDateTime::now_utc();
    let calls = Arc::new(AtomicUsize::new(0));
    Clock::new(move || {
        let late = calls.fetch_add(1, Ordering::SeqCst) > 0;
        let d = if late { later } else { Duration::ZERO };
        Reading {
            wall: t0 + d,
            boot: d,
        }
    })
}

/// B3–B5：`E ≥ grace − M` 之後不寫 delta，也不 commit；差一點的照常。
#[tokio::test]
async fn a_backup_past_grace_minus_one_hour_writes_no_late_delta_and_does_not_commit() {
    let grace = Duration::from_secs(72 * 3600);
    for (later, commits) in [
        (grace - Duration::from_secs(3600), false),
        (grace - Duration::from_secs(3601), true),
    ] {
        let t = TestRepo::new().await;
        let repo = t.open().await;
        let mut o = opts(1);
        o.clock = jumping_clock(later);
        let result = repo.backup(&[src(&t, "src")], o).await;
        let deltas = t.keys("deltas").await.len();
        if commits {
            result.unwrap();
            assert_eq!(deltas, 1);
        } else {
            let err = result.unwrap_err();
            assert!(
                matches!(&err, CoreError::NotCommitted(m) if m.contains("GC grace")),
                "{err}"
            );
            assert_eq!(deltas, 0, "no delta past grace − M");
            assert!(t.keys("snapshots").await.is_empty());
        }
    }
}

/// 固定在 `t0` 的時鐘：snapshot 的 key 事先算得出來。
fn fixed_clock(t0: time::OffsetDateTime) -> Clock {
    Clock::new(move || Reading {
        wall: t0,
        boot: Duration::ZERO,
    })
}

fn snapshot_key(c: u8, t0: time::OffsetDateTime, plus_ns: i64) -> String {
    let ns = i64::try_from(t0.unix_timestamp_nanos()).unwrap() + plus_ns;
    Key::Snapshot {
        client: client(c),
        ts: SnapTs::from_nanos(ns).unwrap(),
        replica: false,
    }
    .path()
}

/// B5 與 §12：寫主體得到「已存在」，讀回的 bytes 與自己的相同（S3 回 5xx、重試、第一次其實已寫入）→ 就是
/// commit 了，只有一份；不同的 bytes 是撞名 → 時間戳 +1 奈秒。
#[tokio::test]
async fn b5_reads_back_an_existing_snapshot_before_retrying() {
    let t0 = time::OffsetDateTime::now_utc();
    let h = HookedRepo::new(init_options(0)).await;
    let s = h.dir.path().join("src");
    sample_source(&s);
    let repo = h.open_as("c").await;
    let key = snapshot_key(1, t0, 0);
    h.store.echo.lock().unwrap().push(key.clone());
    let mut o = opts(1);
    o.clock = fixed_clock(t0);
    let done = repo.backup(std::slice::from_ref(&s), o).await.unwrap();
    assert_eq!(done.snapshot, key);
    assert_eq!(h.keys("snapshots").await, std::slice::from_ref(&key));

    let t1 = t0 + Duration::from_secs(10);
    let taken = snapshot_key(1, t1, 0);
    h.backend
        .put_if_absent(&taken, b"someone else".to_vec())
        .await
        .unwrap();
    let mut o = opts(1);
    o.clock = fixed_clock(t1);
    let done = repo.backup(&[s], o).await.unwrap();
    assert_eq!(done.snapshot, snapshot_key(1, t1, 1));
    assert_eq!(h.keys("snapshots").await.len(), 3);
}

/// §13：replicas=1 時 mpack 先寫主體再寫 `.r1`（主體遺失才用 `.r1`），snapshot 先寫 `.r1` 再寫主體（主體
/// 是 commit）。
#[tokio::test]
async fn replicas_one_writes_copies_in_the_spec_order() {
    let h = HookedRepo::new(init_options(1)).await;
    let s = h.dir.path().join("src");
    sample_source(&s);
    let repo = h.open_as("c").await;
    h.store.reset();
    let done = repo.backup(&[s], opts(1)).await.unwrap();
    let puts = h.store.puts();
    let at = |k: &str| {
        puts.iter()
            .position(|p| p == k)
            .unwrap_or_else(|| panic!("{k} not written"))
    };
    let mpacks: Vec<&String> = puts
        .iter()
        .filter(|p| p.starts_with("mpacks/") && !p.ends_with(".r1"))
        .collect();
    assert!(!mpacks.is_empty());
    for m in mpacks {
        assert!(at(m) < at(&format!("{m}.r1")), "{m}");
    }
    assert!(at(&format!("{}.r1", done.snapshot)) < at(&done.snapshot));
    assert_eq!(
        puts.last(),
        Some(&done.snapshot),
        "the body is the last write"
    );
}

/// B4：`g_c ≠ g_s` 時，沿用的 ID 必須還在持有者的記錄裡（P7c、P7d 會移除被證明不實的項目）。
#[tokio::test]
async fn b4_refuses_when_the_holder_no_longer_lists_a_reused_id() {
    let t = TestRepo::new().await;
    let repo = t.open_as("c1").await;
    repo.backup(&[src(&t, "one")], opts(1)).await.unwrap();
    let records = index_everything(&repo, &BTreeMap::new()).await;
    write_generation(&repo, records, false).await;
    let other = t.open_as("c2").await;
    let prepared = other
        .backup_prepare(&[src(&t, "two")], opts(2))
        .await
        .unwrap();
    assert!(prepared.report().chunks_reused > 0);
    // 下一代：client 1 的資料 pack 的記錄各少了第一項
    let mut records = index_everything(&repo, &BTreeMap::new()).await;
    for r in records
        .iter_mut()
        .filter(|r| r.kind == Kind::Data && r.client == client(1))
    {
        r.entries.remove(0);
    }
    write_generation(&repo, records, false).await;
    let err = prepared.commit().await.unwrap_err();
    assert!(
        matches!(&err, CoreError::NotCommitted(m) if m.contains("no longer holds")),
        "{err}"
    );
}

/// 第 `n` 次讀數起晚 `later`（前面的讀數是 `t0`）。讀數的順序：B1 的開始、最後一份 delta、B4、B5（主體之前）。
fn clock_jumping_at(n: usize, later: Duration) -> Clock {
    let t0 = time::OffsetDateTime::now_utc();
    let calls = Arc::new(AtomicUsize::new(0));
    Clock::new(move || {
        let late = calls.fetch_add(1, Ordering::SeqCst) + 1 >= n;
        let d = if late { later } else { Duration::ZERO };
        Reading {
            wall: t0 + d,
            boot: d,
        }
    })
}

/// B5：B4 之後才超過 `grace − M`，寫主體之前再量一次 E，照樣不 commit。
#[tokio::test]
async fn b5_checks_the_elapsed_time_again_before_the_body() {
    let t = TestRepo::new().await;
    let repo = t.open().await;
    let mut o = opts(1);
    o.clock = clock_jumping_at(4, Duration::from_secs(71 * 3600));
    let err = repo.backup(&[src(&t, "src")], o).await.unwrap_err();
    assert!(
        matches!(&err, CoreError::NotCommitted(m) if m.starts_with("B5")),
        "{err}"
    );
    assert!(t.keys("snapshots").await.is_empty());
}
