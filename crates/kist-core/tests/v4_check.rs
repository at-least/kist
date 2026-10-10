//! v4 的 check（format-v4.md §13.1）：每一條規則一個測試；`--repair` 只做只在不存在時建立的修復，損壞的物件
//! 有驗證過的完好 bytes 時列在 repairable、不寫回（v4 不覆寫）。

#[path = "v4_common/mod.rs"]
mod v4_common;

use std::collections::BTreeMap;
use std::sync::Arc;

use kist_core::v4::check::{CheckOptions, CheckReport};
use kist_core::v4::pack::PackWriter;
use kist_core::v4::Repo;
use kist_core::CoreError;
use kist_format::v4::ids::{ChunkId, ClientId, ItemId, ObjectId};
use kist_format::v4::index::{status, Head, Status};
use kist_format::v4::keys::Key;
use kist_format::v4::meta::{meta_kind, node_type, Entry, Item, Seg};
use kist_format::v4::pack::{Kind, PackEntry};
use kist_format::v4::snapshot::Root;
use v4_common::*;

const PLAIN: CheckOptions = CheckOptions {
    read_data: false,
    repair: false,
};
const READ: CheckOptions = CheckOptions {
    read_data: true,
    repair: false,
};
const REPAIR: CheckOptions = CheckOptions {
    read_data: true,
    repair: true,
};

/// 一個備份過、維護角色收錄過一次（世代 1）的 repo。
async fn indexed(replicas: u8) -> (TestRepo, Repo, String) {
    let t = TestRepo::with(init_options(replicas)).await;
    let repo = t.open().await;
    let src = t.dir.path().join("src");
    sample_source(&src);
    let s = repo
        .backup(std::slice::from_ref(&src), opts(1))
        .await
        .unwrap();
    let records = index_everything(&repo, &BTreeMap::new()).await;
    write_generation(&repo, records, false).await;
    (t, repo, s.snapshot)
}

fn has(list: &[String], needle: &str) -> bool {
    list.iter().any(|e| e.contains(needle))
}

fn clean(r: &CheckReport) {
    assert!(r.errors.is_empty(), "errors: {:#?}", r.errors);
    assert!(r.warnings.is_empty(), "warnings: {:#?}", r.warnings);
}

async fn flip(repo: &Repo, path: &str, at: usize) {
    let mut bytes = repo.backend().get(path).await.unwrap();
    let i = at.min(bytes.len() - 1);
    bytes[i] ^= 1;
    repo.backend().put(path, bytes).await.unwrap();
}

/// client 前綴下的第一個 key（`prefix` 是 `packs`、`mpacks` 之類）。
async fn first(t: &TestRepo, prefix: &str, c: u8) -> String {
    t.keys(&format!("{prefix}/{}", client(c)))
        .await
        .into_iter()
        .find(|k| !k.ends_with(".r1"))
        .unwrap()
}

#[tokio::test]
async fn a_healthy_repository_checks_clean() {
    let (_t, repo, _) = indexed(1).await;
    let r = repo.check(PLAIN).await.unwrap();
    clean(&r);
    assert_eq!(r.generation, 1);
    assert_eq!(r.snapshots, 1);
    assert!(r.items > 0 && r.chunks > 0 && r.packs > 0, "{r:?}");
    clean(&repo.check(READ).await.unwrap());
}

// —— head ——

#[tokio::test]
async fn a_missing_head_r1_is_a_warning_and_repair_creates_it() {
    let (t, repo, _) = indexed(0).await;
    let r1 = Key::Head {
        g: 0,
        replica: true,
    }
    .path();
    let body = repo
        .backend()
        .get(
            &Key::Head {
                g: 0,
                replica: false,
            }
            .path(),
        )
        .await
        .unwrap();
    t.backend.delete(&r1).await.unwrap();
    let r = repo.check(PLAIN).await.unwrap();
    assert!(r.errors.is_empty(), "{r:?}");
    assert!(has(&r.warnings, "has no .r1"), "{r:?}");
    assert!(r.repaired.is_empty(), "not without --repair");
    let r = repo.check(REPAIR).await.unwrap();
    assert!(has(&r.repaired, &r1), "{r:?}");
    assert_eq!(repo.backend().get(&r1).await.unwrap(), body);
    clean(&repo.check(PLAIN).await.unwrap());
}

