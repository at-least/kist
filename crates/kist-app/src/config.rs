//! 設定檔（TOML）。一個檔描述一個 repo 與要對它做的事：
//!
//! ```toml
//! repo = "s3://bucket/prefix"          # 或本機目錄、sftp://…（M4-c）
//! password_file = "/etc/kist/password"  # 只能用檔案，不能把密碼寫在設定裡
//! client_id_file = "/var/lib/kist/client-id"   # 選填
//! cache_dir = "/var/cache/kist"                # 選填
//! timezone = "local"                           # 或 "utc"；排程用哪個時區
//!
//! [backup]
//! paths = ["/home", "/etc"]
//! schedule = "0 2 * * *"      # 5 或 6 欄（秒選填）cron；省略 = 只在 `run --once` 時跑
//! gc_grace = "72h"
//!
//! [forget]                     # 選填；需要 Delete 權限，建議放在維護主機的設定裡
//! keep_daily = 7
//! keep_weekly = 4
//! schedule = "0 4 * * *"
//!
//! [prune]                      # 選填；同上
//! grace = "72h"
//! inactive_after = "30d"
//! repack_below = 50
//! schedule = "0 5 * * *"
//!
//! [notify]
//! webhook_url = "https://example.com/hook"
//! on = ["failure", "incomplete"]   # 也可以加 "success"
//!
//! [serve]                      # 選填；`kist serve` 的 Web UI
//! password_file = "/etc/kist/ui-password"       # 設了：整個 UI 都要 HTTP Basic auth（帳號任意，只比密碼）
//! allowed_hosts = ["backup.example.internal:9898"]  # UI 額外接受的 Host header 值（loopback 形式自動允許）
//! ```
//!
//! backup 主機只放 `[backup]`（憑證只要 Put/Get/List）；`[forget]` / `[prune]` 放在另一台
//! 有 Delete 權限的維護主機——這是抗勒索設計的一部分，不是限制。

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::duration::parse_duration;
use crate::schedule::{Schedule, Timezone};
use crate::{AppError, Result};

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub repo: String,
    pub password_file: PathBuf,
    #[serde(default)]
    pub client_id_file: Option<PathBuf>,
    #[serde(default)]
    pub cache_dir: Option<PathBuf>,
    #[serde(default)]
    pub timezone: Timezone,
    #[serde(default)]
    pub backup: Option<BackupSection>,
    #[serde(default)]
    pub forget: Option<ForgetSection>,
    #[serde(default)]
    pub prune: Option<PruneSection>,
    #[serde(default)]
    pub notify: Option<NotifySection>,
    #[serde(default)]
    pub serve: Option<ServeSection>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackupSection {
    pub paths: Vec<PathBuf>,
    #[serde(default)]
    pub schedule: Option<String>,
    #[serde(default, with = "crate::duration::serde_opt")]
    pub gc_grace: Option<std::time::Duration>,
    /// 每個 pack 旁存幾片 Reed-Solomon 同位（0..=8；0 = 不存，預設）。
    #[serde(default)]
    pub parity: u8,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ForgetSection {
    pub keep_last: Option<u32>,
    pub keep_hourly: Option<u32>,
    pub keep_daily: Option<u32>,
    pub keep_weekly: Option<u32>,
    pub keep_monthly: Option<u32>,
    pub keep_yearly: Option<u32>,
    #[serde(default, with = "crate::duration::serde_opt")]
    pub keep_within: Option<std::time::Duration>,
    #[serde(default)]
    pub schedule: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PruneSection {
    #[serde(default, with = "crate::duration::serde_opt")]
    pub grace: Option<std::time::Duration>,
    /// Tolerated client/prune clock difference; 0 defaults to 1h.
    pub clock_skew: Option<std::time::Duration>,
    #[serde(default, with = "crate::duration::serde_opt")]
    pub inactive_after: Option<std::time::Duration>,
    pub repack_below: Option<u8>,
    #[serde(default)]
    pub schedule: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NotifySection {
    pub webhook_url: String,
    /// 哪些結果要通知：success / incomplete / failure。預設 failure 與 incomplete。
    #[serde(default = "default_notify_on")]
    pub on: Vec<String>,
}

fn default_notify_on() -> Vec<String> {
    vec!["failure".to_owned(), "incomplete".to_owned()]
}

/// `kist serve` 的 Web UI 設定。兩個欄位都選填；只有 `[serve]` 而沒有排程的設定也合法
/// （UI-only 的 serve：手動按「Run backup now」）。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServeSection {
    /// UI 密碼檔（跟 repo 密碼是不同的秘密）。設了：整個 UI 都要 HTTP Basic auth，
    /// 帳號任意、只比密碼。沒設：UI 唯讀，「Run backup now」被停用。
    #[serde(default)]
    pub password_file: Option<PathBuf>,
    /// UI 額外接受的 `Host` header 值（防 DNS rebinding）；綁定位址與 loopback 形式自動允許。
    /// 逐字比對，不折疊大小寫、不補預設埠。
    #[serde(default)]
    pub allowed_hosts: Vec<String>,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).map_err(|e| AppError::Io {
            path: path.to_path_buf(),
            source: e,
        })?;
        let cfg = Self::parse(&text)?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn parse(text: &str) -> Result<Self> {
        toml::from_str(text).map_err(|e| AppError::Config(e.to_string()))
    }

    /// 除了型別以外的規則：路徑非空、cron 合法、保留政策合法、通知事件名稱合法。
    pub fn validate(&self) -> Result<()> {
        if self.repo.trim().is_empty() {
            return Err(AppError::Config("repo must not be empty".to_owned()));
        }
        if let Some(b) = &self.backup {
            if b.paths.is_empty() {
                return Err(AppError::Config(
                    "[backup] paths must not be empty".to_owned(),
                ));
            }
            self.schedule_of(b.schedule.as_deref(), "[backup]")?;
            if b.parity > 8 {
                return Err(AppError::Config("[backup] parity must be 0..=8".to_owned()));
            }
        }
        if let Some(f) = &self.forget {
            let policy = f.policy();
            if policy.is_empty() {
                return Err(AppError::Config(
                    "[forget] needs at least one keep_* rule".to_owned(),
                ));
            }
            policy
                .validate()
                .map_err(|e| AppError::Config(e.to_string()))?;
            self.schedule_of(f.schedule.as_deref(), "[forget]")?;
        }
        if let Some(p) = &self.prune {
            if p.repack_below.is_some_and(|v| v > 100) {
                return Err(AppError::Config(
                    "[prune] repack_below must be 0..=100".to_owned(),
                ));
            }
            self.schedule_of(p.schedule.as_deref(), "[prune]")?;
        }
        if let Some(n) = &self.notify {
            for ev in &n.on {
                if !matches!(ev.as_str(), "success" | "incomplete" | "failure") {
                    return Err(AppError::Config(format!(
                        "[notify] on: unknown event {ev:?} (use success, incomplete, failure)"
                    )));
                }
            }
            if !(n.webhook_url.starts_with("http://") || n.webhook_url.starts_with("https://")) {
                return Err(AppError::Config(
                    "[notify] webhook_url must start with http:// or https://".to_owned(),
                ));
            }
        }
        if self.backup.is_none() && self.forget.is_none() && self.prune.is_none() {
            return Err(AppError::Config(
                "nothing to do: add a [backup], [forget] or [prune] section".to_owned(),
            ));
        }
        Ok(())
    }

    /// 某一節的排程（解析過的）；沒寫 schedule 就 `None`。
    pub fn schedule_of(&self, expr: Option<&str>, section: &str) -> Result<Option<Schedule>> {
        expr.map(|e| {
            Schedule::parse(e, self.timezone)
                .map_err(|err| AppError::Config(format!("{section} schedule {e:?}: {err}")))
        })
        .transpose()
    }

    /// 讀 repo 密碼檔（第一行）。
    pub fn read_password(&self) -> Result<zeroize::Zeroizing<String>> {
        read_password_file(&self.password_file)
    }

    /// 讀 Web UI 密碼檔（`[serve] password_file`，第一行）。沒設定 → `Ok(None)`。
    /// 啟動時讀一次，之後留在記憶體裡（`Zeroizing`）。
    pub fn read_ui_password(&self) -> Result<Option<zeroize::Zeroizing<String>>> {
        match self.serve.as_ref().and_then(|s| s.password_file.as_deref()) {
            Some(path) => read_password_file(path).map(Some),
            None => Ok(None),
        }
    }
}

/// 密碼檔的規則（repo 密碼與 UI 密碼共用）：取第一行、去掉 `\r`，空的算設定錯誤。
fn read_password_file(path: &Path) -> Result<zeroize::Zeroizing<String>> {
    let text = std::fs::read_to_string(path).map_err(|e| AppError::Io {
        path: path.to_path_buf(),
        source: e,
    })?;
    let first = text.lines().next().unwrap_or("").trim_end_matches('\r');
    if first.is_empty() {
        return Err(AppError::Config(format!(
            "password file {} is empty",
            path.display()
        )));
    }
    Ok(zeroize::Zeroizing::new(first.to_owned()))
}

impl ForgetSection {
    pub fn policy(&self) -> kist_core::RetentionPolicy {
        kist_core::RetentionPolicy {
            keep_last: self.keep_last,
            keep_hourly: self.keep_hourly,
            keep_daily: self.keep_daily,
            keep_weekly: self.keep_weekly,
            keep_monthly: self.keep_monthly,
            keep_yearly: self.keep_yearly,
            keep_within: self.keep_within,
        }
    }
}

impl PruneSection {
    pub fn options(&self) -> kist_core::PruneOptions {
        let d = kist_core::PruneOptions::default();
        kist_core::PruneOptions {
            grace: self.grace.unwrap_or(d.grace),
            inactive_after: self.inactive_after.unwrap_or(d.inactive_after),
            clock_skew: self.clock_skew.unwrap_or(d.clock_skew),
            repack_below_percent: self.repack_below.unwrap_or(d.repack_below_percent),
            dry_run: false,
            now: None,
        }
    }
}

/// 給 CLI 用：字串形式的時間長度也走同一個解析。
pub fn duration(s: &str) -> Result<std::time::Duration> {
    parse_duration(s).map_err(AppError::Config)
}
