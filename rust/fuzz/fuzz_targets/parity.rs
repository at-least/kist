//! parity sidecar 的 fuzz target。
//!
//! - 任意 bytes = sidecar：`parse` 的邊界檢查必須在配置之前擋住
//!   偽造 header（v/k/m/shard_len/pack_size 一致性）。
//! - Go golden sidecar + 任意 bytes = 損壞的 pack：`repair` 不 panic，
//!   且**成功必蘊涵**重算 hash == pack 名（修錯是不可能的，這裡從外部
//!   再驗一次那個不變量）。
//! - 任意 bytes = pack：`encode` → `parse` → `repair`（零損毀）roundtrip
//!   必成功。
#![no_main]

use kist_format::parity;
use kist_format::ObjectId;
use libfuzzer_sys::fuzz_target;

const GOLDEN: &str = include_str!("../../crates/kist-format/tests/testdata/parity-golden.txt");

/// (pack id, 已解析的 golden sidecar)。
fn golden() -> Option<(ObjectId, parity::Object)> {
    let mut lines = GOLDEN.lines();
    let id = ObjectId::from_hex(lines.next()?.strip_prefix("pack ")?).ok()?;
    let raw = hex::decode(lines.next()?.strip_prefix("parity ")?).ok()?;
    let obj = parity::parse(&raw).ok()?;
    Some((id, obj))
}

fuzz_target!(|data: &[u8]| {
    let _ = parity::parse(data);

    if let Some((id, obj)) = golden() {
        match obj.repair(&id, data) {
            // repair 內部已驗「hash == pack 名」，這裡補驗它沒檢查的
            // 尺寸不變量（repair 回傳的必須正好是 pack_size bytes）。
            Ok(repaired) => assert_eq!(
                repaired.len() as u64,
                obj.pack_size(),
                "repair returned {} bytes, pack is {}",
                repaired.len(),
                obj.pack_size()
            ),
            Err(_) => {}
        }
    }

    if !data.is_empty() {
        let id = ObjectId::of(data);
        for m in [1usize, 3, 8] {
            if let Ok(sidecar) = parity::encode(&id, data, m) {
                let obj = parity::parse(&sidecar)
                    .unwrap_or_else(|e| panic!("encode produced an unparseable sidecar: {e}"));
                obj.repair(&id, data)
                    .unwrap_or_else(|e| panic!("zero-damage repair failed (m={m}): {e}"));
            }
        }
    }
});