#[tokio::test]
async fn a_lost_head_body_is_rebuilt_from_its_r1() {
    let (t, repo, _) = indexed(0).await;
    let body = Key::Head {
        g: 0,
        replica: false,
    }
    .path();
    t.backend.delete(&body).await.unwrap();
    let r = repo.check(PLAIN).await.unwrap();
    assert!(has(&r.errors, "missing; its .r1 is intact"), "{r:?}");
    let r = repo.check(REPAIR).await.unwrap();
    assert!(has(&r.repaired, &body), "{r:?}");
    clean(&repo.check(PLAIN).await.unwrap());
}

#[tokio::test]
async fn a_head_whose_parent_hash_does_not_match_is_reported() {
    let (_t, repo, _) = indexed(0).await;
    let base = repo.read_head(1).await.unwrap();
    let head = Head {
        g: 2,
        parent: Some(ObjectId::from_bytes([9; 32])),
        ..base.head
    };
    let sealed = repo.keys().seal_head(2, &head.encode().unwrap()).unwrap();
    for replica in [false, true] {
        repo.backend()
            .put_if_absent(&Key::Head { g: 2, replica }.path(), sealed.clone())
            .await
            .unwrap();
    }
    let r = repo.check(PLAIN).await.unwrap();
    assert!(
        has(&r.errors, "parent hash does not match generation 1"),
        "{r:?}"
    );
}

#[tokio::test]
async fn a_missing_generation_is_reported() {
    let (t, repo, _) = indexed(0).await;
    write_generation(
        &repo,
        index_everything(&repo, &BTreeMap::new()).await,
        false,
    )
    .await;
    for replica in [false, true] {
        t.backend
            .delete(&Key::Head { g: 1, replica }.path())
            .await
            .unwrap();
    }
    let r = repo.check(PLAIN).await.unwrap();
    assert!(has(&r.errors, "generations 1..=1 are missing"), "{r:?}");
}

#[tokio::test]
async fn an_unreadable_newest_generation_is_reported_and_readers_fall_back() {
    let (_t, repo, _) = indexed(0).await;
    for replica in [false, true] {
        flip(&repo, &Key::Head { g: 1, replica }.path(), 50).await;
    }
    let r = repo.check(PLAIN).await.unwrap();
    assert!(has(&r.errors, "generation 1 is unreadable"), "{r:?}");
    assert!(has(&r.errors, "readers fall back"), "{r:?}");
    assert_eq!(r.generation, 0);
}

// —— map 與記錄 ——

#[tokio::test]
async fn a_missing_data_pack_is_data_loss() {
    let (t, repo, _) = indexed(0).await;
    let pack = first(&t, "packs", 1).await;
    t.backend.delete(&pack).await.unwrap();
    let r = repo.check(READ).await.unwrap();
    assert!(
        has(&r.errors, &format!("{pack}: missing (data loss)")),
        "{r:?}"
    );
    assert!(has(&r.unrepairable, &pack), "{r:?}");
}

#[tokio::test]
async fn a_lost_mpack_body_is_rebuilt_from_its_r1() {
    let (t, repo, _) = indexed(1).await;
    let mpack = first(&t, "mpacks", 1).await;
    t.backend.delete(&mpack).await.unwrap();
    let r = repo.check(PLAIN).await.unwrap();
    assert!(
        has(&r.errors, &format!("{mpack}: missing; its .r1 is intact")),
        "{r:?}"
    );
    let r = repo.check(REPAIR).await.unwrap();
    assert!(has(&r.repaired, &mpack), "{r:?}");
    clean(&repo.check(READ).await.unwrap());
}

