//! SFTP 後端：把遠端目錄當成 kist 的 key-value 命名空間，一個 key 一個檔案。
//!
//! 語意對齊 Go 參考實作（kist `internal/backend/sftp.go`）：
//! - **寫入一律走暫存檔**：同目錄下 `.tmp-<16hex>`（O_EXCL 建檔）→ 寫完 `fsync`
//!   （伺服器支援 `fsync@openssh.com` 時）→
//!   - `put`（覆蓋）：`posix-rename@openssh.com` 原子換名；
//!   - `put_if_absent`：`hardlink@openssh.com`——link 對已存在的目標會失敗，這就是
//!     「只在不存在時寫入」的原子保證。SFTP 沒有「已存在」的狀態碼（OpenSSH 回泛用的
//!     FAILURE），所以 link 被拒後再用 stat 判別是「已存在」還是別種錯誤。
//!     當機最壞只會留下暫存檔殘骸，正式 key 底下不會出現寫一半的物件。
//! - **伺服器需求**：`hardlink@openssh.com` 與 `posix-rename@openssh.com` 缺一拒絕
//!   （連線時就檢查，給出明確訊息）；`fsync@openssh.com` 有就同步、沒有就略過。
//! - **host key 嚴格驗證**：只接受 known_hosts（預設 `~/.ssh/known_hosts`，可用
//!   `KIST_SFTP_KNOWN_HOSTS` 指定）裡有的 key；不在裡面一律拒絕連線（不做 TOFU）。
//!   連線前先從 known_hosts 取該主機的 key 類型做演算法預選——OpenSSH 伺服器只會
//!   從客戶端提案的類型裡挑，不預選的話，known_hosts 只記了 ed25519 的主機可能
//!   一直收到 ECDSA key 而被自己拒絕（Go 版用兩次撥接解同一件事）。
//! - **認證順序**：`$SSH_AUTH_SOCK` 的 agent → key 檔（`KIST_SFTP_KEY`，可配
//!   `KIST_SFTP_KEY_PASSPHRASE`）→ 密碼（`KIST_SFTP_PASSWORD`）。全缺則報錯。
//!
//! URL（見 [`parse_sftp_url`]）：`sftp://[user@]host[:port]/path`，路徑一律視為絕對
//! 路徑；密碼不得寫進 URL（會進 shell 歷史與 process 清單）。
//!
//! **rclone 橋接**（見 [`parse_rclone_url`]）：`rclone://<remote>/<path>` 讓 kist 自己
//! spawn `rclone serve sftp --stdio <remote>:<path>`，SFTP 走 stdio pipe——不開 TCP
//! port、不用 known_hosts，任何 rclone 設定好的遠端（`rclone config`）都能當儲存體。
//! rclone 的 SFTP 伺服器不實做 `hardlink@openssh.com` 與 O_EXCL 建檔（宣稱支援但
//! 執行回 `OpUnsupported`，實測見 docs/decisions/014-rclone-bridge.md），所以這個
//! 模式下寫入語意比 `sftp://` 寬鬆：暫存檔退回 create+truncate（名稱是 64 位元隨機
//! 數，撞名可忽略；最終 publish 仍是原子的 posix-rename）、`put_if_absent` 退化成
//! 「先 stat 再 posix-rename」。kist 的 `put_if_absent` key 都是「同 key 必同內容」
//! （pack、parity、GC 標記）或一次性寫入（config、snapshot），race 的最壞結果與
//! 緩解（init 讀回驗證）都記在 ADR 014。`sftp://` 的條件寫入守門（hardlink）不變；
//! 暫存檔建檔在任何模式下遇到伺服器拒絕 O_EXCL 都會退回 create+truncate——那只
//! 影響暫存檔（64 位元隨機名），不影響最終 publish 的原子性，也不影響合約。
//!
//! 為什麼是 russh + openssh-sftp-client：認證與 host key 全在程式內完成（不需要外部
//! `ssh` 執行檔，密碼才能非互動輸入）；SFTP 協議層由 openssh-sftp-client 提供，
//! 內建管線化寫入（64 MiB pack 在 WAN 上才不會慢）與三個 OpenSSH 擴充的偵測。

use std::fmt;
use std::ops::Range;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bytes::Bytes;
use futures::{stream::BoxStream, StreamExt};
use object_store::path::Path as StorePath;
use object_store::{
    Error as StoreError, GetOptions, GetResult, GetResultPayload, ListResult, ObjectMeta,
    ObjectStore, PutMode, PutOptions, PutPayload, PutResult,
};
use openssh_sftp_client::file::TokioCompatFile;
use openssh_sftp_client::metadata::MetaData;
use openssh_sftp_client::Error as SftpError;
use openssh_sftp_client::Sftp;
use russh::client::{self, Handle, Msg};
#[cfg(unix)]
use russh::keys::agent::client::AgentClient;
use russh::keys::known_hosts::known_host_keys_path;
use russh::keys::{check_known_hosts_path, decode_secret_key, PrivateKeyWithHashAlg};
use tokio::process::{Child, ChildStderr, Command};

use crate::{BackendError, Result};

/// SFTP 位置：`sftp://[user@]host[:port]/path` 解析後的結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SftpConfig {
    pub user: Option<String>,
    pub host: String,
    pub port: u16,
    /// 遠端絕對路徑（repo 根目錄）。
    pub path: String,
}

