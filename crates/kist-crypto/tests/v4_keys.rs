//! v4 的金鑰推導與密封向量（`docs/format-v4.md` §3、§5）。
//!
//! 預期值全部來自一支獨立的 Go 程式（lukechampine.com/blake3 v1.4.1、golang.org/x/crypto v0.56.0 的
//! argon2 與 chacha20poly1305；config 的 CBOR 手寫），不是由這份程式碼錄的；見 ADR 026 實作第二段。
//! 輸入：密碼 "correct horse battery staple"、salt 0x11×16、Argon2id t=3 m=64 MiB p=4、master 0x42×32、
//! 固定 nonce 0x77×24（只為了向量可重現；實際寫入一律用 OS 亂數）、client 0x1f×16。KEK 與 v3 的向量相同
//! （同一組 Argon2 輸入）。

#![allow(clippy::unwrap_used, clippy::expect_used)]

use kist_crypto::v4::{derive_kek, slot_aad, unwrap_master, wrap_master, Keys};
use kist_crypto::{CryptoError, MasterKey};
use kist_format::v4::config::{Chunker, Config, Kdf, KdfSalt, Slot};
use kist_format::v4::ids::{ClientId, MetaId, RepoId};
use kist_format::v4::keys::SnapTs;

const PASSWORD: &[u8] = b"correct horse battery staple";
const CONFIG_CBOR: &str = "a5617604677265706f5f696450abababababababababababababababab67637265617465641b18d21c5fd88b8000676368756e6b6572a3636d696e1a00080000636176671a00200000636d61781a00800000636b6466a563616c67686172676f6e326964617403616d1a000100006170046473616c745011111111111111111111111111111111";
const KEK: &str = "daa225443258b3c27130b9872378dd616724c8072613fb9483425b824634bc30";
const SLOT_NAME: &str = "34efd77bf593a02e79dea919633c45fc";
const SLOT_AAD: &str =
    "6b6973742f76342f736c6f744616ad3596810910c820b7e456a72ed91254c918301c20ef5ebc07c432c16e46";
const SLOT_CBOR: &str = "a2617604677772617070656458487777777777777777777777777777777777777777777777774505269d1e7c00929fb00def0510b0b3e5da38134860ff6fe82618372183952b3f42db576520cd3195fac2f14deb7f46";
const CHUNK_ID_HELLO: &str = "84f7a82b59cadb1e3af4ffb81da787a2090c3b8182bb7abd3f4e9eba219668bc";
const META_ID_A0: &str = "6fdd2cb1d9a2205b11748f1f7d8813ba4c1a1915560ff0db29a145000506b555";
const CACHE_DIR: &str = "cc1b0783c21708a79f0180def1b644a6";
/// 從 "n0" 起前三個切段的名稱。
const CUT_NAMES: [u32; 3] = [4366, 4978, 8177];
const SEALED_CHUNK: &str =
    "777777777777777777777777777777777777777777777777e292d0e1356baa756fdb62360bb6b147a05e5ae4e58b";
const SEALED_META: &str =
    "777777777777777777777777777777777777777777777777e869d24507aef1ce3d704817d2c6cb1814d6";
const SEALED_TRAILER: &str = "777777777777777777777777777777777777777777777777cd2797ccdc4c973a17aff81a1fe7dc25ba6f801b1d83e1";
const SEALED_SNAPSHOT: &str =
    "7777777777777777777777777777777777777777777777774b907b4a23d39bfcd81601688293dad4c86ebeea";
const SEALED_DELTA: &str =
    "777777777777777777777777777777777777777777777777dd309ad1d1de2a065b64ade9f1c43ba05d64400a0a";
const SEALED_HEAD: &str =
    "777777777777777777777777777777777777777777777777d13097c1d91a51a7b15d278c35d088ba2c4a9dd3";
const SEALED_SHARD: &str =
    "777777777777777777777777777777777777777777777777ca3d97d7d472f8bbdb7cc6779f88f7ccd9decb977e";