#[tokio::test]
async fn a_missing_mpack_r1_is_repaired_when_replicas_is_one() {
    let (t, repo, _) = indexed(1).await;
    let mpack = first(&t, "mpacks", 1).await;
    t.backend.delete(&format!("{mpack}.r1")).await.unwrap();
    let r = repo.check(PLAIN).await.unwrap();
    assert!(has(&r.warnings, &format!("{mpack}: has no .r1")), "{r:?}");
    repo.check(REPAIR).await.unwrap();
    clean(&repo.check(READ).await.unwrap());
}

#[tokio::test]
async fn a_damaged_shard_body_is_repairable_but_not_rewritten() {
    let (t, repo, _) = indexed(1).await;
    let gen = repo.read_head(1).await.unwrap();
    let id = gen.head.shards.iter().flatten().next().copied().unwrap();
    let body = Key::Shard { id, replica: false }.path();
    flip(&repo, &body, 40).await;
    let damaged = repo.backend().get(&body).await.unwrap();
    let r = repo.check(REPAIR).await.unwrap();
    assert!(
        has(
            &r.errors,
            &format!("{body}: content does not match its name")
        ),
        "{r:?}"
    );
    assert!(has(&r.repairable, &body), "{r:?}");
    assert_eq!(
        t.backend.get(&body).await.unwrap(),
        damaged,
        "v4 never overwrites an object"
    );
}

#[tokio::test]
async fn packs_in_no_record_or_delta_and_orphans_are_warnings() {
    let (t, repo, _) = indexed(0).await;
    // 沒有 delta 的 pack（中斷的 backup）
    let keys = Arc::clone(repo.keys());
    let mut w = PackWriter::new(Arc::clone(&keys), Kind::Data, 64 * 1024, 64 * 1024);
    let lone = data("lone", 2000);
    w.add(keys.chunk_id(&lone).into(), &lone).unwrap();
    let p = w.finish().unwrap().unwrap();
    let lone_key = Key::Pack {
        client: client(1),
        id: p.id,
    }
    .path();
    repo.backend()
        .put_if_absent(&lone_key, p.bytes)
        .await
        .unwrap();
    // pack 已不在的 parity；解不開的 delta
    let orphan = Key::Parity {
        client: client(1),
        id: ObjectId::from_bytes([7; 32]),
    }
    .path();
    repo.backend()
        .put_if_absent(&orphan, b"x".to_vec())
        .await
        .unwrap();
    let junk = b"not a delta".to_vec();
    let bad_delta = Key::Delta {
        client: client(1),
        id: ObjectId::of(&junk),
    }
    .path();
    repo.backend()
        .put_if_absent(&bad_delta, junk)
        .await
        .unwrap();
    let r = repo.check(PLAIN).await.unwrap();
    assert!(r.errors.is_empty(), "{r:?}");
    assert!(
        has(
            &r.warnings,
            &format!("{lone_key}: in no map record and no delta")
        ),
        "{r:?}"
    );
    assert!(
        has(&r.warnings, &format!("{orphan}: its pack is gone")),
        "{r:?}"
    );
    assert!(has(&r.warnings, &format!("{bad_delta}: ")), "{r:?}");
    drop(t);
}

