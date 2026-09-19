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
    reject_noncanonical(bytes)?;
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

/// 規範編碼的結構守門（§4 第 2、4 條）：解碼前走一遍 CBOR **結構**，
/// 拒絕 indefinite length（任何 major 的 additional info 31）與 tag
/// （major 6）。ciborium 對兩者靜默容受，Go 端則明文拒絕——同一份
/// bytes 不能在兩個實作得到不同判斷。
///
/// 只走結構不解值：byte/text string 的 payload 整段跳過（內容裡的
/// 0xbf/0xc0 是位元組，不是標頭）；array/map 遞迴，深度上限防堆疊
/// （wire 型別巢狀極淺）。順帶擋掉保留的 additional info 28–30 與
/// 截斷的輸入（正規解碼器對兩者的行為不一而足）。
fn reject_noncanonical(bytes: &[u8]) -> Result<()> {
    let mut pos = 0usize;
    walk_structure(bytes, &mut pos, 0)?;
    if pos != bytes.len() {
        return Err(FormatError::Decode(format!(
            "{} trailing bytes after CBOR value",
            bytes.len() - pos
        )));
    }
    Ok(())
}

/// 巢狀深度上限：wire 型別最深的結構（index blob 的 packs → entries）
/// 只有個位數層；超過就是攻擊。
const MAX_STRUCTURE_DEPTH: usize = 64;

fn walk_structure(data: &[u8], pos: &mut usize, depth: usize) -> Result<()> {
    if depth > MAX_STRUCTURE_DEPTH {
        return Err(FormatError::Decode("CBOR nesting too deep".to_owned()));
    }
    let bad = |what: &str| Err(FormatError::Decode(what.to_owned()));
    let Some(&initial) = data.get(*pos) else {
        return bad("truncated CBOR item");
    };
    let major = initial >> 5;
    let info = initial & 0x1f;
    *pos += 1;

    let read_uint = |pos: &mut usize, n: usize| -> Result<u64> {
        if data.len() < *pos + n {
            return Err(FormatError::Decode("truncated CBOR length".to_owned()));
        }
        let mut v = 0u64;
        for &b in &data[*pos..*pos + n] {
            v = (v << 8) | u64::from(b);
        }
        *pos += n;
        Ok(v)
    };

    let argument = match info {
        0..=23 => u64::from(info),
        24 => read_uint(pos, 1)?,
        25 => read_uint(pos, 2)?,
        26 => read_uint(pos, 4)?,
        27 => read_uint(pos, 8)?,
        31 => return bad("indefinite length is not canonical CBOR"),
        _ => return bad("reserved additional info in CBOR item"),
    };

    match major {
        // 整數與 simple/float：argument 就是值本身，已消費完畢。
        0 | 1 | 7 => Ok(()),
        // byte/text string：payload 整段跳過（內容位元組不是結構）。
        2 | 3 => {
            let end = argument as usize;
            if data.len() < pos.saturating_add(end) {
                return bad("truncated CBOR string payload");
            }
            *pos = pos.saturating_add(end);
            Ok(())
        }
        4 => {
            for _ in 0..argument {
                walk_structure(data, pos, depth + 1)?;
            }
            Ok(())
        }
        5 => {
            for _ in 0..argument {
                walk_structure(data, pos, depth + 1)?;
                walk_structure(data, pos, depth + 1)?;
            }
            Ok(())
        }
        _ => bad("CBOR tag is not canonical"),
    }
}
