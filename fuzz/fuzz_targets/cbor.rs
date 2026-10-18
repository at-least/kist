//! v4 結構解碼的 fuzz target（`docs/format-v4.md` §4、§6、§7、§9、§10、§13）。
//!
//! 第一個 byte 選結構、其餘是 CBOR body。每一種讀取端驗證都吃得到，包括 ID 型別的
//! 長度檢查與 `v` 不是 4 的拒絕。解碼只需不 panic——格式錯誤回 Err 即為正確行為。
//! 已知防線：ciborium 有 RecursionLimitExceeded（tests/cbor_depth.rs 釘著）。
#![no_main]

use kist_format::v4::config::{Config, Slot};
use kist_format::v4::ids::{ClientId, ObjectId};
use kist_format::v4::index::{Delta, Head, Shard};
use kist_format::v4::keys::Key;
use kist_format::v4::meta::decode_item;
use kist_format::v4::pack::PackTrailer;
use kist_format::v4::parity;
use kist_format::v4::snapshot::Snapshot;

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Some((&selector, rest)) = data.split_first() else {
        return;
    };
    match selector % 9 {
        0 => {
            // trailer 的起點用輸入長度當作「檔尾算出來的值」：一致性檢查要走到底
            let key = Key::Pack {
                client: ClientId::from_bytes([1; 16]),
                id: ObjectId::ZERO,
            };
            let _ = PackTrailer::decode(rest, &key, rest.len() as u64, 8 << 20);
        }
        1 => {
            let _ = Head::decode(rest, 1);
        }
        2 => {
            let _ = Shard::decode(rest, 0);
        }
        3 => {
            let _ = Delta::decode(rest, &ClientId::from_bytes([1; 16]));
        }
        4 => {
            let _ = Snapshot::decode(rest);
        }
        5 => {
            let _ = Config::decode(rest);
        }
        6 => {
            let _ = Slot::decode(rest);
        }
        7 => {
            let _ = decode_item(rest);
        }
        _ => {
            let _ = parity::parse(rest);
        }
    }
});
