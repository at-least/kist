//! cron 排程：5 欄（分 時 日 月 週）或 6 欄（前面多一欄秒）。用 `croner` 解析與算下一次。

use croner::parser::{CronParser, Seconds};
use croner::Cron;
use serde::Deserialize;
use time::OffsetDateTime;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Timezone {
    #[default]
    Local,
    Utc,
}

#[derive(Debug, Clone)]
pub struct Schedule {
    cron: Cron,
    tz: Timezone,
    expr: String,
}

impl Schedule {
    pub fn parse(expr: &str, tz: Timezone) -> Result<Self, String> {
        let cron = CronParser::builder()
            .seconds(Seconds::Optional)
            .build()
            .parse(expr)
            .map_err(|e| e.to_string())?;
        Ok(Self {
            cron,
            tz,
            expr: expr.to_owned(),
        })
    }

    pub fn expr(&self) -> &str {
        &self.expr
    }

    /// 嚴格晚於 `after` 的下一次執行時間。找不到（例如 2 月 30 日）回 `None`。
    pub fn next_after(&self, after: OffsetDateTime) -> Option<OffsetDateTime> {
        let secs = after.unix_timestamp();
        let nanos = after.nanosecond();
        let utc = chrono::DateTime::<chrono::Utc>::from_timestamp(secs, nanos)?;
        let next_secs = match self.tz {
            Timezone::Utc => self
                .cron
                .find_next_occurrence(&utc, false)
                .ok()?
                .timestamp(),
            Timezone::Local => {
                let local = utc.with_timezone(&chrono::Local);
                self.cron
                    .find_next_occurrence(&local, false)
                    .ok()?
                    .timestamp()
            }
        };
        OffsetDateTime::from_unix_timestamp(next_secs).ok()
    }
}
