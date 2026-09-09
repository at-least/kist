//! pack 解析的 fuzz target。
//!
//! 三條路徑：
//! 1. 任意 bytes = 整個 pack：footer 算術、magic、trailer 解密封與驗證。
//! 2. 任意 bytes = trailer 明文（CBOR）：模擬「有 bug 的 client」——
//!    trailer 認證過但內容胡來，密封後走完整 `read_trailer` 驗證。
//! 3. 任意 bytes = chunk 明文 / entry 密文：壓縮→密封→解碼 roundtrip
//!    必得原文；任意密文必須 Err、不可 panic。
#![no_main]

use kist_crypto::{MasterKey, RepoKeys};
use kist_format::pack::{self, PackTrailer};
use kist_format::{cbor, ObjectId};

use libfuzzer_sys::fuzz_target;

fn test_keys() -> RepoKeys {
    RepoKeys::from_master(&MasterKey::from_bytes([0x42; 32]))
}

fuzz_target!(|data: &[u8]| {
    let keys = test_keys();

    let _ = kist_core::pack::read_trailer(&keys, data);

    if let Ok(trailer) = cbor::decode::<PackTrailer>(data) {
        let _ = kist_core::pack::validate_trailer(&trailer, data.len());
        let _ = kist_core::pack::validate_trailer(&trailer, 0);
        if let Ok(plain) = cbor::encode(&trailer) {
            if let Ok(sealed) = keys.seal_pack_trailer(&plain) {
                let mut p = pack::begin().to_vec();
                p.extend_from_slice(&sealed);
                p.extend_from_slice(&(sealed.len() as u64).to_be_bytes());
                p.extend_from_slice(&pack::magic());
                let _ = kist_core::pack::read_trailer(&keys, &p);
            }
        }
    }

    if !data.is_empty() {
        let id = keys.chunk_id(data);
        if let Ok(payload) = kist_core::pack::compress_chunk(data) {
            if let Ok(sealed) = keys.seal_chunk(&id, &payload) {
                let back = kist_core::pack::decode_chunk(&keys, &id, &sealed, data.len() as u64)
                    .unwrap_or_else(|e| panic!("chunk roundtrip failed: {e}"));
                assert_eq!(back, data, "chunk roundtrip broke");
            }
        }
        let _ = kist_core::pack::decode_chunk(&keys, &id, data, data.len() as u64);
    }

    let _ = ObjectId::of(data);
});