/// `sftp://[user@]host[:port]/path`。
///
/// - 不帶 user：連線時用目前使用者（`USER` / `LOGNAME`）。
/// - 帳密寫在 URL（`user:pass@`）直接拒絕：密碼會留在 shell 歷史與 process 清單裡，
///   請用 `KIST_SFTP_PASSWORD`（與 Go 參考實作同一理由、同一變數名）。
/// - 支援 `[ipv6]` 形式的主機；不做 percent-decoding，路徑就是字面內容。
pub fn parse_sftp_url(s: &str) -> Result<SftpConfig> {
    let rest = match s.strip_prefix("sftp://") {
        Some(rest) => rest,
        None => return Err(BackendError::InvalidUrl(s.to_owned())),
    };
    let (user, hostport) = match rest.split_once('@') {
        Some((u, hp)) => {
            if u.is_empty() || u.contains(':') {
                return Err(BackendError::InvalidUrl(s.to_owned()));
            }
            (Some(u.to_owned()), hp)
        }
        None => (None, rest),
    };
    // 切出 (host, port 字串, /path)——路徑可能為空，最後統一檢查。
    let (host, port_str, path) = if let Some(stripped) = hostport.strip_prefix('[') {
        let (v6, tail) = stripped
            .split_once(']')
            .ok_or_else(|| BackendError::InvalidUrl(s.to_owned()))?;
        // ']' 後面：":port/path"、":port"、"/path" 或空字串。
        let (port, path) = match tail.strip_prefix(':') {
            Some(after) => match after.split_once('/') {
                Some((p, path)) => (Some(p), path),
                None => (Some(after), ""),
            },
            None => match tail.split_once('/') {
                Some((_, path)) => (None, path),
                None => (None, tail),
            },
        };
        (v6, port, path)
    } else {
        let (hp, path) = match hostport.split_once('/') {
            Some((hp, path)) => (hp, path),
            None => (hostport, ""),
        };
        match hp.split_once(':') {
            Some((h, p)) => (h, Some(p), path),
            None => (hp, None, path),
        }
    };
    if host.is_empty() || path.is_empty() {
        return Err(BackendError::InvalidUrl(s.to_owned()));
    }
    let port = match port_str {
        None => 22,
        Some(p) => match p.parse::<u16>() {
            Ok(0) | Err(_) => return Err(BackendError::InvalidUrl(s.to_owned())),
            Ok(port) => port,
        },
    };
    Ok(SftpConfig {
        user,
        host: host.to_owned(),
        port,
        path: path.to_owned(),
    })
}

/// 認證素材（host key 驗證的來源與使用者金鑰）。密碼與 passphrase 只在記憶體停留
/// 必要的時間。
#[derive(Debug, Clone, Default)]
pub struct SftpAuth {
    /// known_hosts 檔案；`None` 用 `~/.ssh/known_hosts`。
    pub known_hosts: Option<PathBuf>,
    /// 私鑰檔與（可選的）passphrase。
    pub key: Option<(PathBuf, Option<String>)>,
    /// 密碼。
    pub password: Option<String>,
}

/// 從環境變數讀認證素材：`KIST_SFTP_KNOWN_HOSTS` / `KIST_SFTP_KEY` /
/// `KIST_SFTP_KEY_PASSPHRASE` / `KIST_SFTP_PASSWORD`（名稱對齊 Go 參考實作）。
/// 只給 [`crate::Backend::from_url`] 用；其他管道請直接構造 [`SftpAuth`]。
pub fn auth_from_env() -> SftpAuth {
    let known_hosts = std::env::var("KIST_SFTP_KNOWN_HOSTS")
        .ok()
        .map(PathBuf::from);
    let key = match std::env::var("KIST_SFTP_KEY") {
        Ok(path) => Some((
            PathBuf::from(path),
            std::env::var("KIST_SFTP_KEY_PASSPHRASE").ok(),
        )),
        Err(_) => None,
    };
    SftpAuth {
        known_hosts,
        key,
        password: std::env::var("KIST_SFTP_PASSWORD").ok(),
    }
}

fn default_known_hosts() -> Result<PathBuf> {
    std::env::var("HOME")
        .ok()
        .filter(|h| !h.is_empty())
        .map(|h| PathBuf::from(h).join(".ssh").join("known_hosts"))
        .ok_or_else(|| BackendError::Sftp("no known_hosts given and $HOME is not set".to_owned()))
}

/// rclone 位置：`rclone://<remote>/<path>` 解析後的結果。`remote` 為空字串代表
/// rclone 的本機檔案系統（`rclone:///srv/backups`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RcloneConfig {
    pub remote: String,
    /// rclone 遠端上的 repo 根目錄（不含前後斜線）。
    pub path: String,
}

/// `rclone://<remote>/<path>`（remote 留空 = 本機目錄）。
///
/// kist spawn `rclone serve sftp --stdio <remote>:<path>`（remote 留空時直接給本機
/// 絕對路徑），SFTP 走子程序的 stdin/stdout——不開 TCP port、不用 known_hosts；
/// rclone 的雲端設定（`rclone config`）就是唯一的前置設定。二進位預設從 `PATH`
/// 找，可用 `KIST_RCLONE_BIN` 覆蓋。rclone 寫到 stderr 的日誌會被收進一個 4 KiB
/// 的尾巴緩衝：不清掉會塞爆 pipe 把協議卡死，收起來才能在啟動失敗時把真正的
/// 錯誤（remote 名字打錯等）回給使用者。
///
/// **語意比 `sftp://` 寬鬆**（理由與風險分析見模組說明與 ADR 014）：選這個 scheme
/// 就是同意這份妥協。
pub fn parse_rclone_url(s: &str) -> Result<RcloneConfig> {
    let rest = match s.strip_prefix("rclone://") {
        Some(rest) => rest,
        None => return Err(BackendError::InvalidUrl(s.to_owned())),
    };
    // remote 名字切在第一個 '/'；rclone 的 remote 名不含 '/'（設定檔的 section 名）。
    let (remote, path) = match rest.split_once('/') {
        Some((r, p)) => (r, p),
        None => (rest, ""),
    };
    // remote 用**白名單**驗證。安全理由：remote 與 path 會合成**一個** argv 元素餵
    // 給 rclone，而 rclone 的旗標解析穿插在位置參數之間——`--password-command=…`
    // 這種「remote」會被解析成 rclone 的旗標，加密設定檔下該旗標的值會被 shell 執行
    // （審查指出的注入面）。允許字母、數字、`-`、`_`、`.`（rclone 的設定檔 section
    // 名允許點），不以 `-` 開頭；其餘一律拒絕（fail-closed：空白、`=`、控制字元等
    // 都進不了 argv）。path 另拒控制字元。
    if path.is_empty() || !valid_remote_name(remote) || path.chars().any(|c| c.is_control()) {
        return Err(BackendError::InvalidUrl(s.to_owned()));
    }
    Ok(RcloneConfig {
        remote: remote.to_owned(),
        path: path.trim_matches('/').to_owned(),
    })
}

