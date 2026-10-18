//! v4 parity 的 fuzz target（`docs/format-v4.md` §13）。
//!
//! - 任意 bytes = parity 物件：`parse` 的邊界檢查必須在配置之前擋住偽造 header
//!   （v/k/m/shard_len/pack_size 一致性）。
//! - 凍結的 golden（`tests/golden-v4/parity.cbor`）+ 任意 bytes = 損壞的 pack：`repair` 不 panic，
//!   且**成功必蘊涵**重算 hash == pack 名（修錯是不可能的，這裡從外部再驗一次那個不變量）。
//! - 任意 bytes = pack：`encode` → `parse` → `repair`（零損毀）roundtrip 必成功。
#![no_main]

use kist_format::v4::ids::ObjectId;
use kist_format::v4::parity;
use libfuzzer_sys::fuzz_target;

const GOLDEN: &[u8] = include_bytes!("../../crates/kist-format/tests/golden-v4/parity.cbor");

/// golden 的 pack：`"kist/test/golden"` 的 BLAKE3 XOF 流，4321 bytes（與 golden_v4.rs 相同）。
fn golden_pack() -> Vec<u8> {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"kist/test/golden");
    let mut out = vec![0u8; 4321];
    hasher.finalize_xof().fill(&mut out);
    out
}

/// (pack id, 已解析的 golden parity)。
fn golden() -> Option<(ObjectId, parity::Object)> {
    let obj = parity::parse(GOLDEN).ok()?;
    Some((ObjectId::of(&golden_pack()), obj))
}

fuzz_target!(|data: &[u8]| {
    let _ = parity::parse(data);

    if let Some((id, obj)) = golden() {
        if let Ok(repaired) = obj.repair(&id, data) {
            // repair 內部已驗「hash == pack 名」，這裡補驗它沒檢查的尺寸不變量
            // （repair 回傳的必須正好是 pack_size bytes）。
            assert_eq!(
                repaired.len() as u64,
                obj.pack_size,
                "repair returned {} bytes, pack is {}",
                repaired.len(),
                obj.pack_size
            );
            assert_eq!(ObjectId::of(&repaired), id, "repair returned bytes with another name");
        }
    }

    if !data.is_empty() {
        let id = ObjectId::of(data);
        for m in [1usize, 3, 8] {
            if let Ok(raw) = parity::encode(&id, data, m) {
                let obj = parity::parse(&raw)
                    .unwrap_or_else(|e| panic!("encode produced an unparseable object: {e}"));
                obj.repair(&id, data)
                    .unwrap_or_else(|e| panic!("zero-damage repair failed (m={m}): {e}"));
            }
        }
    }
});
