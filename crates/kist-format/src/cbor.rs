//! 所有 metadata 共用的規範 CBOR 編解碼（RFC 8949 Core Deterministic）。
//!
//! 規則（`docs/format.md` §4）：
//! - 整數最短編碼、長度 definite；**map 的 key 依「編碼後的 key bytes」
//!   字典序排序**——這讓 Go（fxamacker CoreDet）與 Rust 對同一份資料
//!   編出逐 byte 相同的輸出，與欄位宣告順序無關。
//! - 解碼忽略未知欄位（向前相容），但拒絕重複 map key 與尾端多餘 bytes。
//! - **絕不回寫**：讀出的物件不得重新編碼後寫回；寫入端一律從事實來源
//!   重新構造。
//!
//! 實作：先用 ciborium 序列化成 `Value`，在 Value 層遞迴排序 map entries
//! （key 以其 CBOR 編碼 bytes 比較），再輸出。ciborium 的整數輸出本來就是
//! 最短形式，與 fxamacker 的 CoreDet 一致（跨語言 golden 測試釘死）。

use ciborium::value::Value;
use serde::{de::DeserializeOwned, Serialize};

use crate::{FormatError, Result};

/// 把任何可序列化的結構編成規範 CBOR bytes。
pub fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    let mut raw = Vec::new();
    ciborium::into_writer(value, &mut raw).map_err(|e| FormatError::Encode(e.to_string()))?;
    let parsed: Value = ciborium::from_reader(std::io::Cursor::new(&raw))
        .map_err(|e| FormatError::Encode(e.to_string()))?;
    let canonical = canonicalize(parsed)?;
    let mut out = Vec::with_capacity(raw.len());
    ciborium::into_writer(&canonical, &mut out).map_err(|e| FormatError::Encode(e.to_string()))?;
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

fn encoded_key(k: &Value) -> Result<Vec<u8>> {
    let mut enc = Vec::new();
    ciborium::into_writer(k, &mut enc).map_err(|e| FormatError::Encode(e.to_string()))?;
    Ok(enc)
}

/// 遞迴把每個 map 的 entries 依「編碼後的 key bytes」排序（RFC 8949 §4.2.1）。
fn canonicalize(v: Value) -> Result<Value> {
    match v {
        Value::Map(entries) => {
            let mut keyed: Vec<(Vec<u8>, Value, Value)> = Vec::with_capacity(entries.len());
            for (k, val) in entries {
                let enc = encoded_key(&k)?;
                keyed.push((enc, k, canonicalize(val)?));
            }
            keyed.sort_by(|a, b| a.0.cmp(&b.0));
            Ok(Value::Map(
                keyed.into_iter().map(|(_, k, val)| (k, val)).collect(),
            ))
        }
        Value::Array(items) => Ok(Value::Array(
            items.into_iter().map(canonicalize).collect::<Result<Vec<_>>>()?,
        )),
        other => Ok(other),
    }
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
