//! v4 的讀取端（format-v4.md §10）：持有者依序嘗試、驗證失敗換下一個、tombstone 排最後；restore、mount、check
//! 在持有者全部失敗時重新載入一次（限流）、最大世代讀不了時退回較舊的世代；snapshot 主體讀不了讀 `.r1`；
//! 目錄依 Dir 的索引查找。

#[path = "v4_common/mod.rs"]
mod v4_common;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use kist_core::v4::live::Live;
use kist_core::v4::pack::PackWriter;
use kist_core::v4::Repo;
use kist_core::CoreError;
use kist_format::v4::ids::{ChunkId, ClientId, ItemId, ObjectId};
use kist_format::v4::index::{status, Status};
use kist_format::v4::keys::Key;
use kist_format::v4::meta::Item;
use kist_format::v4::pack::Kind;
use v4_common::*;

/// 一個檔案、一個 chunk（小於最小 chunk）的來源，備份成 client `c` 的 snapshot。回傳 chunk 的內容與 ID。
async fn one_chunk_backup(
    repo: &Repo,
    dir: &std::path::Path,
    seed: &str,
    c: u8,
) -> (Vec<u8>, ChunkId) {
    let src = dir.join(format!("src-{seed}"));
    std::fs::create_dir_all(&src).unwrap();
    let content = data(seed, 3000);
    std::fs::write(src.join("f"), &content).unwrap();
    repo.backup(&[src], opts(c)).await.unwrap();
    let id = repo.keys().chunk_id(&content);
    (content, id)
}

/// 扮演 prune 的 repack：把 `chunks` 寫進全零 client 前綴下的一個新資料 pack。
async fn maint_pack(repo: &Repo, chunks: &[&[u8]]) -> ObjectId {
    let keys = Arc::clone(repo.keys());
    let mut w = PackWriter::new(Arc::clone(&keys), Kind::Data, 64 * 1024, 64 * 1024);
    for c in chunks {
        w.add(keys.chunk_id(c).into(), c).unwrap();
    }
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
    p.id
}

/// `client` 前綴下唯一的資料 pack。
async fn only_pack(repo: &Repo, c: ClientId) -> Key {
    let keys: Vec<Key> = repo
        .backend()
        .list(&format!("packs/{c}"))
        .await
        .unwrap()
        .into_iter()
        .map(|o| Key::parse(&o.key).unwrap())
        .collect();
    assert_eq!(keys.len(), 1, "{keys:?}");
    keys.into_iter().next().unwrap()
}

/// 翻掉 pack 裡每一項密文的一個 byte（名稱不變）。
async fn damage(repo: &Repo, key: &Key) {
    let reader = repo.reader(None).await.unwrap();
    let trailer = reader.trailer(key).await.unwrap();
    let mut bytes = repo.backend().get(&key.path()).await.unwrap();
    for e in &trailer.entries {
        bytes[(e.o + 30) as usize] ^= 1;
    }
    repo.backend().put(&key.path(), bytes).await.unwrap();
}

fn tombstone() -> Status {
    Status {
        s: status::TOMBSTONE,
        at: 1,
        age: 0,
    }
}

fn order(repo_reader: &kist_core::v4::read::Reader, id: &ChunkId) -> Vec<ClientId> {
    repo_reader
        .state
        .read_candidates(&ItemId::from(*id), Kind::Data)
        .iter()
        .map(|h| h.client)
        .collect()
}

/// §10：某一份驗證失敗就試下一份。client 1 的副本排第一（since 較小）但壞了，讀到的是 prune 的副本。
#[tokio::test]
async fn a_holder_that_fails_verification_falls_through_to_the_next() {
    let t = TestRepo::new().await;
    let repo = t.open().await;
    let (content, id) = one_chunk_backup(&repo, t.dir.path(), "x", 1).await;
    let first = index_everything(&repo, &BTreeMap::new()).await;
    write_generation(&repo, first.clone(), false).await;
    maint_pack(&repo, &[&content]).await;
    // client 1 的記錄沿用上一代的（since 較小），新的 pack 從這一代進 map
    let mut records = index_everything(&repo, &BTreeMap::new()).await;
    for r in &mut records {
        if let Some(old) = first.iter().find(|o| o.id == r.id) {
            r.since = old.since;
        }
    }
    write_generation(&repo, records, false).await;

    let reader = repo.reader(None).await.unwrap();
    assert_eq!(order(&reader, &id), [client(1), ClientId::MAINT]);
    damage(&repo, &only_pack(&repo, client(1)).await).await;
    let reader = repo.reader(None).await.unwrap();
    assert_eq!(reader.chunk(&id).await.unwrap(), content);
}

