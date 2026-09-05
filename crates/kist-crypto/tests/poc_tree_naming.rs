//! TEMPORARY PoC: tree-naming stability under compressor change. Delete
//! after the experiment.
//!
//! Scheme A (kist-rs v1): name = BLAKE3(envelope ciphertext), envelope
//! uses a deterministic nonce derived from the *post-compression* body.
//! Scheme B (kist v2 candidate): name = keyed BLAKE3(tree plaintext CBOR),
//! nonce random.
//!
//! Two "compressor versions" are simulated with zstd level 3 and 19: the
//! same plaintext compresses to different bytes. Evidence produced:
//! - A: nonce(body-derived) differs (no reuse — safe), but the NAME differs
//!      too → identity coupled to compressor version.
//! - B: name is over plaintext → identical under both versions.

use blake3::Hasher;
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{Key, XChaCha20Poly1305, XNonce};

fn tree_plaintext() -> Vec<u8> {
    // Stand-in for a mid-size tree CBOR: repeated-ish structure that
    // compresses differently at different levels.
    let mut buf = Vec::new();
    for i in 0..2000 {
        buf.extend_from_slice(format!("{{n:'file-{i:05}.txt',mode:420,size:{i}}},").as_bytes());
    }
    buf
}

fn scheme_a_name(nonce_key: &[u8; 32], object_key: &[u8; 32], body: &[u8], aad: &[u8; 32]) -> [u8; 32] {
    let mut nonce = [0u8; 24];
    nonce.copy_from_slice(&blake3::keyed_hash(nonce_key, body).as_bytes()[..24]);
    let cipher = XChaCha20Poly1305::new(Key::from_slice(object_key));
    let ct = cipher
        .encrypt(XNonce::from_slice(&nonce), Payload { msg: body, aad })
        .unwrap();
    let mut full = Vec::with_capacity(32 + ct.len());
    full.extend_from_slice(aad);
    full.extend_from_slice(&ct);
    *blake3::hash(&full).as_bytes()
}

fn scheme_a_nonce(nonce_key: &[u8; 32], body: &[u8]) -> [u8; 24] {
    let mut n = [0u8; 24];
    n.copy_from_slice(&blake3::keyed_hash(nonce_key, body).as_bytes()[..24]);
    n
}

fn scheme_b_name(hash_key: &[u8; 32], plaintext: &[u8]) -> [u8; 32] {
    *blake3::keyed_hash(hash_key, plaintext).as_bytes()
}

#[test]
fn poc_tree_naming_stability() {
    let master = [7u8; 32];
    let hash_key: [u8; 32] = blake3::derive_key("kist v1 hash key", &master);
    let nonce_key: [u8; 32] = blake3::derive_key("kist v1 nonce key", &master);
    let object_key: [u8; 32] = blake3::derive_key("kist v1 object key", &master);
    let aad = [0xAAu8; 32]; // stand-in envelope header

    let plaintext = tree_plaintext();
    let body3 = zstd::encode_all(&plaintext[..], 3).unwrap();
    let body19 = zstd::encode_all(&plaintext[..], 19).unwrap();
    println!(
        "POC_ZSTD plain={} body3={} body19={} same={}",
        plaintext.len(),
        body3.len(),
        body19.len(),
        body3 == body19
    );
    assert_ne!(body3, body19, "need two distinct compressor outputs");

    let n3 = scheme_a_nonce(&nonce_key, &body3);
    let n19 = scheme_a_nonce(&nonce_key, &body19);
    let a3 = scheme_a_name(&nonce_key, &object_key, &body3, &aad);
    let a19 = scheme_a_name(&nonce_key, &object_key, &body19, &aad);
    println!(
        "POC_SCHEME_A nonce_same={} name_same={} name3={} name19={}",
        n3 == n19,
        a3 == a19,
        hex(&a3),
        hex(&a19)
    );
    // No nonce reuse (bodies differ → nonces differ), but the name moved.
    assert_ne!(n3, n19);
    assert_ne!(a3, a19, "scheme A name must move when the body moves");

    let b3 = scheme_b_name(&hash_key, &plaintext);
    let b19 = scheme_b_name(&hash_key, &plaintext);
    println!("POC_SCHEME_B name_same={} name={}", b3 == b19, hex(&b3));
    assert_eq!(b3, b19, "scheme B name is over plaintext: compressor-independent");

    // And a random nonce (scheme B sealing) does not enter the name at all:
    let _ = Hasher::new(); // blake3 hasher imported for parity checks
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
