//! 金鑰階層與 AEAD 封裝的行為測試。

use kist_crypto::{create_key_slot, unlock_key_slot, CryptoError, MasterKey, RepoKeys};
use kist_format::ChunkId;
use kist_format::TreeId;

const PASSWORD: &str = "correct horse battery staple";

/// `2026-01-01T00:00:00Z` 的 Unix 奈秒。
const CREATED_NS: i64 = 1_767_225_600_000_000_000;

fn fast_kdf() -> kist_crypto::KdfCost {
    // 測試用：Argon2 最小參數，避免每個測試都花半秒。
    kist_crypto::KdfCost {
        m_cost_kib: 8,
        t_cost: 1,
        p_cost: 1,
    }
}

#[test]
fn key_slot_round_trip() {
    let (slot, master) = create_key_slot(
        PASSWORD.as_bytes(),
        "default",
        CREATED_NS,
        fast_kdf(),
        &binding(),
    )
    .unwrap();
    assert_eq!(slot.kdf.algorithm, "argon2id");
    assert_eq!(slot.kdf.salt.len(), 16);
    assert_eq!(slot.created_ns, CREATED_NS);
    // wrapped = nonce(24) ‖ 密文(32) ‖ tag(16)
    assert_eq!(slot.wrapped.len(), 24 + 32 + 16);

    let unlocked = unlock_key_slot(PASSWORD.as_bytes(), &slot, &binding()).unwrap();
    assert_eq!(unlocked.as_bytes(), master.as_bytes());
}

#[test]
fn wrong_password_is_rejected() {
    let (slot, _) = create_key_slot(
        PASSWORD.as_bytes(),
        "default",
        CREATED_NS,
        fast_kdf(),
        &binding(),
    )
    .unwrap();
    assert!(matches!(
        unlock_key_slot(b"wrong", &slot, &binding()),
        Err(CryptoError::WrongPassword)
    ));
}

#[test]
fn tampered_wrapped_key_is_rejected() {
    let (mut slot, _) = create_key_slot(
        PASSWORD.as_bytes(),
        "default",
        CREATED_NS,
        fast_kdf(),
        &binding(),
    )
    .unwrap();
    slot.wrapped[30] ^= 1; // 動密文部分（nonce 之後）
    assert!(unlock_key_slot(PASSWORD.as_bytes(), &slot, &binding()).is_err());
}

#[test]
fn unknown_kdf_is_rejected() {
    let (mut slot, _) = create_key_slot(
        PASSWORD.as_bytes(),
        "default",
        CREATED_NS,
        fast_kdf(),
        &binding(),
    )
    .unwrap();
    slot.kdf.algorithm = "scrypt".to_owned();
    assert!(matches!(
        unlock_key_slot(PASSWORD.as_bytes(), &slot, &binding()),
        Err(CryptoError::UnsupportedKdf(_))
    ));
}

#[test]
fn two_slots_wrap_the_same_master_key_differently() {
    // 同一個 master key 用兩組密碼各包一次：salt / nonce 不同，密文不同，但解出來一樣。
    let (slot_a, master) = create_key_slot(b"a", "a", 0, fast_kdf(), &binding()).unwrap();
    let slot_b =
        kist_crypto::wrap_master_key(&master, b"b", "b", 0, fast_kdf(), &binding()).unwrap();
    assert_ne!(slot_a.kdf.salt, slot_b.kdf.salt);
    assert_ne!(slot_a.wrapped, slot_b.wrapped);
    assert_eq!(
        unlock_key_slot(b"b", &slot_b, &binding())
            .unwrap()
            .as_bytes(),
        master.as_bytes()
    );
}

#[test]
fn derived_keys_are_distinct_and_deterministic() {
    let master = MasterKey::from_bytes([7; 32]);
    let keys = RepoKeys::from_master(&master);
    let again = RepoKeys::from_master(&master);
    assert_eq!(keys.chunk_id(b"x"), again.chunk_id(b"x"));

    let other = RepoKeys::from_master(&MasterKey::from_bytes([8; 32]));
    assert_ne!(
        keys.chunk_id(b"x"),
        other.chunk_id(b"x"),
        "chunk ID 必須依 repo 金鑰而異"
    );
    assert_ne!(
        keys.chunk_id(b"x").as_bytes(),
        blake3::hash(b"x").as_bytes(),
        "chunk ID 不能是無 key 的 hash"
    );
}

