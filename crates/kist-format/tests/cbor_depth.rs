//! 深巢狀 CBOR 輸入不得爆 stack。`cbor::decode` 直接解進目標型別
//! （不走 Value 中繼），遞迴深度由**目標型別的結構深度**決定——wire 型別
//! 都是淺層 struct，2M 層的巢狀陣列在型別不符處立刻被拒，不需要資源
//! 限制也不會遞迴。若有人把 wire 型別改成遞迴結構，fuzz target
//! `fuzz/fuzz_targets/cbor.rs` 與這個測試會立刻抓到。
use kist_format::cbor;

#[test]
fn deeply_nested_cbor_is_rejected_not_crashing() {
    let depth = 2_000_000usize;
    let mut bytes = vec![0x81u8; depth]; // 0x81 = 1-element array
    bytes.push(0x00);
    let r: Result<serde_bytes::ByteBuf, _> = cbor::decode(&bytes);
    assert!(
        r.is_err(),
        "deeply nested CBOR must be rejected (as a type mismatch), not decoded or crashing"
    );
}