fn h(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

fn hx(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn kdf() -> Kdf {
    Kdf::recommended(KdfSalt::from_bytes([0x11; 16]))
}

#[test]
fn the_config_of_the_vector_is_what_kist_format_writes() {
    let c = Config::new(
        RepoId::from_bytes([0xab; 16]),
        1_788_523_200_000_000_000,
        Chunker::default(),
        kdf(),
    );
    assert_eq!(hx(&c.encode().unwrap()), CONFIG_CBOR);
}

#[test]
fn slot_derivation_and_unwrap_match_the_vectors() {
    let config = h(CONFIG_CBOR);
    let kek = derive_kek(PASSWORD, &kdf()).unwrap();
    assert_eq!(hx(&kek.slot_name()), SLOT_NAME);
    assert_eq!(hx(&slot_aad(&config)), SLOT_AAD);
    let slot = Slot::decode(&h(SLOT_CBOR)).unwrap();
    let master = unwrap_master(&kek, &slot, &config).unwrap();
    assert_eq!(master.as_bytes(), &[0x42; 32]);
    // KEK 本身不公開；它與 v3 向量相同這件事由 Go 程式的輸出記錄。
    let _ = KEK;

    // config 換掉任何一個 byte → 槽開不起來。
    let mut tampered = config.clone();
    tampered[10] ^= 1;
    assert!(matches!(
        unwrap_master(&kek, &slot, &tampered),
        Err(CryptoError::WrongPassword)
    ));
    // 密碼錯 → 一樣的錯誤。
    let wrong = derive_kek(b"wrong", &kdf()).unwrap();
    assert!(matches!(
        unwrap_master(&wrong, &slot, &config),
        Err(CryptoError::WrongPassword)
    ));
    // 亂數 nonce 的包裝來回一致，而且兩次的 bytes 不同。
    let m = MasterKey::from_bytes([0x42; 32]);
    let a = wrap_master(&kek, &m, &config).unwrap();
    let b = wrap_master(&kek, &m, &config).unwrap();
    assert_ne!(a.wrapped, b.wrapped);
    assert_eq!(
        unwrap_master(&kek, &a, &config).unwrap().as_bytes(),
        &[0x42; 32]
    );
}

#[test]
fn subkeys_match_the_vectors() {
    let keys = Keys::derive(&MasterKey::from_bytes([0x42; 32]));
    assert_eq!(keys.chunk_id(b"hello").to_hex(), CHUNK_ID_HELLO);
    assert_eq!(keys.meta_id(&[0xa0]).to_hex(), META_ID_A0);
    assert_eq!(keys.cache_dir_name(), CACHE_DIR);
    let cuts: Vec<u32> = (0..=8177)
        .filter(|i| keys.is_cut(format!("n{i}").as_bytes()))
        .collect();
    assert_eq!(cuts, CUT_NAMES);
}

#[test]
fn every_sealing_rule_matches_the_vectors() {
    let keys = Keys::derive(&MasterKey::from_bytes([0x42; 32]));
    let client = ClientId::from_bytes([0x1f; 16]);
    let ts = SnapTs::from_nanos(1_788_523_200_123_456_789).unwrap();
    let chunk = keys.chunk_id(b"hello");
    let meta = MetaId::from_hex(META_ID_A0).unwrap();
    assert_eq!(
        keys.open_chunk(&chunk, &h(SEALED_CHUNK)).unwrap(),
        b"\x00hello"
    );
    assert_eq!(
        keys.open_meta(&meta, &h(SEALED_META)).unwrap(),
        [0x00, 0xa0]
    );
    assert_eq!(keys.open_trailer(&h(SEALED_TRAILER)).unwrap(), b"trailer");
    assert_eq!(
        keys.open_snapshot(&client, &ts, &h(SEALED_SNAPSHOT))
            .unwrap(),
        b"snap"
    );
    assert_eq!(
        keys.open_delta(&client, &h(SEALED_DELTA)).unwrap(),
        b"delta"
    );
    assert_eq!(keys.open_head(3, &h(SEALED_HEAD)).unwrap(), b"head");
    assert_eq!(keys.open_shard(&h(SEALED_SHARD)).unwrap(), b"shard");

    // AAD 不同（另一個 ID、另一台 client、另一個世代、另一個時間戳）就解不開。
    let other = keys.chunk_id(b"hellp");
    assert!(keys.open_chunk(&other, &h(SEALED_CHUNK)).is_err());
    assert!(keys
        .open_meta(&MetaId::from_bytes([0; 32]), &h(SEALED_META))
        .is_err());
    assert!(keys.open_delta(&ClientId::MAINT, &h(SEALED_DELTA)).is_err());
    assert!(keys.open_head(4, &h(SEALED_HEAD)).is_err());
    let later = SnapTs::from_nanos(1_788_523_200_123_456_790).unwrap();
    assert!(keys
        .open_snapshot(&client, &later, &h(SEALED_SNAPSHOT))
        .is_err());
    // trailer 與 shard 用同一把金鑰、不同的 AAD。
    assert!(keys.open_shard(&h(SEALED_TRAILER)).is_err());
    assert!(keys.open_trailer(&h(SEALED_SHARD)).is_err());
}