/// rclone remote 名的白名單：空字串（= rclone 的本機檔案系統）或「字母數字、
/// `-`、`_`、`.`、不以 `-` 開頭」。`-` 開頭是 flag injection 的關鍵防線。
fn valid_remote_name(remote: &str) -> bool {
    remote.is_empty()
        || (!remote.starts_with('-')
            && remote
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')))
}

fn current_user() -> Result<String> {
    ["USER", "LOGNAME"]
        .iter()
        .find_map(|v| std::env::var(v).ok().filter(|s| !s.is_empty()))
        .ok_or_else(|| BackendError::Sftp("no user given and none found".to_owned()))
}

/// russh 的 client::Handler：握手時的 host key 檢查。嚴格對 known_hosts，
/// 拒絕未記錄的 key（不做 TOFU——TOFU 會把中間人升級成你的備份伺服器）。
struct HostKeyCheck {
    host: String,
    port: u16,
    known_hosts: PathBuf,
}

impl HostKeyCheck {
    fn verify(&self, key: &russh::keys::PublicKey) -> Result<()> {
        match check_known_hosts_path(&self.host, self.port, key, &self.known_hosts) {
            Ok(true) => Ok(()),
            Ok(false) => Err(BackendError::Sftp(format!(
                "host key for {} is not in {}; refusing to connect (add the key to that \
                 file instead of trusting the connection)",
                self.host,
                self.known_hosts.display()
            ))),
            Err(e) => Err(BackendError::Sftp(format!(
                "host key check against {}: {e}",
                self.known_hosts.display()
            ))),
        }
    }
}

impl client::Handler for HostKeyCheck {
    type Error = BackendError;
    async fn check_server_key(
        &mut self,
        key: &russh::keys::PublicKeyOrCertificate,
    ) -> std::result::Result<bool, Self::Error> {
        match key {
            russh::keys::PublicKeyOrCertificate::PublicKey { key, .. } => {
                self.verify(key).map(|_| true)
            }
            // kist 不用 SSH 憑證；known_hosts 也不會有對應記錄，保守拒絕。
            russh::keys::PublicKeyOrCertificate::Certificate(_) => Err(BackendError::Sftp(
                "server presented an SSH certificate; kist only verifies plain host keys \
                 against known_hosts"
                    .to_owned(),
            )),
        }
    }
}

/// 連線：TCP + 握手（host key 驗證）+ 認證 + SFTP subsystem + 伺服器能力檢查。
async fn connect(cfg: &SftpConfig, auth: &SftpAuth) -> Result<(Keepalive, Arc<Sftp>, bool)> {
    let user = match &cfg.user {
        Some(u) => u.clone(),
        None => current_user()?,
    };
    let known_hosts = match &auth.known_hosts {
        Some(p) => p.clone(),
        None => default_known_hosts()?,
    };

    // 演算法預選：該主機在 known_hosts 記錄的 key 類型。沒記錄也照連（讓握手的
    // check_server_key 給出帶檔名的錯誤），只是不預選。
    let mut config = client::Config {
        inactivity_timeout: Some(std::time::Duration::from_secs(600)),
        ..Default::default()
    };
    match known_host_keys_path(&cfg.host, cfg.port, &known_hosts) {
        Ok(keys) if !keys.is_empty() => {
            config.preferred.key = keys
                .into_iter()
                .map(|(_, pk)| pk.algorithm())
                .collect::<Vec<_>>()
                .into();
        }
        // 主機還不在 known_hosts（等一下會被拒）：給一組保守的標準演算法，
        // 讓協商走得到 host key 檢查、錯誤訊息才會是「不在 known_hosts 裡」
        // 而不是莫名其妙的演算法協商失敗。
        Ok(_) => {
            config.preferred.key = std::borrow::Cow::Owned(vec![
                russh::keys::Algorithm::Ed25519,
                russh::keys::Algorithm::Ecdsa {
                    curve: russh::keys::EcdsaCurve::NistP256,
                },
                russh::keys::Algorithm::Rsa {
                    hash: Some(russh::keys::HashAlg::Sha512),
                },
                russh::keys::Algorithm::Rsa {
                    hash: Some(russh::keys::HashAlg::Sha256),
                },
            ]);
        }
        Err(e) => {
            return Err(BackendError::Sftp(format!(
                "host key check against {}: {e}",
                known_hosts.display()
            )))
        }
    }

    let mut handle = client::connect(
        Arc::new(config),
        (cfg.host.as_str(), cfg.port),
        HostKeyCheck {
            host: cfg.host.clone(),
            port: cfg.port,
            known_hosts: known_hosts.clone(),
        },
    )
    .await
    .map_err(|e| {
        BackendError::Sftp(format!(
            "connect sftp://{}@{}:{}/{}: {e}",
            user, cfg.host, cfg.port, cfg.path
        ))
    })?;

    authenticate(&mut handle, &user, auth).await?;

    let channel: russh::Channel<Msg> = handle
        .channel_open_session()
        .await
        .map_err(|e| BackendError::Sftp(format!("open session: {e}")))?;
    channel
        .request_subsystem(true, "sftp")
        .await
        .map_err(|e| BackendError::Sftp(format!("request sftp subsystem: {e}")))?;
    let (read_half, write_half) = tokio::io::split(channel.into_stream());
    let sftp = Sftp::new(write_half, read_half, Default::default())
        .await
        .map_err(|e| BackendError::Sftp(format!("start sftp: {e}")))?;

    // 伺服器能力檢查：兩個擴充都是寫入語意的根基，缺一個都不行（訊息對齊 Go 版）。
    if !sftp.support_hardlink() || !sftp.support_posix_rename() {
        return Err(BackendError::Sftp(
            "the server does not offer hardlink@openssh.com and posix-rename@openssh.com, \
             which kist needs for atomic writes; OpenSSH's sftp-server does"
                .to_owned(),
        ));
    }
    let fsync = sftp.support_fsync();
    Ok((Keepalive::Ssh { _handle: handle }, Arc::new(sftp), fsync))
}

/// 讓連線活著的所有權：SSH 模式是 russh 的連線把手（drop 即斷線）；rclone 模式
/// 是子程序（`kill_on_drop`）。`SftpInner` 的欄位順序讓 sftp 先 drop（關掉 stdin，
/// rclone 才有機會自己收尾），子程序最後才被 kill。
enum Keepalive {
    Ssh {
        _handle: Handle<HostKeyCheck>,
    },
    Stdio {
        /// `kill_on_drop(true)`：drop 即 kill。kist 的每個操作都等伺服器 ACK 才算
        /// 完成，CLI 結束時不會有還在飛的請求；kill 只是避免子程序在異常路徑上
        /// 掛著（正常路徑 sftp drop 已關 stdin，rclone 會自己退出）。
        _child: Child,
    },
}

/// rclone 的 stderr 尾巴緩衝：背景任務永久讀、只留最後 4 KiB。不清 stderr 的話
/// pipe 滿了 rclone 會卡死；收起來才能在啟動失敗時附上真正的錯誤訊息。
struct StderrTail {
    buf: Arc<Mutex<Vec<u8>>>,
    /// 收集任務的把手：`text_settled` 等它結束，尾巴才保證是全部。
    task: tokio::task::JoinHandle<()>,
}

impl StderrTail {
    fn spawn(mut stderr: ChildStderr) -> Self {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let inner = Arc::clone(&buf);
        let task = tokio::spawn(async move {
            use tokio::io::AsyncReadExt as _;
            let mut chunk = [0u8; 512];
            loop {
                match stderr.read(&mut chunk).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let mut buf = inner.lock().unwrap_or_else(|e| e.into_inner());
                        buf.extend_from_slice(&chunk[..n]);
                        let excess = buf.len().saturating_sub(4096);
                        drain_front(&mut buf, excess);
                    }
                }
            }
        });
        Self { buf, task }
    }

    /// 等背景收集任務把 stderr 讀完（子程序退出 → EOF → 任務結束）再回傳尾巴。
    /// `wait` 是上限：任務沒能結束時照樣回目前已收到的內容。
    async fn text_settled(mut self, wait: std::time::Duration) -> String {
        let _ = tokio::time::timeout(wait, &mut self.task).await;
        let buf = self.buf.lock().unwrap_or_else(|e| e.into_inner());
        String::from_utf8_lossy(&buf).trim().to_owned()
    }
}