#[tokio::test]
async fn unreadable_and_unknown_records_are_reported() {
    let t = TestRepo::new().await;
    let repo = t.open().await;
    let src = t.dir.path().join("src");
    sample_source(&src);
    repo.backup(std::slice::from_ref(&src), opts(1))
        .await
        .unwrap();
    // 兩個沒人引用的 pack
    let mut ids = Vec::new();
    for seed in ["u1", "u2"] {
        let keys = Arc::clone(repo.keys());
        let mut w = PackWriter::new(Arc::clone(&keys), Kind::Data, 64 * 1024, 64 * 1024);
        let c = data(seed, 2000);
        w.add(keys.chunk_id(&c).into(), &c).unwrap();
        let p = w.finish().unwrap().unwrap();
        repo.backend()
            .put_if_absent(
                &Key::Pack {
                    client: ClientId::MAINT,
                    id: p.id,
                }
                .path(),
                p.bytes,
            )
            .await
            .unwrap();
        ids.push(p.id);
    }
    let status = BTreeMap::from([
        (
            ids[0],
            Status {
                s: status::UNREADABLE,
                at: 0,
                age: 0,
            },
        ),
        (
            ids[1],
            Status {
                s: status::UNKNOWN,
                at: 0,
                age: 0,
            },
        ),
    ]);
    write_generation(&repo, index_everything(&repo, &status).await, false).await;
    let r = repo.check(PLAIN).await.unwrap();
    assert!(r.errors.is_empty(), "{r:?}");
    assert!(has(&r.warnings, "unreadable (nobody uses it"), "{r:?}");
    assert!(has(&r.warnings, "newer format version"), "{r:?}");
}

#[tokio::test]
async fn a_record_size_that_does_not_match_the_listing_is_reported() {
    let t = TestRepo::new().await;
    let repo = t.open().await;
    let src = t.dir.path().join("src");
    sample_source(&src);
    repo.backup(std::slice::from_ref(&src), opts(1))
        .await
        .unwrap();
    let mut records = index_everything(&repo, &BTreeMap::new()).await;
    let r0 = records.iter_mut().find(|r| r.kind == Kind::Data).unwrap();
    r0.size += 1;
    let path = Key::Pack {
        client: r0.client,
        id: r0.id,
    }
    .path();
    write_generation(&repo, records, false).await;
    let r = repo.check(PLAIN).await.unwrap();
    assert!(
        has(&r.errors, &format!("{path}: ")) && has(&r.errors, "but its record says"),
        "{r:?}"
    );
}

// —— snapshot ——

/// P5：被引用的 ID 只剩 tombstone 的持有者 → prune 會刪掉它，是錯誤。
#[tokio::test]
async fn an_item_held_only_by_a_tombstone_is_an_error() {
    let t = TestRepo::new().await;
    let repo = t.open().await;
    let src = t.dir.path().join("src");
    sample_source(&src);
    repo.backup(std::slice::from_ref(&src), opts(1))
        .await
        .unwrap();
    let pack = first(&t, "packs", 1).await;
    let Key::Pack { id, .. } = Key::parse(&pack).unwrap() else {
        unreachable!()
    };
    let status = BTreeMap::from([(
        id,
        Status {
            s: status::TOMBSTONE,
            at: 1,
            age: 0,
        },
    )]);
    write_generation(&repo, index_everything(&repo, &status).await, false).await;
    let r = repo.check(PLAIN).await.unwrap();
    assert!(has(&r.errors, "held only by tombstoned packs"), "{r:?}");
}

/// P5：被引用的 metadata item 沒有任何持有者（目錄的 tree 不在任何 mpack 裡）。
#[tokio::test]
async fn an_item_with_no_holder_is_an_error() {
    let t = TestRepo::new().await;
    let repo = t.open().await;
    let lost = repo
        .keys()
        .meta_id(&Item::Seg(Seg::new(Vec::new())).encode().unwrap());
    let mut d = Entry::new(b"lost".to_vec(), node_type::DIR, meta_kind::GENERIC);
    d.tree = Some(lost);
    let seg = Item::Seg(Seg::new(vec![d]));
    let root = Root {
        path: b"/hand".to_vec(),
        t: node_type::DIR,
        tree: Some(repo.keys().meta_id(&seg.encode().unwrap())),
        entry: None,
    };
    hand_snapshot(&repo, client(1), hours_from_now(1), vec![root], vec![seg]).await;
    let r = repo.check(PLAIN).await.unwrap();
    assert!(
        has(&r.errors, &format!("/hand/lost: {lost} has no holder")),
        "{r:?}"
    );
}

