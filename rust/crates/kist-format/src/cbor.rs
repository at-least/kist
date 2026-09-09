//! 所有 metadata 共用的 CBOR 編解碼（規範編碼，`docs/format.md` §4）。
//!
//! 規則：
//! - struct 依「規格中各結構的欄位表順序」輸出——serde 的欄位宣告順序
//!   就是那個順序，所以這裡**不做任何排序**（曾用 Value 層排序正規化，
//!   實測慢 20 倍；欄位表釘順序後兩個實作都零成本）。
//! - 解碼忽略未知欄位（向前相容），拒絕重複欄位（serde struct visitor）
//!   與尾端多餘 bytes。wire 型別都是 struct，直接解進 `T` 不走 Value 中繼。
//! - **絕不回寫**：讀出的物件不得重新編碼後寫回；寫入端一律從事實來源
//!   重新構造。
//!
//! 守則：**struct 欄位的宣告順序是格式**。重排欄位 = 改變 tree ID 等所有
//! 內容定址；golden 測試與跨語言向量（Go `internal/interop`）會抓到。

use serde::{de::DeserializeOwned, Serialize};

use crate::{FormatError, Result};

/// 把任何可序列化的結構編成規範 CBOR bytes。
pub fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    ciborium::into_writer(value, &mut out).map_err(|e| FormatError::Encode(e.to_string()))?;
    Ok(out)
}

/// 同 [`encode`]，但把 CBOR 串流寫進 `writer` 而不物化整份明文。
/// 給 index blob 這種超大結構用（1M chunk ≈ 56 MiB）：輸出與
/// [`encode`] 完全相同，只是編碼過程直接進 writer。
pub fn encode_to_writer<T: Serialize, W: std::io::Write>(value: &T, mut writer: W) -> Result<()> {
    ciborium::into_writer(value, &mut writer).map_err(|e| FormatError::Encode(e.to_string()))?;
    Ok(())
}

/// 從 CBOR bytes 解出結構。多餘的尾端 bytes 視為錯誤。
///
/// 直接解進 `T`，**不走 `ciborium::value::Value` 中繼**：那會把每個
/// 整數/byte string 都裝箱成通用值樹（100 萬 chunk 的 index blob 多出
/// ~280 MiB 的暫存垃圾）。重複欄位的拒絕由 serde 的 struct visitor
/// 提供（wire 上的型別都是 struct；`decode::<Value>` 不受此保護，
/// 但 kist 不用 Value 當 wire 型別）。
pub fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    let mut cursor = std::io::Cursor::new(bytes);
    let value: T =
        ciborium::from_reader(&mut cursor).map_err(|e| FormatError::Decode(e.to_string()))?;
    let consumed = usize::try_from(cursor.position()).unwrap_or(usize::MAX);
    if consumed != bytes.len() {
        return Err(FormatError::Decode(format!(
            "{} trailing bytes after CBOR value",
            bytes.len().saturating_sub(consumed)
        )));
    }
    Ok(value)
}