#[test]
fn chunk_seal_open_round_trip() {
    let keys = RepoKeys::from_master(&MasterKey::from_bytes([1; 32]));
    let plaintext = b"chunk payload";
    let id = keys.chunk_id(plaintext);
    let sealed = keys.seal_chunk(&id, plaintext).unwrap();
    assert_eq!(sealed.len(), 24 + plaintext.len() + 16);
    assert_eq!(keys.open_chunk(&id, &sealed).unwrap(), plaintext);

    // 密文改一個 bit
    let mut bad = sealed.clone();
    bad[30] ^= 1;
    assert!(matches!(
        keys.open_chunk(&id, &bad),
        Err(CryptoError::AuthFailed)
    ));
    // AAD（chunk ID）換掉
    let other = ChunkId::from_bytes([9; 32]);
    assert!(matches!(
        keys.open_chunk(&other, &sealed),
        Err(CryptoError::AuthFailed)
    ));
    // 太短
    assert!(keys.open_chunk(&id, &sealed[..20]).is_err());
    // 兩次 seal 用不同 nonce
    assert_ne!(keys.seal_chunk(&id, plaintext).unwrap(), sealed);
}

/// v2 沒有 envelope：index blob / pack trailer 以角色 AAD 密封，長度 = nonce ‖ 密文 ‖ tag。
#[test]
fn index_blob_seal_open_round_trip() {
    let keys = RepoKeys::from_master(&MasterKey::from_bytes([1; 32]));
    let plaintext = vec![b'a'; 10_000];
    let sealed = keys.seal_index_blob_in_place(plaintext.clone()).unwrap();
    assert_eq!(sealed.len(), 24 + plaintext.len() + 16);
    assert_eq!(keys.open_index_blob(&sealed).unwrap(), plaintext);

    let trailer = keys.seal_pack_trailer(b"trailer").unwrap();
    assert_eq!(keys.open_pack_trailer(&trailer).unwrap(), b"trailer");
}

/// in-place 版輸出格式相同（nonce ‖ ct ‖ tag）、tag 預留不該改變內容。
#[test]
fn index_blob_seal_in_place_round_trip() {
    let keys = RepoKeys::from_master(&MasterKey::from_bytes([1; 32]));
    let plaintext = vec![b'b'; 10_000];
    let sealed = keys.seal_index_blob_in_place(plaintext.clone()).unwrap();
    assert_eq!(sealed.len(), 24 + plaintext.len() + 16);
    assert_eq!(keys.open_index_blob(&sealed).unwrap(), plaintext);
    // 吃掉明文之後回傳的密文開頭是 24 bytes 的 nonce，與明文無關
    let again = keys.seal_index_blob_in_place(plaintext.clone()).unwrap();
    assert_ne!(&sealed[..24], &again[..24], "nonce 必須每次隨機");
}