/// mpack 的主體壞了、`.r1` 完好：讀取端讀 `.r1`（不讀資料的 check 不報錯）；`--read-data` 回報主體，列在
/// repairable、不寫回。
#[tokio::test]
async fn a_damaged_mpack_body_with_an_intact_r1_is_repairable() {
    let (t, repo, _) = indexed(1).await;
    let mpack = first(&t, "mpacks", 1).await;
    flip(&repo, &mpack, 40).await;
    let damaged = t.backend.get(&mpack).await.unwrap();
    clean(&repo.check(PLAIN).await.unwrap());
    let r = repo.check(REPAIR).await.unwrap();
    assert!(
        has(
            &r.errors,
            &format!("{mpack}: content does not match its name")
        ),
        "{r:?}"
    );
    assert!(
        has(&r.repairable, &format!("{mpack}: its .r1 is intact")),
        "{r:?}"
    );
    assert_eq!(t.backend.get(&mpack).await.unwrap(), damaged);
}

/// 讀 item 時排第一的持有者驗證失敗、下一個讀得到：讀取端照常讀，check 照樣回報壞的那一份。
#[tokio::test]
async fn a_holder_that_fails_verification_is_reported_even_when_another_works() {
    let t = TestRepo::new().await;
    let repo = t.open().await;
    let src = t.dir.path().join("src");
    sample_source(&src);
    let s = repo
        .backup(std::slice::from_ref(&src), opts(1))
        .await
        .unwrap();
    let first_records = index_everything(&repo, &BTreeMap::new()).await;
    write_generation(&repo, first_records.clone(), false).await;
    // 維護角色的 mpack 也放一份 roots 的 item；client 1 的記錄 since 較小，排第一
    let snap = repo.read_snapshot(&skey(&s.snapshot)).await.unwrap();
    let reader = repo.reader(None).await.unwrap();
    let cbor = reader.item(&snap.roots).await.unwrap().encode().unwrap();
    let keys = Arc::clone(repo.keys());
    let mut w = PackWriter::new(Arc::clone(&keys), Kind::Meta, 64 * 1024, 64 * 1024);
    w.add(snap.roots.into(), &cbor).unwrap();
    let p = w.finish().unwrap().unwrap();
    repo.backend()
        .put_if_absent(
            &Key::Mpack {
                client: ClientId::MAINT,
                id: p.id,
                replica: false,
            }
            .path(),
            p.bytes,
        )
        .await
        .unwrap();
    let mut records = index_everything(&repo, &BTreeMap::new()).await;
    for r in &mut records {
        if let Some(old) = first_records.iter().find(|o| o.id == r.id) {
            r.since = old.since;
        }
    }
    write_generation(&repo, records, false).await;
    let mine = first(&t, "mpacks", 1).await;
    flip(&repo, &mine, 40).await;
    let r = repo.check(PLAIN).await.unwrap();
    assert!(
        has(
            &r.errors,
            &format!("the copy in {mine} failed verification")
        ),
        "{r:?}"
    );
}

#[tokio::test]
async fn a_snapshot_with_only_an_r1_is_a_warning_and_still_walked() {
    let (t, repo, snap) = indexed(1).await;
    t.backend.delete(&snap).await.unwrap();
    let pack = first(&t, "packs", 1).await;
    t.backend.delete(&pack).await.unwrap();
    let r = repo.check(PLAIN).await.unwrap();
    assert!(
        has(&r.warnings, &format!("{snap}: only its .r1 exists")),
        "{r:?}"
    );
    assert!(has(&r.errors, "missing (data loss)"), "{r:?}");
    assert_eq!(r.snapshots, 1);
}

#[tokio::test]
async fn a_missing_snapshot_r1_is_repaired_when_replicas_is_one() {
    let (t, repo, snap) = indexed(1).await;
    let r1 = format!("{snap}.r1");
    t.backend.delete(&r1).await.unwrap();
    let r = repo.check(PLAIN).await.unwrap();
    assert!(has(&r.warnings, &format!("{snap}: has no .r1")), "{r:?}");
    let r = repo.check(REPAIR).await.unwrap();
    assert!(has(&r.repaired, &r1), "{r:?}");
    assert_eq!(
        t.backend.get(&r1).await.unwrap(),
        t.backend.get(&snap).await.unwrap()
    );
}

