//! 金鑰階層與 AEAD 封裝的行為測試。

use kist_crypto::{create_key_slot, unlock_key_slot, CryptoError, MasterKey, RepoKeys};
use kist_format::envelope::{Compression, ObjectKind};
use kist_format::ChunkId;

const PASSWORD: &str = "correct horse battery staple";

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
        "2026-01-01T00:00:00Z",
        fast_kdf(),
        &binding(),
    )
    .unwrap();
    assert_eq!(slot.kdf.algorithm, "argon2id");
    assert_eq!(slot.kdf.salt.len(), 16);
    assert_eq!(slot.wrapped_master_key.nonce.len(), 24);
    assert_eq!(slot.wrapped_master_key.ciphertext.len(), 32 + 16);

    let unlocked = unlock_key_slot(PASSWORD.as_bytes(), &slot, &binding()).unwrap();
    assert_eq!(unlocked.as_bytes(), master.as_bytes());
}

#[test]
fn wrong_password_is_rejected() {
    let (slot, _) = create_key_slot(
        PASSWORD.as_bytes(),
        "default",
        "2026-01-01T00:00:00Z",
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
        "2026-01-01T00:00:00Z",
        fast_kdf(),
        &binding(),
    )
    .unwrap();
    slot.wrapped_master_key.ciphertext[0] ^= 1;
    assert!(unlock_key_slot(PASSWORD.as_bytes(), &slot, &binding()).is_err());
}

#[test]
fn unknown_kdf_is_rejected() {
    let (mut slot, _) = create_key_slot(
        PASSWORD.as_bytes(),
        "default",
        "2026-01-01T00:00:00Z",
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
    let (slot_a, master) = create_key_slot(b"a", "a", "t", fast_kdf(), &binding()).unwrap();
    let slot_b =
        kist_crypto::wrap_master_key(&master, b"b", "b", "t", fast_kdf(), &binding()).unwrap();
    assert_ne!(slot_a.kdf.salt, slot_b.kdf.salt);
    assert_ne!(
        slot_a.wrapped_master_key.ciphertext,
        slot_b.wrapped_master_key.ciphertext
    );
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

#[test]
fn object_seal_open_round_trip_with_compression() {
    let keys = RepoKeys::from_master(&MasterKey::from_bytes([1; 32]));
    let plaintext = vec![b'a'; 10_000];
    let sealed = keys
        .seal_object(ObjectKind::Index, Compression::Zstd, &plaintext)
        .unwrap();
    assert!(sealed.len() < 1_000, "可壓縮的內容應該被壓縮");
    assert_eq!(
        keys.open_object(ObjectKind::Index, &sealed).unwrap(),
        plaintext
    );

    let raw = keys
        .seal_object(ObjectKind::Index, Compression::None, &plaintext)
        .unwrap();
    assert_eq!(raw.len(), 32 + plaintext.len() + 16);
    assert_eq!(
        keys.open_object(ObjectKind::Index, &raw).unwrap(),
        plaintext
    );
}

#[test]
fn object_kind_is_bound_by_aad() {
    let keys = RepoKeys::from_master(&MasterKey::from_bytes([1; 32]));
    let sealed = keys
        .seal_object(ObjectKind::Index, Compression::None, b"index")
        .unwrap();
    // 期待 tree 卻拿到 index：header 上的種類不符
    assert!(matches!(
        keys.open_object(ObjectKind::Tree, &sealed),
        Err(CryptoError::KindMismatch { .. })
    ));
    // 把 header 的種類改成 tree：AAD 不符，AEAD 驗證失敗
    let mut forged = sealed;
    forged[5] = ObjectKind::Tree as u8;
    assert!(matches!(
        keys.open_object(ObjectKind::Tree, &forged),
        Err(CryptoError::AuthFailed)
    ));
}

#[test]
fn tree_sealing_is_deterministic_but_others_are_not() {
    let keys = RepoKeys::from_master(&MasterKey::from_bytes([1; 32]));
    let a = keys
        .seal_object(ObjectKind::Tree, Compression::Zstd, b"tree bytes")
        .unwrap();
    let b = keys
        .seal_object(ObjectKind::Tree, Compression::Zstd, b"tree bytes")
        .unwrap();
    assert_eq!(
        a, b,
        "同樣的 tree 明文必須得到 byte-for-byte 相同的密文，否則子樹無法重用"
    );
    let c = keys
        .seal_object(ObjectKind::Tree, Compression::Zstd, b"tree bytes!")
        .unwrap();
    assert_ne!(a, c);

    let x = keys
        .seal_object(ObjectKind::Snapshot, Compression::Zstd, b"snap")
        .unwrap();
    let y = keys
        .seal_object(ObjectKind::Snapshot, Compression::Zstd, b"snap")
        .unwrap();
    assert_ne!(x, y, "非 tree 的物件用隨機 nonce");

    // 不同 repo 金鑰下，同樣的 tree 明文密文不同（nonce 由祕密 key 推導）
    let other = RepoKeys::from_master(&MasterKey::from_bytes([2; 32]));
    let z = other
        .seal_object(ObjectKind::Tree, Compression::Zstd, b"tree bytes")
        .unwrap();
    assert_ne!(a[8..32], z[8..32], "nonce 不能只是明文的公開 hash");
}

#[test]
fn tree_nonce_is_derived_from_the_encrypted_body_not_the_plaintext() {
    // 同一份明文、不同壓縮設定 → 加密的 bytes 不同 → nonce 必須不同，否則就是 nonce 重用。
    let keys = RepoKeys::from_master(&MasterKey::from_bytes([1; 32]));
    let plaintext = vec![b'a'; 1000];
    let zstd = keys
        .seal_object(ObjectKind::Tree, Compression::Zstd, &plaintext)
        .unwrap();
    let raw = keys
        .seal_object(ObjectKind::Tree, Compression::None, &plaintext)
        .unwrap();
    assert_ne!(zstd[8..32], raw[8..32], "nonce 必須隨被加密的 body 而異");
    assert_eq!(
        keys.open_object(ObjectKind::Tree, &zstd).unwrap(),
        plaintext
    );
    assert_eq!(keys.open_object(ObjectKind::Tree, &raw).unwrap(), plaintext);
}

fn binding() -> kist_crypto::KeyBinding {
    kist_crypto::KeyBinding {
        repo_id: vec![9; 16],
        chunker: kist_format::config::ChunkerParams::default(),
    }
}

#[test]
fn key_slot_is_bound_to_repo_id_and_chunker_params() {
    let (slot, _) = create_key_slot(PASSWORD.as_bytes(), "d", "t", fast_kdf(), &binding()).unwrap();
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
        create_key_slot(PASSWORD.as_bytes(), "d", "t", fast_kdf(), &binding()).unwrap();
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
