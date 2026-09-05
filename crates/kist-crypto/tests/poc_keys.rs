//! TEMPORARY PoC: cross-language key-derivation vectors for the v2 format.
//! The expected values were produced by the Go implementation (internal/
//! poc_keys_test.go) and must match byte-for-byte. Delete after the
//! experiment.

use argon2::{Algorithm, Argon2, Params, Version};
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{Key, XChaCha20Poly1305, XNonce};

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[test]
fn poc_key_derivation_matches_go() {
    let password = b"correct horse battery staple";
    let salt = [0x11u8; 16];
    let repo_id = [0xABu8; 16];
    let master = [0x42u8; 32];

    // Argon2id: t=3, m=64 MiB, p=4, out=32 — the RFC 9106 second option.
    let params = Params::new(64 * 1024, 3, 4, Some(32)).unwrap();
    let argon = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    let mut kek = [0u8; 32];
    argon.hash_password_into(password, &salt, &mut kek).unwrap();
    println!("POC_KEK {}", hex(&kek));
    assert_eq!(
        hex(&kek),
        "daa225443258b3c27130b9872378dd616724c8072613fb9483425b824634bc30"
    );

    for (ctx, expected) in [
        ("kist/v2/hash", "1953dd93ebf5b2e60606cb54e9b5a01debcb64a51c186fcaed7a698e776c8a24"),
        ("kist/v2/chunk", "cd800501750684a7f2de3982090396759351e2ba153a47c0cabd7a307c844d59"),
        ("kist/v2/meta", "b9bb8e689db093d3b7969ebd013efbcf04bb0f49c59d8934484fbda5bff91581"),
        ("kist/v2/index", "7db4ac7f2de7ff9f7ea22282af0bd5963a2a955d773db866210dd3055ecd8d7b"),
    ] {
        let sub = blake3::derive_key(ctx, &master);
        println!("POC_SUB_{ctx} {}", hex(&sub));
        assert_eq!(hex(&sub), expected, "subkey {ctx} differs from Go");
    }

    // Master-key AAD: "kist/v2/master\x00" || repo_id || chunker params LE.
    let mut aad = Vec::new();
    aad.extend_from_slice(b"kist/v2/master\x00");
    aad.extend_from_slice(&repo_id);
    for v in [512 * 1024u32, 2 * 1024 * 1024, 8 * 1024 * 1024] {
        aad.extend_from_slice(&v.to_le_bytes());
    }
    println!("POC_AAD {}", hex(&aad));
    assert_eq!(
        hex(&aad),
        "6b6973742f76322f6d617374657200abababababababababababababababab000008000000200000008000"
    );

    // Sealed master with fixed nonce must match Go byte-for-byte.
    let nonce = [0x77u8; 24];
    let cipher = XChaCha20Poly1305::new(Key::from_slice(&kek));
    let sealed = cipher
        .encrypt(
            XNonce::from_slice(&nonce),
            Payload { msg: &[0x99u8; 32], aad: &aad },
        )
        .unwrap();
    let sealed_full = [nonce.as_slice(), sealed.as_slice()].concat();
    println!("POC_AEAD_SEALED {}", hex(&sealed_full));
    assert_eq!(
        hex(&sealed_full),
        "7777777777777777777777777777777777777777777777774a054977332ad225cc963cd3bcd0fe51f82b59359e4edca977747c311f4978994a900ca56665b7b02c3655ac81e9ab25"
    );
}

#[test]
fn poc_aad_to_file() {
    let mut aad = Vec::new();
    aad.extend_from_slice(b"kist/v2/master\x00");
    aad.extend_from_slice(&[0xABu8; 16]);
    for v in [512 * 1024u32, 2 * 1024 * 1024, 8 * 1024 * 1024] {
        aad.extend_from_slice(&v.to_le_bytes());
    }
    std::fs::write("/tmp/poc/aad-rs.bin", &aad).unwrap();
    println!("POC_AAD_LEN {}", aad.len());
}
