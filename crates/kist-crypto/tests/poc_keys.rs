//! 跨語言金鑰推導向量（v3）。v3 起向量由 **Rust（產品／規格管理者）**
//! 錄製，Go 端（internal/interop）移植時必須對相同輸入得到逐 byte 相同
//! 的輸出（V3-KEYS-1）。
//!
//! 固定輸入：password / salt / repo_id / master 全部用固定 bytes，
//! Argon2 參數 = RFC 9106 第二組建議，nonce 固定 0x77×24（僅測試；
//! 實際寫入永遠用 OS 亂數 nonce）。

use argon2::{Algorithm, Argon2, Params, Version};
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn invariants_cbor() -> Vec<u8> {
    let inv = kist_format::config::Invariants::new(
        vec![0xABu8; 16],
        kist_format::config::ChunkerParams::default(),
    );
    kist_format::cbor::encode(&inv).unwrap()
}

#[test]
fn poc_key_derivation_matches_recorded_vectors() {
    let password = b"correct horse battery staple";
    let salt = [0x11u8; 16];
    let master = [0x42u8; 32];

    // Argon2id: t=3, m=64 MiB, p=4, out=32 — the RFC 9106 second option.
    // （KDF 與 v2 相同——這條向量在 v2 就已與 Go 逐 byte 對過。）
    let params = Params::new(64 * 1024, 3, 4, Some(32)).unwrap();
    let argon = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    let mut kek = [0u8; 32];
    argon.hash_password_into(password, &salt, &mut kek).unwrap();
    println!("POC_KEK {}", hex(&kek));
    assert_eq!(
        hex(&kek),
        "daa225443258b3c27130b9872378dd616724c8072613fb9483425b824634bc30"
    );

    // v3 子金鑰 context。
    let mut sub_hex = std::collections::BTreeMap::new();
    for ctx in [
        "kist/v3/hash",
        "kist/v3/chunk",
        "kist/v3/meta",
        "kist/v3/index",
    ] {
        let sub = blake3::derive_key(ctx, &master);
        println!("POC_SUB_{ctx} {}", hex(&sub));
        sub_hex.insert(ctx.to_owned(), hex(&sub));
    }
    for (ctx, expected) in [
        ("kist/v3/hash", HASH_SUB_HEX),
        ("kist/v3/chunk", CHUNK_SUB_HEX),
        ("kist/v3/meta", META_SUB_HEX),
        ("kist/v3/index", INDEX_SUB_HEX),
    ] {
        assert_eq!(sub_hex[ctx], expected, "subkey {ctx} differs from vector");
    }

    // v3 的 master AAD 是常數（不變式在密文裡，不在 AAD 排版裡）。
    println!("POC_AAD {}", hex(kist_format::AAD_MASTER));
    assert_eq!(hex(kist_format::AAD_MASTER), MASTER_AAD_HEX);

    // Sealed master：payload = master(32) ‖ Invariants CBOR，AAD = 常數。
    // 固定 nonce 僅為了向量可重現。
    let nonce = [0x77u8; 24];
    let cipher = XChaCha20Poly1305::new((&kek).into());
    let mut payload = vec![0x99u8; 32];
    payload.extend_from_slice(&invariants_cbor());
    let sealed = cipher
        .encrypt(
            &XNonce::from(nonce),
            Payload {
                msg: &payload,
                aad: kist_format::AAD_MASTER,
            },
        )
        .unwrap();
    let sealed_full = [nonce.as_slice(), sealed.as_slice()].concat();
    println!("POC_PAYLOAD {}", hex(&payload));
    println!("POC_AEAD_SEALED {}", hex(&sealed_full));
    assert_eq!(hex(&sealed_full), SEALED_MASTER_HEX);
    // payload 的後半段就是 Invariants 的規範 CBOR（V3-CBOR 附帶釘住）。
    assert_eq!(
        hex(&invariants_cbor()),
        "a3617603677265706f5f696450abababababababababababababababab676368756e6b6572a3636d696e1a00080000636176671a00200000636d61781a00800000"
    );
}

// v3 向量（由本測試的 println 錄製；Go 端移植時對相同輸入驗證）。
const HASH_SUB_HEX: &str = "cb79b897d172c800d23506629ca5b14781f8b0232ab1714c7f85b8199f4e527d";
const CHUNK_SUB_HEX: &str = "15806189fdb9ae9e6b867a9d40a1cce2ac24a26c74492b64389ce5aafffb579f";
const META_SUB_HEX: &str = "c4f44efcd5a8c073177757493a3d7f895800060bef9bca2102c5aa6f13ebaf52";
const INDEX_SUB_HEX: &str = "2121d5548b3373b970ab0a1618a068985e8f41f38565586c1cdf60c1e266e93f";
const MASTER_AAD_HEX: &str = "6b6973742f76332f6d6173746572";
const SEALED_MASTER_HEX: &str = "7777777777777777777777777777777777777777777777774a054977332ad225cc963cd3bcd0fe51f82b59359e4edca977747c311f4978996ea17e7661a70a03fc1184a97ddf61b4f75cef5b50b5f1bfdab8ccb9b44e7900f6a3564de3b1afb09ccaa7837436e6490b7e60d24fefdf3ee8eb4f6befca7708812c498df9958ff1c3519284a97abb97d5";