/// §10：tombstone 的持有者排最後，只在其他都失敗時讀。
#[tokio::test]
async fn a_tombstoned_holder_is_read_only_when_every_other_holder_fails() {
    let h = HookedRepo::new(init_options(0)).await;
    let repo = h.open_as("c").await;
    let (content, id) = one_chunk_backup(&repo, h.dir.path(), "x", 1).await;
    let maint = maint_pack(&repo, &[&content]).await;
    let mine = only_pack(&repo, client(1)).await;
    let Key::Pack { id: mine_id, .. } = mine else {
        unreachable!()
    };
    let records = index_everything(&repo, &BTreeMap::from([(mine_id, tombstone())])).await;
    write_generation(&repo, records, false).await;

    let reader = repo.reader(None).await.unwrap();
    assert_eq!(order(&reader, &id), [ClientId::MAINT, client(1)]);
    h.store.reset();
    assert_eq!(reader.chunk(&id).await.unwrap(), content);
    assert_eq!(
        h.store.ranged(),
        [Key::Pack {
            client: ClientId::MAINT,
            id: maint
        }
        .path()],
        "the tombstoned pack is not read while a live holder works"
    );

    damage(
        &repo,
        &Key::Pack {
            client: ClientId::MAINT,
            id: maint,
        },
    )
    .await;
    let reader = repo.reader(None).await.unwrap();
    assert_eq!(reader.chunk(&id).await.unwrap(), content, "last resort");
}

/// 開始得比 prune 早的讀取端：item 被搬進 prune 的新 pack、記在下一代，舊的 pack 被刪。持有者全部失敗 →
/// 重新載入一次 → 讀到新的 pack。
#[tokio::test]
async fn a_reader_that_started_before_a_repack_reloads_once() {
    let t = TestRepo::new().await;
    let repo = t.open().await;
    let (content, id) = one_chunk_backup(&repo, t.dir.path(), "x", 1).await;
    write_generation(
        &repo,
        index_everything(&repo, &BTreeMap::new()).await,
        false,
    )
    .await;
    let live = repo.live().await.unwrap();
    assert_eq!(live.reader().state.gen.g, 1);

    // prune：新的 pack 進 map，舊的成為 tombstone，然後被刪
    maint_pack(&repo, &[&content]).await;
    let old = only_pack(&repo, client(1)).await;
    let Key::Pack { id: old_id, .. } = old else {
        unreachable!()
    };
    let records = index_everything(&repo, &BTreeMap::from([(old_id, tombstone())])).await;
    write_generation(&repo, records, false).await;
    repo.backend().delete(&old.path()).await.unwrap();

    let got = live.run(async |r| r.chunk(&id).await).await.unwrap();
    assert_eq!(got, content);
    let (epoch, reader) = live.current();
    assert_eq!((epoch, reader.state.gen.g), (1, 2));
}

/// 重新載入限流：同一個 interval 之內第二次全部失敗不再重新列出，直接回錯誤。
#[tokio::test]
async fn reloading_is_throttled() {
    let h = HookedRepo::new(init_options(0)).await;
    let repo = h.open_as("c").await;
    let (_, x) = one_chunk_backup(&repo, h.dir.path(), "x", 1).await;
    let pack = only_pack(&repo, client(1)).await;
    repo.backend().delete(&pack.path()).await.unwrap();

    let (state, _) = repo.usable_state().await.unwrap();
    let live = Live::new(repo.clone(), state, Duration::from_secs(3600));
    let err = live.run(async |r| r.chunk(&x).await).await.unwrap_err();
    assert!(matches!(err, CoreError::Unavailable { .. }), "{err}");
    assert_eq!(live.current().0, 1, "the first failure reloads");
    h.store.reset();
    let err = live.run(async |r| r.chunk(&x).await).await.unwrap_err();
    assert!(matches!(err, CoreError::Unavailable { .. }), "{err}");
    assert_eq!(
        live.current().0,
        1,
        "the second one, within the interval, does not"
    );
    assert_eq!(h.store.counts().get("list"), None, "{:?}", h.store.counts());

    // interval 0：每次都重新載入
    let (state, _) = repo.usable_state().await.unwrap();
    let eager = Live::new(repo.clone(), state, Duration::ZERO);
    for _ in 0..2 {
        assert!(eager.run(async |r| r.chunk(&x).await).await.is_err());
    }
    assert_eq!(eager.current().0, 2);
}