/// `Vec::drain(..n)` 的借用分離寫法（在還持有 lock 的當下整理緩衝）。
fn drain_front(buf: &mut Vec<u8>, n: usize) {
    let rest = buf.split_off(n);
    *buf = rest;
}

/// spawn `rclone serve sftp --stdio <source>` 並完成 SFTP 版本交換。
/// 回傳（子程序、sftp、是否支援 fsync、stderr 尾巴）。
async fn connect_stdio(cfg: &RcloneConfig) -> Result<(Child, Arc<Sftp>, bool, StderrTail)> {
    let bin = std::env::var("KIST_RCLONE_BIN").unwrap_or_else(|_| "rclone".to_owned());
    // remote 留空 = 本機目錄，直接給絕對路徑；否則 rclone 的 `remote:path` 寫法。
    let source = if cfg.remote.is_empty() {
        format!("/{}", cfg.path)
    } else {
        format!("{}:{}", cfg.remote, cfg.path)
    };
    let mut child = Command::new(&bin)
        .args(["serve", "sftp", "--stdio", "--log-level", "ERROR", &source])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| {
            BackendError::Rclone(format!(
                "cannot spawn {bin} (set KIST_RCLONE_BIN if rclone is not in PATH): {e}"
            ))
        })?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| BackendError::Rclone("spawned rclone without stdin".to_owned()))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| BackendError::Rclone("spawned rclone without stdout".to_owned()))?;
    let tail = StderrTail::spawn(
        child
            .stderr
            .take()
            .ok_or_else(|| BackendError::Rclone("spawned rclone without stderr".to_owned()))?,
    );

    let sftp = tokio::select! {
        // 正常路徑：版本交換完成。
        res = Sftp::new(stdin, stdout, Default::default()) => match res {
            Ok(s) => s,
            Err(e) => {
                // 版本交換失敗：等子程序退出（stderr EOF），附上 rclone 說的話。
                let _ = tokio::time::timeout(
                    std::time::Duration::from_millis(500),
                    child.wait(),
                )
                .await;
                let said = tail.text_settled(std::time::Duration::from_millis(500)).await;
                let mut details = format!("start sftp over rclone stdio: {e}");
                if !said.is_empty() {
                    details.push_str(&format!("; rclone said: {said}"));
                }
                if let Ok(Some(status)) = child.try_wait() {
                    details.push_str(&format!("; rclone exited: {status}"));
                }
                return Err(BackendError::Rclone(details));
            }
        },
        // rclone 提前退出（remote 名字打錯、設定檔壞掉……）：版本交換永遠等不到。
        // 上游在 multi_thread runtime 上 stdout EOF 不會喚醒 Sftp::new（會無限等，
        // 見 ADR 014），所以用 wait() 先看到退出。wait() 是 cancel safe 的。
        status = child.wait() => {
            let said = tail.text_settled(std::time::Duration::from_millis(500)).await;
            let mut details =
                "rclone exited before completing the sftp handshake".to_owned();
            match status {
                Ok(status) => details.push_str(&format!(" (exit: {status})")),
                Err(e) => details.push_str(&format!(" (wait failed: {e})")),
            }
            if !said.is_empty() {
                details.push_str(&format!("; rclone said: {said}"));
            }
            return Err(BackendError::Rclone(details));
        }
        // 保險絲：30 秒沒完成版本交換（懸掛、極慢的遠端）——殺掉回錯。
        _ = tokio::time::sleep(std::time::Duration::from_secs(30)) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            let said = tail.text_settled(std::time::Duration::from_millis(500)).await;
            let mut details =
                "rclone did not complete the sftp handshake within 30s".to_owned();
            if !said.is_empty() {
                details.push_str(&format!("; rclone said: {said}"));
            }
            return Err(BackendError::Rclone(details));
        }
    };
    // 寬鬆模式唯一的 publish 原語是 posix-rename（hardlink 在 rclone 上不會動）。
    if !sftp.support_posix_rename() {
        return Err(BackendError::Rclone(
            "rclone's sftp server does not offer posix-rename@openssh.com, \
             which kist needs to publish objects atomically"
                .to_owned(),
        ));
    }
    let fsync = sftp.support_fsync();
    Ok((child, Arc::new(sftp), fsync, tail))
}

