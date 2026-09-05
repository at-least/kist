//! 所有 metadata 共用的 CBOR 編解碼（規範編碼，`docs/format.md` §4）。
//!
//! 規則：
//! - struct 依「規格中各結構的欄位表順序」輸出——serde 的欄位宣告順序
//!   就是那個順序，所以這裡**不做任何排序**（曾用 Value 層排序正規化，
//!   實測慢 20 倍；欄位表釘順序後兩個實作都零成本）。
//! - 解碼忽略未知欄位（向前相容），但拒絕重複 map key 與尾端多餘 bytes。
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

/// 從 CBOR bytes 解出結構。多餘的尾端 bytes 與重複 map key 視為錯誤。
pub fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    let mut cursor = std::io::Cursor::new(bytes);
    let value: Value = ciborium::from_reader(&mut cursor).map_err(|e| FormatError::Decode(e.to_string()))?;
    let consumed = usize::try_from(cursor.position()).unwrap_or(usize::MAX);
    if consumed != bytes.len() {
        return Err(FormatError::Decode(format!(
            "{} trailing bytes after CBOR value",
            bytes.len().saturating_sub(consumed)
        )));
    }
    reject_duplicate_keys(&value)?;
    ciborium::from_reader(std::io::Cursor::new(bytes)).map_err(|e| FormatError::Decode(e.to_string()))
}

use ciborium::value::Value;

fn encoded_key(k: &Value) -> Result<Vec<u8>> {
    let mut enc = Vec::new();
    ciborium::into_writer(k, &mut enc).map_err(|e| FormatError::Encode(e.to_string()))?;
    Ok(enc)
}

/// 拒絕重複的 map key：同一份文件出現兩個讀法是偽造或損壞，不是相容性。
fn reject_duplicate_keys(v: &Value) -> Result<()> {
    match v {
        Value::Map(entries) => {
            let mut keys: Vec<Vec<u8>> = Vec::with_capacity(entries.len());
            for (k, val) in entries {
                keys.push(encoded_key(k).map_err(|e| FormatError::Decode(e.to_string()))?);
                reject_duplicate_keys(val)?;
            }
            keys.sort();
            for pair in keys.windows(2) {
                if pair[0] == pair[1] {
                    return Err(FormatError::Decode("duplicate map key".to_owned()));
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                reject_duplicate_keys(item)?;
            }
        }
        _ => {}
    }
    Ok(())
}
