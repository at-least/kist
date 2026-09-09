//! CBOR 解碼的 fuzz target。
//!
//! 第一個 byte 選結構、其餘是 CBOR body。所有 metadata 結構都吃得到，
//! 包括 ID 型別的 32-byte 邊界檢查。解碼只需不 panic——格式錯誤回 Err
//! 即為正確行為。已知防線：ciborium 有 RecursionLimitExceeded
//! （tests/cbor_depth.rs 釘著）。
#![no_main]

use kist_format::config::{ChunkerParams, RepoConfig};
use kist_format::index::IndexBlob;
use kist_format::pack::PackTrailer;
use kist_format::snapshot::Snapshot;
use kist_format::tree::Tree;
use kist_format::{cbor, parity};

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Some((&selector, rest)) = data.split_first() else {
        return;
    };
    match selector % 7 {
        0 => {
            let _ = cbor::decode::<PackTrailer>(rest);
        }
        1 => {
            let _ = cbor::decode::<IndexBlob>(rest);
        }
        2 => {
            let _ = cbor::decode::<Tree>(rest);
        }
        3 => {
            let _ = cbor::decode::<Snapshot>(rest);
        }
        4 => {
            let _ = parity::parse(rest);
        }
        5 => {
            let _ = cbor::decode::<RepoConfig>(rest);
        }
        _ => {
            let _ = cbor::decode::<ChunkerParams>(rest);
        }
    }
});