/// §10：最大世代讀不了（兩份 head 都壞了、或分片不見），restore 與 mount 退回較舊的世代並警告；backup 停止
/// （B1）。
#[tokio::test]
async fn readers_fall_back_to_an_older_generation_and_backup_refuses() {
    let t = TestRepo::new().await;
    let repo = t.open().await;
    let (content, id) = one_chunk_backup(&repo, t.dir.path(), "x", 1).await;
    write_generation(
        &repo,
        index_everything(&repo, &BTreeMap::new()).await,
        false,
    )
    .await;
    let g2 = write_generation(
        &repo,
        index_everything(&repo, &BTreeMap::new()).await,
        false,
    )
    .await;
    for replica in [false, true] {
        let key = Key::Head { g: g2, replica }.path();
        let mut bytes = repo.backend().get(&key).await.unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 1;
        repo.backend().put(&key, bytes).await.unwrap();
    }
    let (state, skipped) = repo.usable_state().await.unwrap();
    assert_eq!(state.gen.g, 1);
    assert_eq!(skipped.len(), 1, "{skipped:?}");
    assert!(skipped[0].starts_with("generation 2:"), "{skipped:?}");
    let live = repo.live().await.unwrap();
    assert_eq!(
        live.run(async |r| r.chunk(&id).await).await.unwrap(),
        content
    );

    let src = t.dir.path().join("src-x");
    let err = repo.backup(&[src], opts(1)).await.unwrap_err();
    assert!(matches!(err, CoreError::Generation(_)), "{err}");
}

/// 同上，壞的是最大世代的分片。
#[tokio::test]
async fn a_generation_with_a_missing_shard_is_skipped_by_readers() {
    let t = TestRepo::new().await;
    let repo = t.open().await;
    one_chunk_backup(&repo, t.dir.path(), "x", 1).await;
    write_generation(
        &repo,
        index_everything(&repo, &BTreeMap::new()).await,
        false,
    )
    .await;
    let g2 = write_generation(
        &repo,
        index_everything(&repo, &BTreeMap::new()).await,
        false,
    )
    .await;
    let gen2 = repo.read_head(g2).await.unwrap();
    let shard = gen2.head.shards.iter().flatten().next().copied().unwrap();
    let in_g1 = repo
        .read_head(1)
        .await
        .unwrap()
        .head
        .shards
        .contains(&Some(shard));
    assert!(!in_g1, "the shard must be new in generation 2");
    let uncached = t.open_uncached().await;
    uncached
        .backend()
        .delete(
            &Key::Shard {
                id: shard,
                replica: false,
            }
            .path(),
        )
        .await
        .unwrap();
    let (state, skipped) = uncached.usable_state().await.unwrap();
    assert_eq!((state.gen.g, skipped.len()), (1, 1), "{skipped:?}");
}

/// §9 與 P5 的順序：snapshot 主體讀不了就讀 `.r1`；兩份都不行才是錯誤，訊息帶兩份的原因。
#[tokio::test]
async fn a_snapshot_whose_body_is_damaged_is_read_from_its_r1() {
    let t = TestRepo::with(init_options(1)).await;
    let repo = t.open().await;
    let src = t.dir.path().join("src");
    sample_source(&src);
    let s = repo.backup(&[src], opts(1)).await.unwrap();
    let key = Key::parse(&s.snapshot).unwrap();
    let mut bytes = repo.backend().get(&s.snapshot).await.unwrap();
    bytes[30] ^= 1;
    repo.backend().put(&s.snapshot, bytes).await.unwrap();
    let back = repo.read_snapshot(&key).await.unwrap();
    assert_eq!(back.parent, None);
    assert_eq!(back.client, client(1));

    repo.backend()
        .delete(&format!("{}.r1", s.snapshot))
        .await
        .unwrap();
    let err = repo.read_snapshot(&key).await.unwrap_err().to_string();
    assert!(err.contains("body:") && err.contains(".r1:"), "{err}");
}

