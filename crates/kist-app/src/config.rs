//! 設定檔（TOML）。一個檔描述一個 repo 與要對它做的事：
//!
//! ```toml
//! repo = "s3://bucket/prefix"          # 或本機目錄、sftp://…（M4-c）
//! password_file = "/etc/kist/password"  # 只能用檔案，不能把密碼寫在設定裡
//! client_id_file = "/var/lib/kist/client-id"   # 選填
//! cache_dir = "/var/cache/kist"                # 選填；預設是使用者的快取目錄（同 CLI）
//! timezone = "local"                           # 或 "utc"；排程用哪個時區
//!
//! [backup]
//! paths = ["/home", "/etc"]   # 或只有一個 sftp:// 或 s3:// URL：直接備份那個遠端來源（同 `kist backup`）
//! schedule = "0 2 * * *"      # 5 或 6 欄（秒選填）cron；省略 = 只在 `run --once` 時跑
//! exclude = ["*.tmp", "node_modules", "/home/me/.cache"]   # 選填；同 CLI 的 --exclude（ADR 020）
//! iexclude = ["*.bak"]                  # 選填；同 --iexclude（不分大小寫，排在 exclude 之後）
//! exclude_if_present = [".nobackup"]    # 選填；同 --exclude-if-present（NAME 或 NAME:HEADER）
//! exclude_caches = true                 # 選填；同 --exclude-caches（CACHEDIR.TAG）
//! exclude_larger_than = "1G"            # 選填；同 --exclude-larger-than（1024 為底）
//! one_file_system = true                # 選填；同 --one-file-system
//!
//! [forget]                     # 選填；需要 Delete 權限，建議放在維護主機的設定裡
//! keep_daily = 7
//! keep_weekly = 4
//! schedule = "0 4 * * *"
//!
//! [prune]                      # 選填；同上（grace 存在 repo 裡，以 `kist settings --grace` 改）
//! repack_below = 50
//! read_data = false             # 選填；同 --read-data（每輪整份讀資料 pack 驗證、修復，慢）
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
    /// 每個 pack 旁存幾片 Reed-Solomon 同位（0..=8；0 = 不存，預設）。
    #[serde(default)]
    pub parity: u8,
    /// 排除樣式，規則同 CLI 的 `--exclude`（ADR 020，`kist_core::filter`）。
    #[serde(default)]
    pub exclude: Vec<String>,
    /// 不分大小寫的排除樣式，同 `--iexclude`（ADR 023；`!` 的順序上排在 `exclude` 之後）。
    #[serde(default)]
    pub iexclude: Vec<String>,
    /// 記號檔，同 `--exclude-if-present`（`NAME` 或 `NAME:HEADER`，ADR 021）。
    #[serde(default)]
    pub exclude_if_present: Vec<String>,
    /// 同 `--exclude-caches`：有 CACHEDIR.TAG（簽章正確）的目錄只留那個檔。
    #[serde(default)]
    pub exclude_caches: bool,
    /// 同 `--exclude-larger-than`：`"100M"` 之類，1024 為底。
    #[serde(default, with = "crate::size::serde_opt")]
    pub exclude_larger_than: Option<u64>,
    /// 同 `--one-file-system`。
    #[serde(default)]
    pub one_file_system: bool,
}

impl BackupSection {
    /// 這一節的來源：`paths` 只有一個 `sftp://` 或 `s3://` URL 就是那個遠端來源，否則是本機路徑（同 `kist
    /// backup`）。URL 與別的路徑混在一起，載入設定時（[`Config::validate`]）就是錯誤。
    pub fn source(&self) -> Result<kist_core::SourceSpec> {
        kist_core::SourceSpec::for_paths(&self.paths)
            .map_err(|e| AppError::Config(format!("[backup] paths: {e}")))
    }

