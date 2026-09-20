//! `rebuild-index`：從 pack trailer 重建 index。

mod common;

use common::*;
use kist_core::{CheckOptions, RestoreOptions};

#[tokio::test]
async fn rebuild_after_all_index_blobs_are_lost() {
    let t = TestRepo::new().await;
    let src = t.dir.path().join("src");
    make_source(&src);
    let repo = t.open().await;
    let s = repo
        .backup(std::slice::from_ref(&src), backup_options())
        .await
        .unwrap();
    let before = repo.load_index().await.unwrap();
    for blob in walk_files(&t.repo_path().join("indexes")) {
        if blob.is_file() {
            std::fs::remove_file(blob).unwrap();
        }
    }
    assert!(!repo
        .check(CheckOptions {
            read_data: false,
            repair: false
        })
        .await
        .unwrap()
        .errors
        .is_empty());
    let broken = repo
        .restore(
            &s.snapshot_key,
            &t.dir.path().join("out0"),
            RestoreOptions::default(),
        )
        .await
        .unwrap();
    assert!(!broken.errors.is_empty(), "沒有 index 時檔案應該還原失敗");

    let summary = repo.rebuild_index().await.unwrap();
    assert_eq!(summary.packs as usize, before.pack_count());
    assert_eq!(summary.chunks as usize, before.len());
    assert_eq!(summary.superseded, 0);
    assert_eq!(t.count("indexes"), 1);

    let after = repo.load_index().await.unwrap();
    assert_eq!(after.len(), before.len());
    for (id, loc) in before.chunks() {
        assert_eq!(after.get(&id), Some(loc), "chunk {id}");
    }
    let report = repo
        .check(CheckOptions {
            read_data: true,
            repair: false,
        })
        .await
        .unwrap();
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    let target = t.dir.path().join("out");
    repo.restore(&s.snapshot_key, &target, RestoreOptions::default())
        .await
        .unwrap();
    assert_same_tree(&src, &target.join(src.strip_prefix("/").unwrap_or(&src)));
}

#[tokio::test]
async fn rebuild_with_existing_blobs_supersedes_them() {
    let t = TestRepo::new().await;
    let src = t.dir.path().join("src");
    make_source(&src);
    let repo = t.open().await;
    repo.backup(std::slice::from_ref(&src), backup_options())
        .await
        .unwrap();
    std::fs::write(src.join("more.bin"), random_bytes(41, 100 * 1024)).unwrap();
    repo.backup(std::slice::from_ref(&src), backup_options())
        .await
        .unwrap();
    assert_eq!(t.count("indexes"), 2);
    let before = repo.load_index().await.unwrap();

    let summary = repo.rebuild_index().await.unwrap();
    assert_eq!(summary.superseded, 2);
    assert_eq!(
        t.count("indexes"),
        3,
        "舊 blob 不刪（M3 的 GC 才刪），新 blob 取代它們"
    );
    let after = repo.load_index().await.unwrap();
    assert_eq!(after.len(), before.len());
    assert_eq!(after.pack_count(), before.pack_count());
    let report = repo
        .check(CheckOptions {
            read_data: false,
            repair: false,
        })
        .await
        .unwrap();
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    assert!(
        report.warnings.is_empty(),
        "重建後不該有未被 index 引用的 pack：{:?}",
        report.warnings
    );
}

#[tokio::test]
async fn rebuild_reports_corrupt_pack_trailer() {
    let t = TestRepo::new().await;
    let src = t.dir.path().join("src");
    make_source(&src);
    let repo = t.open().await;
    repo.backup(std::slice::from_ref(&src), backup_options())
        .await
        .unwrap();
    let mut packs = walk_files(&t.repo_path().join("packs"));
    packs.retain(|p| p.is_file());
    let victim = &packs[0];
    let mut bytes = std::fs::read(victim).unwrap();
    let n = bytes.len();
    bytes[n - 40] ^= 0xff; // trailer 內
    std::fs::write(victim, bytes).unwrap();

    let err = repo.rebuild_index().await.unwrap_err();
    let name = victim.file_name().unwrap().to_str().unwrap();
    assert!(err.to_string().contains(name), "{err}");
}

/// 「認證」不等於「一致」：trailer 有有效 tag、但內容前後矛盾（有 bug 的 client
/// 寫得出這種 pack，見 `pack::read_trailer` 的 doc）。rebuild 必須套用與
/// `read_trailer` 同一套一致性檢查，不能把這種 pack 收進重建出來的 index。
#[tokio::test]
async fn rebuild_rejects_authenticated_but_inconsistent_trailer() {
    let t = TestRepo::new().await;
    let src = t.dir.path().join("src");
    make_source(&src);
    let repo = t.open().await;
    repo.backup(std::slice::from_ref(&src), backup_options())
        .await
        .unwrap();

    // config 是明文 CBOR：用它解出 repo 的 key，重封一份被改壞的 trailer。
    let config: kist_format::config::RepoConfig =
        kist_format::cbor::decode(&std::fs::read(t.repo_path().join("config")).unwrap()).unwrap();
    let unlocked = kist_crypto::unlock_key_slot(PASSWORD.as_bytes(), &config.key).unwrap();
    let keys = kist_crypto::RepoKeys::from_master(&unlocked.master);

    // 挑一個 ≥2 entry 的 pack，把 entries[1].offset 覆寫成 entries[0].offset
    // （連續性破圖；值取自同一份 trailer，重封後 tag 有效）。
    let mut victim = None;
    for p in walk_files(&t.repo_path().join("packs")) {
        let bytes = std::fs::read(&p).unwrap();
        let trailer_len =
            kist_format::pack::parse_footer(&bytes[bytes.len() - 16..]).unwrap() as usize;
        let sealed = &bytes[bytes.len() - 16 - trailer_len..bytes.len() - 16];
        let plain = keys.open_pack_trailer(sealed).unwrap();
        let mut trailer: kist_format::pack::PackTrailer =
            kist_format::cbor::decode(&plain).unwrap();
        if trailer.entries.len() < 2 {
            continue;
        }
        trailer.entries[1].offset = trailer.entries[0].offset;
        let new_sealed = keys
            .seal_pack_trailer(&kist_format::cbor::encode(&trailer).unwrap())
            .unwrap();
        let mut out = bytes[..bytes.len() - 16 - trailer_len].to_vec();
        out.extend_from_slice(&new_sealed);
        out.extend_from_slice(&(new_sealed.len() as u64).to_be_bytes());
        out.extend_from_slice(&kist_format::pack::magic());
        std::fs::write(&p, out).unwrap();
        victim = Some(p);
        break;
    }
    let victim = victim.expect("測試資料裡該有至少一個多 entry 的 pack");

    let err = repo.rebuild_index().await.unwrap_err();
    let name = victim.file_name().unwrap().to_str().unwrap();
    assert!(err.to_string().contains(name), "錯誤要指名 pack：{err}");
}
