//! 所有 metadata 共用的 CBOR 編解碼。
//!
//! 規則：
//! - struct 以「欄位順序」寫成 map，key 是欄位名稱字串（self-describing）。
//! - 解碼時忽略未知欄位，這是向前相容的基礎：新版本可以加欄位，舊版本照樣能讀。
//! - 不用 `HashMap`：同一份資料必須永遠編出同一串 bytes（tree 重用依賴這點）。

use serde::{de::DeserializeOwned, Serialize};

use crate::{FormatError, Result};

/// 把任何可序列化的結構編成 CBOR bytes。
pub fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    ciborium::into_writer(value, &mut buf).map_err(|e| FormatError::Encode(e.to_string()))?;
    Ok(buf)
}

/// 從 CBOR bytes 解出結構。多餘的尾端 bytes 視為錯誤。
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
