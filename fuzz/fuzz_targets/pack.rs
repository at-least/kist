//! v4 pack 解析的 fuzz target（`docs/format-v4.md` §6）。
//!
//! 四條路徑：
//! 1. 任意 bytes = 整個 pack：footer 算術、magic、trailer 解密封與一致性驗證（`trailer_of`）。
//! 2. 任意 bytes = trailer 明文（CBOR）：模擬「有 bug 的 client」——trailer 認證過但內容胡來，
//!    密封後走完整的 `trailer_of` 驗證。
//! 3. 任意 bytes = chunk／metadata item 的明文：壓縮→密封→解開 roundtrip 必得原文；
//!    任意密文必須 Err、不可 panic。
//! 4. 任意 bytes = 一項的明文框（`alg byte ‖ data`）：`unframe` 不 panic。
#![no_main]

use kist_core::v4::pack::{compress, open_chunk, open_meta, trailer_of};
use kist_crypto::v4::Keys;
use kist_crypto::MasterKey;
use kist_format::v4::ids::{ClientId, ObjectId};
use kist_format::v4::keys::Key;
use kist_format::v4::pack::{self, PackTrailer};
use kist_format::cbor;

use libfuzzer_sys::fuzz_target;

const CHUNK_MAX: u32 = 8 << 20;

fn test_keys() -> Keys {
    Keys::derive(&MasterKey::from_bytes([0x42; 32]))
}

fuzz_target!(|data: &[u8]| {
    let keys = test_keys();
    let pack_key = Key::Pack {
        client: ClientId::from_bytes([1; 16]),
        id: ObjectId::ZERO,
    };
    let mpack_key = Key::Mpack {
        client: ClientId::from_bytes([1; 16]),
        id: ObjectId::ZERO,
        replica: false,
    };

    let _ = trailer_of(&keys, &pack_key, data, CHUNK_MAX);
    let _ = trailer_of(&keys, &mpack_key, data, CHUNK_MAX);
    let _ = pack::split_trailer(data);
    let _ = pack::parse_footer(data);
    let _ = pack::unframe(data);

    if let Ok(trailer) = cbor::decode::<PackTrailer>(data) {
        let _ = trailer.check(data.len() as u64, u64::from(CHUNK_MAX));
        let _ = trailer.check(0, 0);
        if let Ok(plain) = cbor::encode(&trailer) {
            if let Ok(sealed) = keys.seal_trailer(&plain) {
                let p = pack::finish(pack::begin(), &sealed);
                let _ = trailer_of(&keys, &pack_key, &p, CHUNK_MAX);
                let _ = trailer_of(&keys, &mpack_key, &p, CHUNK_MAX);
            }
        }
    }

    if !data.is_empty() {
        let id = keys.chunk_id(data);
        if let Ok(framed) = compress(data) {
            if let Ok(sealed) = keys.seal_chunk(&id, &framed) {
                let back = open_chunk(&keys, &id, &sealed, data.len() as u64, u64::from(CHUNK_MAX))
                    .unwrap_or_else(|e| panic!("chunk roundtrip failed: {e}"));
                assert_eq!(back, data, "chunk roundtrip broke");
            }
        }
        let _ = open_chunk(&keys, &id, data, data.len() as u64, u64::from(CHUNK_MAX));

        let mid = keys.meta_id(data);
        if data.len() as u64 <= kist_core::v4::pack::MAX_META {
            if let Ok(framed) = compress(data) {
                if let Ok(sealed) = keys.seal_meta(&mid, &framed) {
                    let back = open_meta(&keys, &mid, &sealed, data.len() as u64)
                        .unwrap_or_else(|e| panic!("metadata roundtrip failed: {e}"));
                    assert_eq!(back, data, "metadata roundtrip broke");
                }
            }
        }
        let _ = open_meta(&keys, &mid, data, data.len() as u64);
    }

    let _ = ObjectId::of(data);
});