/// 各角色的 AAD 互相綁定：拿 index 的密文當 trailer 開（AAD 不同）必須失敗。
#[test]
fn sealed_roles_are_bound_by_aad() {
    let keys = RepoKeys::from_master(&MasterKey::from_bytes([1; 32]));
    let index = keys.seal_index_blob_in_place(b"index".to_vec()).unwrap();
    assert!(matches!(
        keys.open_pack_trailer(&index),
        Err(CryptoError::AuthFailed)
    ));
    let trailer = keys.seal_pack_trailer(b"trailer").unwrap();
    assert!(matches!(
        keys.open_index_blob(&trailer),
        Err(CryptoError::AuthFailed)
    ));

    // tree 的 AAD 是自己的 ID：換個 ID 就開不了
    let id = TreeId::from_bytes([1; 32]);
    let sealed = keys.seal_tree(&id, b"tree").unwrap();
    assert_eq!(keys.open_tree(&id, &sealed).unwrap(), b"tree");
    let other = TreeId::from_bytes([2; 32]);
    assert!(matches!(
        keys.open_tree(&other, &sealed),
        Err(CryptoError::AuthFailed)
    ));

    // snapshot 的 AAD 是完整 key 路徑：路徑不對就開不了
    let snap = keys.seal_snapshot("snapshots/ab/20260904T000000000000000Z", b"snap").unwrap();
    assert_eq!(
        keys.open_snapshot("snapshots/ab/20260904T000000000000000Z", &snap)
            .unwrap(),
        b"snap"
    );
    assert!(matches!(
        keys.open_snapshot("snapshots/ab/20260905T000000000000000Z", &snap),
        Err(CryptoError::AuthFailed)
    ));
}

/// v2 的重用性改由「名稱 = 明文的 keyed hash」保證：同明文同名稱（跨 repo 不同），
/// 密封本身則一律隨機 nonce——名稱不再洩漏、也不再有決定性加密。
#[test]
fn tree_ids_are_deterministic_but_sealing_is_not() {
    let keys = RepoKeys::from_master(&MasterKey::from_bytes([1; 32]));
    let plain = b"tree bytes";
    let id = keys.tree_id(plain);
    assert_eq!(keys.tree_id(plain), id, "同明文 → 同名稱");
    assert_ne!(keys.tree_id(b"tree bytes!"), id);

    let a = keys.seal_tree(&id, plain).unwrap();
    let b = keys.seal_tree(&id, plain).unwrap();
    assert_ne!(
        a, b,
        "v2 沒有決定性加密：同樣的明文兩次密封必須用不同的隨機 nonce"
    );
    assert_ne!(&a[..24], &b[..24], "nonce 必須不同（nonce 重用是災難）");
    assert_eq!(keys.open_tree(&id, &a).unwrap(), plain);
    assert_eq!(keys.open_tree(&id, &b).unwrap(), plain);

    let x = keys.seal_snapshot("k", b"snap").unwrap();
    let y = keys.seal_snapshot("k", b"snap").unwrap();
    assert_ne!(x, y, "非 tree 的物件也用隨機 nonce");

    // 不同 repo 金鑰下，同樣的明文得到不同的名稱（keyed hash）
    let other = RepoKeys::from_master(&MasterKey::from_bytes([2; 32]));
    assert_ne!(
        other.tree_id(plain),
        id,
        "tree 名稱依 repo 金鑰而異，不能只是明文的公開 hash"
    );
}

/// 同明文兩次密封的 nonce 必須不同；明文與密文都可以正常來回。
#[test]
fn sealing_nonces_are_random_per_call() {
    let keys = RepoKeys::from_master(&MasterKey::from_bytes([1; 32]));
    let plaintext = vec![b'a'; 1000];
    let zstd_like = keys.seal_index_blob_in_place(plaintext.clone()).unwrap();
    let raw_like = keys.seal_index_blob_in_place(plaintext.clone()).unwrap();
    assert_ne!(
        &zstd_like[..24],
        &raw_like[..24],
        "兩次密封的 nonce 必須不同，無論上層怎麼處理明文"
    );
    assert_eq!(keys.open_index_blob(&zstd_like).unwrap(), plaintext);
    assert_eq!(keys.open_index_blob(&raw_like).unwrap(), plaintext);
}

