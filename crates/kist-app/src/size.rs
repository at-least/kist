//! `<number>[k|m|g|t]` 形式的大小，以 1024 為底、單位不分大小寫（同 restic 的
//! `--exclude-larger-than`）。設定檔與 CLI 共用。

pub fn parse_size(s: &str) -> std::result::Result<u64, String> {
    let s = s.trim();
    let split = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    let n: u64 = num
        .parse()
        .map_err(|_| format!("{s:?}: not a size (a number of bytes, or with k, m, g, t)"))?;
    let shift = match unit {
        "" => 0,
        "k" | "K" => 10,
        "m" | "M" => 20,
        "g" | "G" => 30,
        "t" | "T" => 40,
        _ => return Err(format!("{s:?}: unknown unit {unit:?} (use k, m, g, t)")),
    };
    n.checked_mul(1u64 << shift)
        .ok_or_else(|| format!("{s:?}: too large"))
}

/// serde 用：設定檔裡的 `"100M"`，或直接寫 bytes 的整數（審查修正：原本整數是載入錯誤）。
pub mod serde_opt {
    use serde::de::{Error, Unexpected, Visitor};
    use serde::Deserializer;

    struct Size;

    impl Visitor<'_> for Size {
        type Value = u64;

        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("a size: a number of bytes, or a string such as \"100M\"")
        }

        fn visit_u64<E: Error>(self, n: u64) -> Result<u64, E> {
            Ok(n)
        }

        fn visit_i64<E: Error>(self, n: i64) -> Result<u64, E> {
            u64::try_from(n).map_err(|_| E::invalid_value(Unexpected::Signed(n), &self))
        }

        fn visit_str<E: Error>(self, s: &str) -> Result<u64, E> {
            super::parse_size(s).map_err(E::custom)
        }
    }

    /// 欄位有 `#[serde(default)]`：沒寫是 `None`，寫了才會走到這裡。
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<u64>, D::Error> {
        d.deserialize_any(Size).map(Some)
    }
}

#[cfg(test)]
// 測試裡 unwrap／expect 失敗就是測試失敗，正是要的結果。
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn parse_size_table() {
        assert_eq!(parse_size("0"), Ok(0));
        assert_eq!(parse_size("512"), Ok(512));
        assert_eq!(parse_size("1k"), Ok(1024));
        assert_eq!(parse_size("2K"), Ok(2048));
        assert_eq!(parse_size("100M"), Ok(100 << 20));
        assert_eq!(parse_size("1g"), Ok(1 << 30));
        assert_eq!(parse_size(" 3T "), Ok(3 << 40));
        // 小數、帶 B 的寫法、空的、單位在前、負數、未知單位：全拒
        for bad in ["1.5M", "1MB", "1MiB", "", "M", "k1", "-1", "1x", "1 M"] {
            assert!(parse_size(bad).is_err(), "{bad:?} 不該被接受");
        }
        assert!(parse_size("18446744073709551615k").is_err(), "溢出要報錯");
    }
}