async fn authenticate(
    handle: &mut Handle<HostKeyCheck>,
    user: &str,
    auth: &SftpAuth,
) -> Result<()> {
    // RSA key 要問伺服器支援哪個 hash（rsa-sha2-256/512）；其他 key 不需要。
    let rsa_hash = handle
        .best_supported_rsa_hash()
        .await
        .ok()
        .flatten()
        .flatten();

    // 1. agent：有 SSH_AUTH_SOCK 就試；拿不到身分或全部被拒就往下降。
    //    （agent 走 Unix socket，Windows 上沒有這條路。）
    #[cfg(unix)]
    if let Ok(mut agent) = AgentClient::connect_env().await {
        if let Ok(identities) = agent.request_identities().await {
            for identity in &identities {
                let pk = identity.public_key().into_owned();
                let hash_alg = if pk.algorithm().is_rsa() {
                    rsa_hash
                } else {
                    None
                };
                if let Ok(r) = handle
                    .authenticate_publickey_with(user, pk, hash_alg, &mut agent)
                    .await
                {
                    if r.success() {
                        return Ok(());
                    }
                }
            }
        }
    }

    // 2. key 檔。
    if let Some((key_path, passphrase)) = &auth.key {
        let pem = std::fs::read_to_string(key_path)
            .map_err(|e| BackendError::Sftp(format!("read key {}: {e}", key_path.display())))?;
        let key = decode_secret_key(&pem, passphrase.as_deref())
            .map_err(|e| BackendError::Sftp(format!("parse key {}: {e}", key_path.display())))?;
        let hash_alg = if key.algorithm().is_rsa() {
            rsa_hash
        } else {
            None
        };
        let r = handle
            .authenticate_publickey(user, PrivateKeyWithHashAlg::new(Arc::new(key), hash_alg))
            .await
            .map_err(|e| BackendError::Sftp(format!("publickey auth: {e}")))?;
        if r.success() {
            return Ok(());
        }
    }

    // 3. 密碼。
    if let Some(password) = &auth.password {
        let r = handle
            .authenticate_password(user, password)
            .await
            .map_err(|e| BackendError::Sftp(format!("password auth: {e}")))?;
        if r.success() {
            return Ok(());
        }
    }

    Err(BackendError::Sftp(
        "no way to authenticate: no SSH agent, no key file, no password".to_owned(),
    ))
}

fn is_dir(m: &MetaData) -> bool {
    m.file_type().map(|t| t.is_dir()).unwrap_or(false)
}

/// SFTP 檔案錯誤是否為「不存在」。
fn is_not_found(e: &SftpError) -> bool {
    matches!(
        e,
        SftpError::SftpError(openssh_sftp_client::error::SftpErrorKind::NoSuchFile, _)
    )
}

/// SFTP 檔案錯誤是否為「伺服器不支援這個操作」（SSH_FX_OP_UNSUPPORTED）。
/// rclone 對 O_EXCL 建檔與 hardlink 都是這個：宣稱支援、執行才拒絕。
fn is_unsupported(e: &SftpError) -> bool {
    matches!(
        e,
        SftpError::SftpError(openssh_sftp_client::error::SftpErrorKind::OpUnsupported, _)
    )
}

fn generic(e: impl std::error::Error + Send + Sync + 'static) -> StoreError {
    StoreError::Generic {
        store: "sftp",
        source: Box::new(e),
    }
}

fn to_meta(path: &str, m: &MetaData) -> std::result::Result<ObjectMeta, StoreError> {
    let size = m.len().unwrap_or(0);
    let secs = m.modified().map(|ts| ts.into_raw() as i64).unwrap_or(0);
    let last_modified =
        chrono::DateTime::from_timestamp(secs, 0).unwrap_or(chrono::DateTime::UNIX_EPOCH);
    // 伺服器給了 object_store 命名規則外的名稱：略過該條目，不讓整個 list 失敗
    let location = StorePath::parse(path).map_err(generic)?;
    Ok(ObjectMeta {
        location,
        last_modified,
        size,
        e_tag: None,
        version: None,
    })
}

fn dir_of(path: &str) -> &str {
    match path.rsplit_once('/') {
        Some((dir, _)) if !dir.is_empty() => dir,
        _ => "/",
    }
}

/// 共用的連線狀態（`SftpStore` 內包 `Arc`：`delete_stream`／`list` 需要 `'static`）。
struct SftpInner {
    sftp: Arc<Sftp>,
    fsync: bool,
    /// rclone:// 橋接的寬鬆模式：`put_if_absent` 退化成 stat + posix-rename
    /// （見模組說明與 ADR 014）。
    relaxed: bool,
    /// O_EXCL 建檔被伺服器拒絕時只警告一次。
    warned_no_o_excl: AtomicBool,
    /// rclone 的 stderr 尾巴（stdio 模式才有；錯誤訊息附帶 rclone 說了什麼）。
    stderr_tail: Option<StderrTail>,
    /// **欄位順序即 drop 順序**：先 drop sftp（關 stdin 讓 rclone 收尾），連線
    /// 把手／子程序最後才 drop（斷線／kill）。
    keepalive: Keepalive,
    root: String,
    display: String,
}

impl Drop for SftpInner {
    fn drop(&mut self) {
        // 欄位宣告順序已保證 sftp 先 drop（關掉 stdin，rclone 有機會自己收尾），
        // keepalive（ssh 把手＝斷線、子程序＝kill_on_drop）與 stderr 的背景任務
        // 最後收。這裡讀取只是把這個所有權契約寫成程式碼。
        let _ = &self.sftp;
        let _ = &self.stderr_tail;
        let _ = &self.keepalive;
    }
}

impl SftpInner {
    fn full(&self, location: &StorePath) -> String {
        format!("{}/{}", self.root, location)
    }

    /// 一層一層建（SFTP 沒有 mkdir -p）；已存在視為成功。
    async fn ensure_dir(&self, path: &str) -> Result<()> {
        let mut cur = String::new();
        for part in path.split('/').filter(|p| !p.is_empty()) {
            cur.push('/');
            cur.push_str(part);
            let mut fs = self.sftp.fs();
            if let Err(e) = fs.create_dir(&cur).await {
                if is_not_found(&e) {
                    return Err(BackendError::Sftp(format!(
                        "mkdir {cur}: parent missing: {e}"
                    )));
                }
                let mut fs = self.sftp.fs();
                match fs.metadata(&cur).await {
                    Ok(m) if is_dir(&m) => {} // 已存在，OK
                    Ok(_) => {
                        return Err(BackendError::Sftp(format!(
                            "{cur} exists and is not a directory"
                        )))
                    }
                    Err(e) => return Err(BackendError::Sftp(format!("mkdir {cur}: {e}"))),
                }
            }
        }
        Ok(())
    }