    /// 這一節的排除規則（ADR 020、021）。載入設定時（[`Config::validate`]）就跑一次：
    /// 寫錯的樣式或記號讓 `kist run`／`kist serve` 起不來，不會等到排程時間才失敗。
    pub fn exclusions(&self) -> Result<kist_core::Exclusions> {
        let patterns = kist_core::PathFilter::new(&self.exclude)
            .map_err(|e| AppError::Config(format!("[backup] exclude: {e}")))?
            .and_ignoring_case(&self.iexclude)
            .map_err(|e| AppError::Config(format!("[backup] iexclude: {e}")))?;
        kist_core::Exclusions::new(
            patterns,
            &self.exclude_if_present,
            self.exclude_caches,
            self.exclude_larger_than,
            self.one_file_system,
        )
        .map_err(|e| AppError::Config(format!("[backup] {e}")))
    }
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
    /// v4 的 grace 在 repo 的 head 裡（`kist settings --grace`），v3 的 grace、clock_skew、inactive_after
    /// 都不再是設定：寫了就是 deny_unknown_fields 的錯誤，不默默忽略。
    pub repack_below: Option<u8>,
    /// 同 `prune --read-data`。
    #[serde(default)]
    pub read_data: bool,
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
            if usize::from(b.parity) > kist_core::MAX_PARITY_SHARDS {
                return Err(AppError::Config(format!(
                    "[backup] parity must be 0..={}",
                    kist_core::MAX_PARITY_SHARDS
                )));
            }
            b.source()?;
            b.exclusions()?;
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
            // scheme 大小寫不拘（URL scheme 本就 case-insensitive）。
            let scheme = n.webhook_url.to_ascii_lowercase();
            if !(scheme.starts_with("http://") || scheme.starts_with("https://")) {
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

/// 密碼檔的規則（repo 密碼與 UI 密碼共用；CLI 的 `--password-file` 也走這裡，
/// ADR 019 A11）：取第一行、去掉 `\r`，空的算設定錯誤。
/// 整個檔的內容與取出的密碼都包 `Zeroizing`，離開作用域時清零（ADR 007）。
pub fn read_password_file(path: &Path) -> Result<zeroize::Zeroizing<String>> {
    let text = std::fs::read_to_string(path).map_err(|e| AppError::Io {
        path: path.to_path_buf(),
        source: e,
    })?;
    let text = zeroize::Zeroizing::new(text);
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
    pub fn policy(&self) -> kist_core::retention::RetentionPolicy {
        kist_core::retention::RetentionPolicy {
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
    pub fn options(&self) -> kist_core::v4::PruneOptions {
        let d = kist_core::v4::PruneOptions::default();
        kist_core::v4::PruneOptions {
            repack_below: self.repack_below.unwrap_or(d.repack_below),
            read_data: self.read_data,
            ..d
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod scheme_case_tests {
    use super::*;

    /// scheme 大小寫不拘（URL scheme 本就 case-insensitive）。
    #[test]
    fn webhook_scheme_is_case_insensitive() {
        let cfg = Config::parse(
            "repo = \"x\"\npassword_file = \"/etc/kist/pw\"\n[backup]\npaths = [\"/data\"]\nschedule = \"0 2 * * *\"\n[notify]\nwebhook_url = \"HTTPS://Example.com/hook\"\n",
        )
        .unwrap();
        assert!(cfg.validate().is_ok(), "大寫 scheme 是合法的 webhook URL");
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod v3_keys_tests {
    use super::*;

    /// v4 的 grace 在 repo 裡：v3 的 `[backup] gc_grace` 與 `[prune] grace`、`clock_skew`、
    /// `inactive_after` 寫了就是錯誤，不默默忽略（以為設了其實沒有）。
    #[test]
    fn removed_grace_keys_are_errors() {
        for (section, key) in [
            ("[backup]\npaths = [\"/e\"]", "gc_grace = \"72h\""),
            ("[prune]", "grace = \"72h\""),
            ("[prune]", "clock_skew = \"1h\""),
            ("[prune]", "inactive_after = \"30d\""),
        ] {
            let text = format!("repo = \"x\"\npassword_file = \"/p\"\n{section}\n{key}\n");
            let err = Config::parse(&text).unwrap_err().to_string();
            assert!(err.contains("unknown field"), "{key}: {err}");
        }
    }

    #[test]
    fn prune_repack_below_maps_to_the_options() {
        let cfg =
            Config::parse("repo = \"x\"\npassword_file = \"/p\"\n[prune]\nrepack_below = 30\n")
                .unwrap();
        assert_eq!(cfg.prune.unwrap().options().repack_below, 30);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod parity_tests {
    use super::*;

    fn with_parity(m: u32) -> Config {
        Config::parse(&format!(
            "repo = \"x\"\npassword_file = \"/p\"\n[backup]\npaths = [\"/e\"]\nparity = {m}\n"
        ))
        .unwrap()
    }

    /// `[backup] parity` 的上限就是 MAX_PARITY_SHARDS（ADR 019 A12）：8 收、9 拒。
    #[test]
    fn parity_above_max_is_rejected() {
        assert!(with_parity(8).validate().is_ok());
        assert!(with_parity(9).validate().is_err());
    }
}
