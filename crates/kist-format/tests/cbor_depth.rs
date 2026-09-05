//! ciborium 的 RecursionLimitExceeded 是 kist-format CBOR 解碼對深巢狀
//! 輸入的第一道防線（`cbor::decode` 會先把 bytes 解成 `Value` 再比對
//! 目標型別，遞迴結構就發生在那裡）。這個測試釘住該防線：若換掉或
//! 升級 ciborium 後限制消失，2M 層巢狀會在這裡爆 stack，fuzz target
//! `fuzz/fuzz_targets/cbor.rs` 也會立刻抓到。
use kist_format::cbor;

#[test]
fn deeply_nested_cbor_is_rejected_not_crashing() {
    let depth = 2_000_000usize;
    let mut bytes = vec![0x81u8; depth]; // 0x81 = 1-element array
    bytes.push(0x00);
    let r: Result<serde_bytes::ByteBuf, _> = cbor::decode(&bytes);
    let err = r
        .expect_err("deeply nested CBOR must not decode")
        .to_string();
    assert!(
        err.contains("RecursionLimit"),
        "expected the recursion limit to reject it, got: {err}"
    );
}
