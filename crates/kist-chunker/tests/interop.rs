//! 跨語言 conformance 測試：同樣的向量由 Go 實作（internal/interop/）
//! 消費。任何讓一邊期望值改變的格式改動，都會弄壞另一邊的 repo。

use kist_chunker::Chunker;
use kist_format::config::ChunkerParams;
use kist_format::tree::{content_type, node_type, Entry, Tree};
use kist_format::TreeId;

/// 共用的測試輸入：xorshift64* 產生的 bytes，與 Go 端同一函式逐 byte
/// 相同。產生器本身就是 corpus；進 repo 的只有邊界清單。
fn corpus_bytes(seed: u64, n: usize) -> Vec<u8> {
    let mut out = vec![0u8; n];
    let mut state = seed;
    for b in out.iter_mut() {
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
        state = state.wrapping_mul(0x2545F4914F6CDD1D);
        *b = (state >> 33) as u8;
    }
    out
}

fn load_boundaries(name: &str) -> Vec<usize> {
    let raw = std::fs::read(format!("tests/testdata/{name}")).expect("testdata file");
    let text = String::from_utf8(raw).expect("ascii");
    text.lines()
        .filter(|l| !l.is_empty())
        .map(|l| l.trim().parse().expect("number"))
        .collect()
}

#[test]
fn interop_chunker_boundaries() {
    let cases: &[(&str, ChunkerParams, usize)] = &[
        (
            "chunker-boundaries-small.txt",
            ChunkerParams { min: 1 << 10, avg: 4 << 10, max: 16 << 10 },
            4 << 20,
        ),
        ("chunker-boundaries-default.txt", ChunkerParams::default(), 16 << 20),
    ];
    for (name, params, len) in cases {
        let want = load_boundaries(name);
        let chunker = Chunker::new(*params);
        let data = corpus_bytes(0x5eed_1234, *len);
        let got: Vec<usize> = chunker
            .chunks(std::io::Cursor::new(&data))
            .map(|c| c.expect("chunk").len())
            .collect();
        assert_eq!(got.len(), want.len(), "{name}: chunk count differs from Go");
        for (i, (g, w)) in got.iter().zip(want.iter()).enumerate() {
            assert_eq!(g, w, "{name}: chunk {i} differs from Go");
        }
    }
}

#[test]
fn interop_key_derivation() {
    // 向量兩邊釘死；argon2 參數 64 MiB / t=3 / p=4。
    let password = b"correct horse battery staple";
    let salt = [0x11u8; 16];
    let master = [0x42u8; 32];

    let params = argon2::Params::new(64 * 1024, 3, 4, Some(32)).unwrap();
    let argon = argon2::Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params);
    let mut kek = [0u8; 32];
    argon.hash_password_into(password, &salt, &mut kek).unwrap();
    assert_eq!(
        to_hex(&kek),
        "daa225443258b3c27130b9872378dd616724c8072613fb9483425b824634bc30"
    );

    for (ctx, want) in [
        ("kist/v2/hash", "1953dd93ebf5b2e60606cb54e9b5a01debcb64a51c186fcaed7a698e776c8a24"),
        ("kist/v2/chunk", "cd800501750684a7f2de3982090396759351e2ba153a47c0cabd7a307c844d59"),
        ("kist/v2/meta", "b9bb8e689db093d3b7969ebd013efbcf04bb0f49c59d8934484fbda5bff91581"),
        ("kist/v2/index", "7db4ac7f2de7ff9f7ea22282af0bd5963a2a955d773db866210dd3055ecd8d7b"),
    ] {
        assert_eq!(to_hex(&blake3::derive_key(ctx, &master)), want, "subkey {ctx}");
    }
}


fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