/// §7 的查找：多段的目錄沿 Dir 的索引找到涵蓋名稱的那一段；段的第一個與最後一個名稱、段之間、所有名稱之前與之後
/// 都對。
#[tokio::test]
async fn lookup_in_a_segmented_directory_finds_every_name_and_nothing_else() {
    let t = TestRepo::new().await;
    let repo = t.open().await;
    let src = t.dir.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    let names = names_with_cuts(&repo, 3);
    for n in &names {
        std::fs::write(src.join(n), n.as_bytes()).unwrap();
    }
    let s = repo
        .backup(std::slice::from_ref(&src), opts(1))
        .await
        .unwrap();
    let reader = repo.reader(None).await.unwrap();
    let snap = repo
        .read_snapshot(&Key::parse(&s.snapshot).unwrap())
        .await
        .unwrap();
    let tree = reader.roots(&snap.roots).await.unwrap()[0].tree.unwrap();
    let Item::Dir(dir) = reader.item(&tree).await.unwrap() else {
        panic!("{} names should make a directory index", names.len());
    };
    assert!(dir.segs.len() >= 3, "{} segments", dir.segs.len());
    let firsts: Vec<String> = dir
        .segs
        .iter()
        .map(|s| String::from_utf8(s.first.clone()).unwrap())
        .collect();
    let mut probes: Vec<String> = Vec::new();
    for f in &firsts {
        let i = names.iter().position(|n| n == f).unwrap();
        probes.push(f.clone());
        if i > 0 {
            probes.push(names[i - 1].clone());
        }
    }
    probes.push(names.last().unwrap().clone());
    for p in &probes {
        let e = reader.lookup(&tree, p.as_bytes()).await.unwrap();
        assert_eq!(e.map(|e| e.n), Some(p.as_bytes().to_vec()), "{p}");
    }
    for missing in ["a", "f", "f000000x", &format!("{}x", firsts[1]), "g"] {
        assert_eq!(
            reader.lookup(&tree, missing.as_bytes()).await.unwrap(),
            None,
            "{missing}"
        );
    }
}

/// snapshot 的指定：`latest`、完整的 key、`<client>/<時間戳>`、唯一的時間戳前綴；找不到與不唯一是錯誤。
#[tokio::test]
async fn snapshots_are_listed_by_time_and_resolved_from_a_spec() {
    let t = TestRepo::new().await;
    let repo = t.open().await;
    let src = t.dir.path().join("src");
    sample_source(&src);
    let a = repo
        .backup(std::slice::from_ref(&src), opts(1))
        .await
        .unwrap();
    let b = repo
        .backup(std::slice::from_ref(&src), opts(2))
        .await
        .unwrap();
    let keys: Vec<String> = repo
        .list_snapshot_keys()
        .await
        .unwrap()
        .iter()
        .map(Key::path)
        .collect();
    assert_eq!(keys, [a.snapshot.clone(), b.snapshot.clone()]);
    assert_eq!(
        repo.resolve_snapshot("latest").await.unwrap().path(),
        b.snapshot
    );
    assert_eq!(
        repo.resolve_snapshot(&a.snapshot).await.unwrap().path(),
        a.snapshot
    );
    let short = a.snapshot.strip_prefix("snapshots/").unwrap();
    assert_eq!(
        repo.resolve_snapshot(short).await.unwrap().path(),
        a.snapshot
    );
    let ts = short.split('/').nth(1).unwrap();
    assert_eq!(repo.resolve_snapshot(ts).await.unwrap().path(), a.snapshot);
    assert!(matches!(
        repo.resolve_snapshot("1999").await.unwrap_err(),
        CoreError::SnapshotNotFound(_)
    ));
    assert!(matches!(
        repo.resolve_snapshot("2").await.unwrap_err(),
        CoreError::AmbiguousSnapshot(_, 2)
    ));
}
