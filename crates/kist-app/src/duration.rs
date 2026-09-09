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