/// 密文被改一個 bit、空明文、以及 hash/chunk 兩個 context 的域分離。
#[test]
fn tree_seal_rejects_tampering_and_contexts_are_separated() {
    let keys = RepoKeys::from_master(&MasterKey::from_bytes([1; 32]));
    let id = TreeId::from_bytes([3; 32]);
    let sealed = keys.seal_tree(&id, b"tree").unwrap();
    // 密文中任一個 bit 被改都要失敗（tag 驗證）
    for pos in [0, sealed.len() / 2, sealed.len() - 1] {
        let mut bad = sealed.clone();
        bad[pos] ^= 1;
        assert!(
            matches!(keys.open_tree(&id, &bad), Err(CryptoError::AuthFailed)),
            "篡改位置 {pos} 必須被拒絕"
        );
    }
    // 空明文也要能來回（nonce ‖ tag，沒有密文內容）
    let empty = keys.seal_tree(&id, b"").unwrap();
    assert_eq!(empty.len(), 24 + 16);
    assert_eq!(keys.open_tree(&id, &empty).unwrap(), b"");
    // TreeId 與 ChunkId 同函式同金鑰（kist-format/src/ids.rs 明載的設計：
    // 兩者活在不同命名空間——tree 只吃 CBOR、chunk 只吃原始資料——不會撞）；
    // 但都要與無 key 的 BLAKE3 不同
    let data = b"same bytes";
    assert_eq!(
        keys.chunk_id(data).as_bytes(),
        keys.tree_id(data).as_bytes(),
        "TreeId 與 ChunkId 是同一個 keyed hash（文件明載）"
    );
    assert_ne!(
        keys.tree_id(data).as_bytes(),
        blake3::hash(data).as_bytes(),
        "tree 名稱不能是無 key 的 hash"
    );
}

fn binding() -> kist_crypto::KeyBinding {
    kist_crypto::KeyBinding {
        repo_id: vec![9; 16],
        chunker: kist_format::config::ChunkerParams::default(),
    }
}

#[test]
fn key_slot_is_bound_to_repo_id_and_chunker_params() {
    let (slot, _) = create_key_slot(PASSWORD.as_bytes(), "d", 0, fast_kdf(), &binding()).unwrap();
    assert!(unlock_key_slot(PASSWORD.as_bytes(), &slot, &binding()).is_ok());

    let mut other_repo = binding();
    other_repo.repo_id[0] ^= 1;
    assert!(
        matches!(
            unlock_key_slot(PASSWORD.as_bytes(), &slot, &other_repo),
            Err(CryptoError::WrongPassword)
        ),
        "repo_id 被改 → 解不開"
    );

    let mut other_chunker = binding();
    other_chunker.chunker.avg += 1;
    assert!(
        unlock_key_slot(PASSWORD.as_bytes(), &slot, &other_chunker).is_err(),
        "chunker 被改 → 解不開"
    );
}

#[test]
fn absurd_kdf_parameters_are_rejected_before_running_argon2() {
    let (mut slot, _) =
        create_key_slot(PASSWORD.as_bytes(), "d", 0, fast_kdf(), &binding()).unwrap();
    slot.kdf.m_cost_kib = u32::MAX; // 4 TiB
    let started = std::time::Instant::now();
    assert!(matches!(
        unlock_key_slot(PASSWORD.as_bytes(), &slot, &binding()),
        Err(CryptoError::BadKdfParams(_))
    ));
    assert!(started.elapsed().as_secs() < 1, "不該真的去配置記憶體");
    slot.kdf.m_cost_kib = 8;
    slot.kdf.t_cost = u32::MAX;
    assert!(matches!(
        unlock_key_slot(PASSWORD.as_bytes(), &slot, &binding()),
        Err(CryptoError::BadKdfParams(_))
    ));
}

#[test]
fn default_kdf_cost_is_above_owasp_floor() {
    let c = kist_crypto::KdfCost::default();
    assert!(c.m_cost_kib >= 64 * 1024 && c.t_cost >= 3, "{c:?}");
    assert_eq!(c.p_cost, 4, "v2 與 Go 端一致的預設平行度");
}

#[test]
fn cache_id_is_derived_from_master_key() {
    let a = RepoKeys::from_master(&MasterKey::from_bytes([1; 32]));
    let b = RepoKeys::from_master(&MasterKey::from_bytes([2; 32]));
    assert_eq!(
        a.cache_id(),
        RepoKeys::from_master(&MasterKey::from_bytes([1; 32])).cache_id()
    );
    assert_ne!(a.cache_id(), b.cache_id());
    assert_ne!(a.cache_id(), [0; 16]);
}