#[tokio::test]
async fn a_damaged_snapshot_body_with_an_intact_r1_is_repairable() {
    let (_t, repo, snap) = indexed(1).await;
    flip(&repo, &snap, 30).await;
    let r = repo.check(REPAIR).await.unwrap();
    assert!(has(&r.errors, &format!("{snap}: ")), "{r:?}");
    assert!(
        has(&r.repairable, &format!("{snap}: its .r1 is intact")),
        "{r:?}"
    );
}

/// 惡意 repo 的目錄鏈超過深度上限：check 回報、不深入，不是 stack overflow；上限前一層是誠實的深度，沒有錯。
#[test]
fn too_deep_a_directory_chain_is_reported_not_crashed() {
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(deep_body())
        })
        .unwrap()
        .join()
        .unwrap();
}

async fn deep_body() {
    let t = TestRepo::new().await;
    let repo = t.open().await;
    let chain = |depth: usize, leaf: &[u8]| {
        let mut items = vec![Item::Seg(Seg::new(vec![Entry::new(
            leaf.to_vec(),
            node_type::FILE,
            meta_kind::GENERIC,
        )]))];
        for _ in 0..depth {
            let mut d = Entry::new(b"d".to_vec(), node_type::DIR, meta_kind::GENERIC);
            d.tree = Some(
                repo.keys()
                    .meta_id(&items.last().unwrap().encode().unwrap()),
            );
            items.push(Item::Seg(Seg::new(vec![d])));
        }
        items
    };
    let root = |path: &[u8], items: &[Item]| Root {
        path: path.to_vec(),
        t: node_type::DIR,
        tree: Some(
            repo.keys()
                .meta_id(&items.last().unwrap().encode().unwrap()),
        ),
        entry: None,
    };
    let ok = chain(kist_core::MAX_TREE_DEPTH - 1, b"ok");
    hand_snapshot(
        &repo,
        client(1),
        hours_from_now(1),
        vec![root(b"/ok", &ok)],
        ok,
    )
    .await;
    let r = repo.check(PLAIN).await.unwrap();
    assert!(r.errors.is_empty(), "上限前一層是誠實深度：{:?}", r.errors);
    let deep = chain(4_200, b"deep");
    hand_snapshot(
        &repo,
        client(1),
        hours_from_now(2),
        vec![root(b"/deep", &deep)],
        deep,
    )
    .await;
    let r = repo.check(PLAIN).await.unwrap();
    let msg = format!("nesting deeper than {}", kist_core::MAX_TREE_DEPTH);
    assert!(has(&r.errors, &msg), "{:?}", r.errors);
}

/// §7：檔案的 size 必須等於它的 chunk 的合計。
#[tokio::test]
async fn a_file_whose_size_does_not_match_its_chunks_is_reported() {
    let t = TestRepo::new().await;
    let repo = t.open().await;
    let src = t.dir.path().join("chunks");
    std::fs::create_dir_all(&src).unwrap();
    let payload = data("payload", 1000);
    std::fs::write(src.join("p"), &payload).unwrap();
    repo.backup(&[src], opts(1)).await.unwrap();
    let mut f = Entry::new(b"liar".to_vec(), node_type::FILE, meta_kind::GENERIC);
    f.size = payload.len() as u64 + 1;
    f.chunks = vec![ItemId::from(repo.keys().chunk_id(&payload))];
    let seg = Item::Seg(Seg::new(vec![f]));
    let root = Root {
        path: b"/hand".to_vec(),
        t: node_type::DIR,
        tree: Some(repo.keys().meta_id(&seg.encode().unwrap())),
        entry: None,
    };
    hand_snapshot(&repo, client(1), hours_from_now(1), vec![root], vec![seg]).await;
    let r = repo.check(PLAIN).await.unwrap();
    assert!(
        has(&r.errors, "/hand/liar") && has(&r.errors, "the entry says"),
        "{r:?}"
    );
}