    /// 暫存檔寫入：O_EXCL 建檔 → 寫 → fsync（伺服器支援時）→ close。
    /// 回傳暫存檔路徑；呼叫端負責 link/rename 進正式位址或刪掉。
    /// 寫入過程任何失敗都會把暫存檔清掉——不然 flaky 網路會在伺服器上累積殘骸。
    async fn spool(&self, dir: &str, bytes: &[u8]) -> std::result::Result<String, StoreError> {
        self.ensure_dir(dir).await.map_err(generic)?;
        let mut name = [0u8; 8];
        getrandom::fill(&mut name).map_err(generic)?;
        let scratch = format!("{dir}/.tmp-{}", hex::encode(name));

        // O_EXCL 建檔是第一選擇（撞名的第二個寫入者會直接失敗）。伺服器不支援時
        // （rclone 對 create_new 回 OpUnsupported）退回 create+truncate：名稱是
        // 64 位元隨機數，撞名機率可忽略，最終 publish 仍是原子的
        // （hardlink／posix-rename）。只對 OpUnsupported 退——權限等其他錯誤照傳。
        let opened = self
            .sftp
            .options()
            .write(true)
            .create_new(true)
            .open(&scratch)
            .await;
        let mut f = match opened {
            Ok(f) => f,
            Err(e) if is_unsupported(&e) => {
                if !self.warned_no_o_excl.swap(true, Ordering::Relaxed) {
                    eprintln!(
                        "kist: server rejected exclusive scratch-file creation (operation \
                         unsupported); falling back to create+truncate with a random name — \
                         publishing the final object stays atomic"
                    );
                }
                self.sftp
                    .options()
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .open(&scratch)
                    .await
                    .map_err(generic)?
            }
            Err(e) => return Err(generic(e)),
        };
        let wrote = async {
            f.write_all(bytes).await.map_err(generic)?;
            if self.fsync {
                f.sync_all().await.map_err(generic)?;
            }
            f.close().await.map_err(generic)
        }
        .await;
        if let Err(e) = wrote {
            self.remove_quiet(&scratch).await;
            return Err(e);
        }
        Ok(scratch)
    }

    async fn remove_quiet(&self, path: &str) {
        let mut fs = self.sftp.fs();
        let _ = fs.remove_file(path).await;
    }

    async fn meta_of(&self, full: &str) -> std::result::Result<ObjectMeta, StoreError> {
        let mut fs = self.sftp.fs();
        let m = fs.metadata(full).await.map_err(|e| {
            if is_not_found(&e) {
                StoreError::NotFound {
                    path: full.to_owned(),
                    source: "no such object".into(),
                }
            } else {
                generic(e)
            }
        })?;
        if is_dir(&m) {
            return Err(StoreError::NotFound {
                path: full.to_owned(),
                source: "is a directory".into(),
            });
        }
        to_meta(full, &m)
    }

    fn store_error(&self, location: &StorePath, e: SftpError) -> StoreError {
        if is_not_found(&e) {
            StoreError::NotFound {
                path: location.to_string(),
                source: "no such object".into(),
            }
        } else {
            generic(e)
        }
    }
}

/// 物件儲存：一個 key 一個檔案，實作 `object_store::ObjectStore`。
pub(crate) struct SftpStore(Arc<SftpInner>);

impl SftpStore {
    pub async fn open(cfg: &SftpConfig, auth: &SftpAuth) -> Result<Self> {
        let (keepalive, sftp, fsync) = connect(cfg, auth).await?;
        let root = format!("/{}", cfg.path.trim_matches('/'));
        let inner = Arc::new(SftpInner {
            sftp,
            fsync,
            relaxed: false,
            warned_no_o_excl: AtomicBool::new(false),
            stderr_tail: None,
            keepalive,
            root: root.clone(),
            display: format!(
                "sftp://{}@{}:{}/{}",
                cfg.user.clone().unwrap_or_default(),
                cfg.host,
                cfg.port,
                root
            ),
        });
        inner.ensure_dir(&root).await?;
        Ok(Self(inner))
    }

    /// rclone 橋接（寬鬆語意，見模組說明與 ADR 014）。served root 就是 rclone 的
    /// source，所以 SFTP 路徑的 root 是 `/`；repo 的目錄結構由 `ensure_dir` 在
    /// 各自的寫入路徑上按需建立。
    pub async fn open_rclone(cfg: &RcloneConfig) -> Result<Self> {
        let (child, sftp, fsync, tail) = connect_stdio(cfg).await?;
        let display = if cfg.remote.is_empty() {
            format!("rclone:///{}", cfg.path)
        } else {
            format!("rclone://{}/{}", cfg.remote, cfg.path)
        };
        let inner = Arc::new(SftpInner {
            sftp,
            fsync,
            relaxed: true,
            warned_no_o_excl: AtomicBool::new(false),
            stderr_tail: Some(tail),
            keepalive: Keepalive::Stdio { _child: child },
            root: String::new(),
            display,
        });
        Ok(Self(inner))
    }
}

impl fmt::Display for SftpStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SftpStore({})", self.0.display)
    }
}

impl fmt::Debug for SftpStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SftpStore")
            .field("root", &self.0.root)
            .finish()
    }
}

fn bytes_of(payload: &PutPayload) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.content_length());
    for chunk in payload.iter() {
        out.extend_from_slice(chunk);
    }
    out
}

/// `list` 的遞迴走訪（自由函式：要放進 `'static` 的 stream，抓 `Arc<Sftp>` 就好）。
/// 跳過 `.` 開頭的暫存殘骸。SFTP 一次列一個目錄；kist 的樹是淺的
/// （packs/、indexes/、snapshots/<client>/…）。
/// `root` 不帶尾斜線；回傳的 ObjectMeta.location 是**相對於 root** 的 key
/// （object_store 的語意），不是遠端絕對路徑。
async fn walk(
    sftp: &Sftp,
    root: &str,
    dir: &str,
    out: &mut Vec<ObjectMeta>,
) -> std::result::Result<(), StoreError> {
    let mut fs = sftp.fs();
    let d = match fs.open_dir(dir).await {
        Ok(d) => d,
        Err(e) => {
            return if is_not_found(&e) {
                Ok(()) // 這個 prefix 還沒有任何東西
            } else {
                Err(generic(e))
            };
        }
    };
    drop(fs);
    let mut entries = std::pin::pin!(d.read_dir());
    while let Some(entry) = entries.next().await {
        let entry = entry.map_err(generic)?;
        let name = entry.filename().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        let full = format!("{dir}/{name}");
        if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            Box::pin(walk(sftp, root, &full, out)).await?;
            continue;
        }
        let key = full
            .strip_prefix(root)
            .and_then(|r| r.strip_prefix('/'))
            .map(|r| r.to_owned())
            .unwrap_or_else(|| full.clone());
        if let Ok(m) = to_meta(&key, &entry.metadata()) {
            out.push(m);
        }
    }
    Ok(())
}

