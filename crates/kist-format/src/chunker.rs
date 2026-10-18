//! FastCDC 的參數（`docs/format-v4.md` §6.1）：`kist-chunker` 吃它，v4 的 config 把 `chunker`
//! 欄位轉成它（[`crate::v4::config::Chunker`] 是 config 裡的形狀，欄位相同）。

use serde::{Deserialize, Serialize};

use crate::{FormatError, Result};

/// FastCDC 參數（bytes）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChunkerParams {
    #[serde(rename = "min")]
    pub min: u32,
    #[serde(rename = "avg")]
    pub avg: u32,
    #[serde(rename = "max")]
    pub max: u32,
}

impl ChunkerParams {
    /// 切塊參數的硬性限制：越界值會讓掃描越界或配置巨量記憶體。與 `v4::config::Chunker::validate`
    /// 相同的範圍。
    pub fn validate(&self) -> Result<()> {
        let bad = |msg: String| Err(FormatError::InvalidParams(msg));
        if self.min < 64 || self.min > 1024 * 1024 {
            return bad(format!("chunker.min {} is outside 64..=1 MiB", self.min));
        }
        if self.avg < 256 || self.avg > 16 * 1024 * 1024 {
            return bad(format!("chunker.avg {} is outside 256..=16 MiB", self.avg));
        }
        if self.max < 1024 || self.max > 64 * 1024 * 1024 {
            return bad(format!(
                "chunker.max {} is outside 1 KiB..=64 MiB",
                self.max
            ));
        }
        if !(self.min <= self.avg && self.avg <= self.max) {
            return bad(format!(
                "chunker sizes must satisfy min <= avg <= max, got {}/{}/{}",
                self.min, self.avg, self.max
            ));
        }
        Ok(())
    }
}

impl Default for ChunkerParams {
    fn default() -> Self {
        Self {
            min: 512 * 1024,
            avg: 2 * 1024 * 1024,
            max: 8 * 1024 * 1024,
        }
    }
}
