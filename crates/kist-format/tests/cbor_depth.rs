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

/// §4 第 2、4 條：indefinite length 與 tag 都是解碼拒絕。ciborium 對兩者
/// 靜默容受（indef 照解、tag 跳過），Go 端（fxamacker
/// IndefLengthForbidden + TagsForbidden）則拒絕——同一份 bytes 不能在
/// 兩個實作得到不同判斷。
#[test]
fn indefinite_lengths_and_tags_are_rejected() {
    #[derive(serde::Deserialize)]
    struct S {
        a: u64,
    }
    // map(1){ "a": 5 } 的 indefinite 版本：0xbf … 0xff。ciborium 照解。
    let indef = [0xbfu8, 0x61, b'a', 0x05, 0xff];
    let r: Result<S, _> = cbor::decode(&indef);
    assert!(r.is_err(), "indefinite-length map must be rejected");

    // tag(1) 包住整數 5：ciborium 跳過 tag 照解。
    let tagged = [0xc1u8, 0x05];
    let r: Result<u64, _> = cbor::decode(&tagged);
    assert!(r.is_err(), "a tagged value must be rejected");

    // 事實來源的規範編碼不受影響。
    let ok = [0xa1u8, 0x61, b'a', 0x05];
    let s: S = cbor::decode(&ok).unwrap();
    assert_eq!(s.a, 5);
}