#[async_trait]
impl ObjectStore for SftpStore {
    async fn put_opts(
        &self,
        location: &StorePath,
        payload: PutPayload,
        opts: PutOptions,
    ) -> object_store::Result<PutResult> {
        let bytes = bytes_of(&payload);
        let inner = &*self.0;
        let full = inner.full(location);
        match opts.mode {
            PutMode::Create => {
                // 快路徑：已存在就早退（每晚對同一棵未變 tree 重複上傳時省下整份上傳）；
                // 真正的守門是後面的 hardlink（嚴格模式）或 publish 前的這次 stat（寬鬆模式）。
                if inner.meta_of(&full).await.is_ok() {
                    return Err(StoreError::AlreadyExists {
                        path: location.to_string(),
                        source: "exists".into(),
                    });
                }
                let scratch = inner.spool(dir_of(&full), &bytes).await?;
                let published = if inner.relaxed {
                    // rclone 橋接：hardlink 在 rclone 上不會動（宣稱支援、執行回
                    // OpUnsupported），退化成 posix-rename publish。race 視窗（兩個
                    // 寫入者同時通過上面的 stat）與緩解見模組說明與 ADR 014。
                    let mut fs = inner.sftp.fs();
                    let renamed = fs.rename(&scratch, &full).await;
                    drop(fs);
                    inner.remove_quiet(&scratch).await;
                    renamed.map(|_| ())
                } else {
                    let mut fs = inner.sftp.fs();
                    let linked = fs.hard_link(&scratch, &full).await;
                    drop(fs);
                    inner.remove_quiet(&scratch).await;
                    linked
                };
                match published {
                    Ok(()) => Ok(PutResult {
                        e_tag: None,
                        version: None,
                        extensions: Default::default(),
                    }),
                    Err(e) => {
                        // publish 被拒：用 stat 判別「已存在」還是別種錯誤
                        // （hardlink 是原子的，目標在的話就是完整的）。
                        let exists = inner.meta_of(&full).await.is_ok();
                        if exists {
                            Err(StoreError::AlreadyExists {
                                path: location.to_string(),
                                source: Box::new(e),
                            })
                        } else if !inner.relaxed && is_unsupported(&e) {
                            // 伺服器宣稱支援 hardlink 卻在實際 link 時拒絕（rclone
                            // 就是這樣）：給出指向橋接的明確錯誤，而不是莫名的 generic。
                            Err(StoreError::Generic {
                                store: "sftp",
                                source: format!(
                                    "server advertises hardlink@openssh.com but rejected the \
                                     link ({e}); kist needs it for atomic conditional writes — \
                                     if this is rclone serve sftp, use the rclone:// bridge \
                                     instead"
                                )
                                .into(),
                            })
                        } else {
                            Err(generic(e))
                        }
                    }
                }
            }
            PutMode::Overwrite => {
                let scratch = inner.spool(dir_of(&full), &bytes).await?;
                let mut fs = inner.sftp.fs();
                let renamed = fs.rename(&scratch, &full).await;
                drop(fs);
                // posix-rename 成功時暫存名已經不存在；失敗時清掉殘骸。
                inner.remove_quiet(&scratch).await;
                renamed.map_err(generic)?;
                Ok(PutResult {
                    e_tag: None,
                    version: None,
                    extensions: Default::default(),
                })
            }
            other => Err(StoreError::NotImplemented {
                operation: format!("put_opts with mode {other:?}"),
                implementer: "sftp".to_owned(),
            }),
        }
    }

    async fn put_multipart_opts(
        &self,
        _location: &StorePath,
        _opts: object_store::PutMultipartOptions,
    ) -> object_store::Result<Box<dyn object_store::MultipartUpload>> {
        Err(StoreError::NotImplemented {
            operation: "put_multipart_opts".to_owned(),
            implementer: "sftp".to_owned(),
        })
    }

    async fn get_opts(
        &self,
        location: &StorePath,
        options: GetOptions,
    ) -> object_store::Result<GetResult> {
        let inner = &*self.0;
        let full = inner.full(location);
        let meta = inner.meta_of(&full).await?;
        // GetRange（Offset/Suffix/Bounded）用伺服器給的長度收斂成具體範圍；
        // 不合法的範圍（start > end 等）直接當錯誤。
        let range: Range<u64> = match options.range {
            Some(r) => r.as_range(meta.size).map_err(|e| StoreError::Generic {
                store: "sftp",
                source: format!("invalid range for {full}: {e}").into(),
            })?,
            None => 0..meta.size,
        };
        if options.head {
            // head 請求（ObjectStoreExt::head 走這裡）：只要 meta，內容給空的。
            return Ok(GetResult {
                payload: GetResultPayload::Stream(
                    futures::stream::once(async { Ok(Bytes::new()) }).boxed(),
                ),
                meta,
                range: 0..0,
                attributes: Default::default(),
                extensions: Default::default(),
            });
        }
        let fh = inner
            .sftp
            .open(&full)
            .await
            .map_err(|e| inner.store_error(location, e))?;
        let mut tf = std::pin::pin!(TokioCompatFile::from(fh));
        use tokio::io::{AsyncReadExt as _, AsyncSeekExt as _};
        if range.start > 0 {
            tf.as_mut()
                .seek(std::io::SeekFrom::Start(range.start))
                .await
                .map_err(generic)?;
        }
        let mut buf = vec![0u8; (range.end - range.start) as usize];
        tf.as_mut().read_exact(&mut buf).await.map_err(generic)?;
        Ok(GetResult {
            payload: GetResultPayload::Stream(
                futures::stream::once(async move { Ok(Bytes::from(buf)) }).boxed(),
            ),
            meta,
            range,
            attributes: Default::default(),
            extensions: Default::default(),
        })
    }

