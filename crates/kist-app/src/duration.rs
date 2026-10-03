//! `<number><unit>` 形式的時間長度：s / m / h / d / w。設定檔與 CLI 共用。

use std::time::Duration;

pub fn parse_duration(s: &str) -> std::result::Result<Duration, String> {
    let s = s.trim();
    let split = s
        .find(|c: char| !c.is_ascii_digit())
        .ok_or_else(|| format!("{s:?}: missing unit (s, m, h, d, w)"))?;
    let (num, unit) = s.split_at(split);
    let n: u64 = num.parse().map_err(|_| format!("{s:?}: not a number"))?;
    let secs = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86_400,
        "w" => 7 * 86_400,
        _ => return Err(format!("{s:?}: unknown unit {unit:?} (use s, m, h, d, w)")),
    };
    n.checked_mul(secs)
        .map(Duration::from_secs)
        .ok_or_else(|| format!("{s:?}: too large"))
}

/// serde 用：設定檔裡的 `"72h"` → Duration。
pub mod serde_opt {
    use serde::{Deserialize, Deserializer};

    pub fn deserialize<'de, D: Deserializer<'de>>(
        d: D,
    ) -> Result<Option<std::time::Duration>, D::Error> {
        let s: Option<String> = Option::deserialize(d)?;
        s.map(|s| super::parse_duration(&s).map_err(serde::de::Error::custom))
            .transpose()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// 釘住整張解析表：prune 的 grace/inactive_after 從這裡進來，"72h" 與
    /// "72" 的差別就是 prune 能不能刪東西——默默接受無單位數字或拆錯
    /// 複合單位都不許。
    #[test]
    fn parse_duration_table() {
        assert_eq!(parse_duration("72h"), Ok(Duration::from_secs(72 * 3600)));
        assert_eq!(parse_duration("30d"), Ok(Duration::from_secs(30 * 86_400)));
        assert_eq!(parse_duration("90s"), Ok(Duration::from_secs(90)));
        assert_eq!(parse_duration("5m"), Ok(Duration::from_secs(300)));
        assert_eq!(parse_duration("2w"), Ok(Duration::from_secs(14 * 86_400)));
        assert_eq!(parse_duration(" 1h "), Ok(Duration::from_secs(3600)));
        // 沒有單位、單位在前、小數、複合單位、未知單位、負數：全拒
        for bad in ["72", "h", "s5", "1.5h", "1h30m", "5H", "-5s", ""] {
            assert!(parse_duration(bad).is_err(), "{bad:?} 不該被接受");
        }
        // u64 溢出要報錯，不能繞回小值
        assert!(parse_duration("18446744073709551615w").is_err());
    }
}