// —— 讀資料 ——

/// 損壞的資料 pack：有 parity 而修得回（重算 BLAKE3 等於名稱）是 repairable，沒有 parity 是 unrepairable。都不寫回。
#[tokio::test]
async fn read_data_finds_a_damaged_pack_and_proves_the_parity_repair() {
    for parity in [2u8, 0] {
        let t = TestRepo::new().await;
        let repo = t.open().await;
        let src = t.dir.path().join("src");
        sample_source(&src);
        let mut o = opts(1);
        o.parity = parity;
        repo.backup(std::slice::from_ref(&src), o).await.unwrap();
        let pack = first(&t, "packs", 1).await;
        flip(&repo, &pack, 200).await;
        let damaged = t.backend.get(&pack).await.unwrap();
        let plain = repo.check(PLAIN).await.unwrap();
        assert!(
            !has(&plain.errors, "content does not match its name"),
            "data packs are not read without --read-data: {plain:?}"
        );
        let r = repo.check(REPAIR).await.unwrap();
        assert!(
            has(
                &r.errors,
                &format!("{pack}: content does not match its name")
            ),
            "{r:?}"
        );
        if parity > 0 {
            assert!(
                has(&r.repairable, &format!("{pack}: its parity repairs it")),
                "{r:?}"
            );
        } else {
            assert!(has(&r.unrepairable, &pack), "{r:?}");
        }
        assert_eq!(t.backend.get(&pack).await.unwrap(), damaged);
    }
}

/// 記錄 ⊆ trailer：記錄（或 delta）列出、trailer 沒有的項目，或位置不同，都是錯誤。
#[tokio::test]
async fn a_record_must_be_a_subset_of_the_trailer() {
    let t = TestRepo::new().await;
    let repo = t.open().await;
    let src = t.dir.path().join("src");
    sample_source(&src);
    repo.backup(std::slice::from_ref(&src), opts(1))
        .await
        .unwrap();
    let mut records = index_everything(&repo, &BTreeMap::new()).await;
    let r0 = records.iter_mut().find(|r| r.kind == Kind::Data).unwrap();
    let path = Key::Pack {
        client: r0.client,
        id: r0.id,
    }
    .path();
    let ghost = ItemId::from(ChunkId::from_bytes([0xEE; 32]));
    let last = *r0.entries.last().unwrap();
    r0.entries.push(PackEntry { i: ghost, ..last });
    r0.entries[0].r += 1;
    write_generation(&repo, records, false).await;
    let plain = repo.check(PLAIN).await.unwrap();
    assert!(
        !has(&plain.errors, "trailer"),
        "only --read-data reads trailers: {plain:?}"
    );
    let r = repo.check(READ).await.unwrap();
    assert!(
        has(
            &r.errors,
            &format!("{path}: its record lists {ghost} but the trailer does not")
        ),
        "{r:?}"
    );
    assert!(
        has(&r.errors, "places") && has(&r.errors, "differently from the trailer"),
        "{r:?}"
    );
}

#[tokio::test]
async fn repair_is_refused_on_a_backend_without_atomic_create() {
    let store = Arc::new(object_store::memory::InMemory::new());
    let backend = kist_backend::Backend::from_store(
        store,
        kist_backend::RepoLocation::Rclone(kist_backend::sftp::RcloneConfig {
            remote: String::new(),
            path: "repo".to_owned(),
        }),
    );
    let repo = Repo::init(backend, PASSWORD, init_options(0))
        .await
        .unwrap();
    let err = repo.check(REPAIR).await.unwrap_err();
    assert!(matches!(err, CoreError::Usage(_)), "{err}");
    clean(&repo.check(READ).await.unwrap());
}