    async fn get_ranges(
        &self,
        location: &StorePath,
        ranges: &[Range<u64>],
    ) -> object_store::Result<Vec<Bytes>> {
        // 一次讀全檔再切片：kist 的 range 讀（trailer、多段）都落在同一個物件上，
        // 物件 ≤ 128 MiB；整讀換程式直白。
        let inner = &*self.0;
        let full = inner.full(location);
        let meta = inner.meta_of(&full).await?;
        let fh = inner
            .sftp
            .open(&full)
            .await
            .map_err(|e| inner.store_error(location, e))?;
        let mut tf = std::pin::pin!(TokioCompatFile::from(fh));
        use tokio::io::AsyncReadExt as _;
        let mut buf = Vec::with_capacity(meta.size as usize);
        tf.as_mut().read_to_end(&mut buf).await.map_err(generic)?;
        // 切片前用**實際讀到的長度**做邊界檢查：index 壞掉或 pack 被截斷時回乾淨的
        // 錯誤，而不是切片 panic。
        for r in ranges {
            if r.end > buf.len() as u64 || r.start > r.end {
                return Err(StoreError::Generic {
                    store: "sftp",
                    source: format!("range {r:?} out of bounds for {full} ({} bytes)", buf.len())
                        .into(),
                });
            }
        }
        Ok(ranges
            .iter()
            .map(|r| Bytes::copy_from_slice(&buf[r.start as usize..r.end as usize]))
            .collect())
    }

    fn delete_stream(
        &self,
        locations: BoxStream<'static, object_store::Result<StorePath>>,
    ) -> BoxStream<'static, object_store::Result<StorePath>> {
        let inner = self.0.clone();
        locations
            .map(move |loc| {
                let inner = inner.clone();
                async move {
                    let loc = loc?;
                    let full = inner.full(&loc);
                    let mut fs = inner.sftp.fs();
                    match fs.remove_file(&full).await {
                        Ok(()) => Ok(Some(loc)),
                        // 刪不存在的物件視為已刪（與 S3 一致）。**要回報該路徑**：
                        // object_store 的單鍵 delete 包裝要求 delete_stream 對一個
                        // location 剛好 yield 一次，靜默跳過會讓它變成錯誤。
                        Err(e) if is_not_found(&e) => Ok(Some(loc)),
                        Err(e) => Err(generic(e)),
                    }
                }
            })
            .buffered(10)
            .filter_map(|r| async move {
                match r {
                    Ok(Some(path)) => Some(Ok(path)),
                    Ok(None) => None,
                    Err(e) => Some(Err(e)),
                }
            })
            .boxed()
    }

    fn list(
        &self,
        prefix: Option<&StorePath>,
    ) -> BoxStream<'static, object_store::Result<ObjectMeta>> {
        let sftp = self.0.sftp.clone();
        let dir = match prefix {
            Some(p) => format!("{}/{}", self.0.root, p),
            None => self.0.root.clone(),
        };
        // rclone 模式的 root 是空字串（served root 即 SFTP 根）；SFTP 路徑要絕對。
        let dir = if dir.is_empty() { "/".to_owned() } else { dir };
        let root = self.0.root.clone();
        let task = async move {
            let mut out = Vec::new();
            walk(&sftp, &root, &dir, &mut out).await?;
            Ok::<_, StoreError>(out)
        };
        futures::stream::once(task)
            .map(|r| match r {
                Ok(metas) => futures::stream::iter(metas.into_iter().map(Ok)).boxed(),
                Err(e) => futures::stream::once(async move { Err(e) }).boxed(),
            })
            .flatten()
            .boxed()
    }

    async fn list_with_delimiter(
        &self,
        prefix: Option<&StorePath>,
    ) -> object_store::Result<ListResult> {
        // key 一樣回相對 root 的形式（跟 list 一致）。
        let root = self.0.root.clone();
        let dir = match prefix {
            Some(p) => format!("{}/{}", root, p),
            None => root.clone(),
        };
        let dir = if dir.is_empty() { "/".to_owned() } else { dir };
        let mut fs = self.0.sftp.fs();
        let d = match fs.open_dir(&dir).await {
            Ok(d) => d,
            Err(e) if is_not_found(&e) => {
                return Ok(ListResult {
                    common_prefixes: vec![],
                    objects: vec![],
                    extensions: Default::default(),
                });
            }
            Err(e) => return Err(generic(e)),
        };
        drop(fs);
        let rel = |full: &str| {
            full.strip_prefix(&root)
                .and_then(|r| r.strip_prefix('/'))
                .map(|r| r.to_owned())
                .unwrap_or_else(|| full.to_owned())
        };
        let mut objects = Vec::new();
        let mut common_prefixes = Vec::new();
        let mut entries = std::pin::pin!(d.read_dir());
        while let Some(entry) = entries.next().await {
            let entry = entry.map_err(generic)?;
            let name = entry.filename().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            let full = format!("{dir}/{name}");
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                match StorePath::parse(rel(&full)) {
                    Ok(p) => common_prefixes.push(p),
                    // 伺服器給了 object_store 命名規則外的名稱：略過，不讓整個 list 失敗
                    Err(_) => continue,
                }
            } else if let Ok(m) = to_meta(&rel(&full), &entry.metadata()) {
                objects.push(m);
            }
        }
        common_prefixes.sort();
        Ok(ListResult {
            common_prefixes,
            objects,
            extensions: Default::default(),
        })
    }

    async fn copy_opts(
        &self,
        from: &StorePath,
        to: &StorePath,
        _options: object_store::CopyOptions,
    ) -> object_store::Result<()> {
        let inner = &*self.0;
        let src = inner.full(from);
        let dst = inner.full(to);
        let fh = inner
            .sftp
            .open(&src)
            .await
            .map_err(|e| inner.store_error(from, e))?;
        let mut tf = std::pin::pin!(TokioCompatFile::from(fh));
        use tokio::io::AsyncReadExt as _;
        let mut buf = Vec::new();
        tf.as_mut().read_to_end(&mut buf).await.map_err(generic)?;
        let scratch = inner.spool(dir_of(&dst), &buf).await?;
        let mut fs = inner.sftp.fs();
        let renamed = fs.rename(&scratch, &dst).await;
        drop(fs);
        inner.remove_quiet(&scratch).await;
        renamed.map_err(generic)
    }
}
