//! `kist serve` 的 HTTP 端：`/metrics`（OpenMetrics 文字，給 Prometheus 抓）、`/healthz`，
//! 以及 Web UI（`/`、`/ui/*`、`/static/*`；maud 在編譯期把 Rust 函式產生 HTML + htmx，靜態檔用 rust-embed 包進 binary）。
//!
//! UI 的三道門（`ui_guard`，依序）：
//! 1. `Host` header 逐字比對（防 DNS rebinding）：綁定位址、`localhost:<port>`、`127.0.0.1:<port>`、
//!    `[::1]:<port>` 與 `[serve] allowed_hosts`；不折疊大小寫、不補預設埠。不對 → 421。
//! 2. HTTP Basic auth（只在 `[serve] password_file` 有設時）：帳號任意、只比密碼，
//!    比對用 `blake3::hash` 兩邊各算一次再比（`Hash` 的 `==` 是常數時間，長度也藏起來）。
//! 3. POST（改狀態的請求）：沒設密碼一律 403；CSRF 靠 `HX-Request: true`（htmx 一定送，
//!    跨站頁面沒有 CORS 就加不了自訂 header）且不能 `Sec-Fetch-Site: cross-site`。
//!
//! `/metrics`、`/healthz` 不經過這些門（既有的 scrape 設定不能壞）。只綁使用者指定的位址
//! （CLI 預設 127.0.0.1）；Basic auth 的密碼走明文 HTTP——不要直接暴露到網路上。

use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::{Path, Request, State};
use axum::http::{header, HeaderMap, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use kist_core::SnapshotInfo;
use maud::{html, Markup, DOCTYPE};
use time::OffsetDateTime;
use tokio::net::TcpListener;
use tokio::sync::{watch, Semaphore};
use zeroize::Zeroizing;

use crate::client_id;
use crate::config::Config;
use crate::daemon::{configured, DaemonHandle};
use crate::jobs::{open_repo, JobKind, JobStatus};
use crate::metrics::{unix_now, Metrics};
use crate::Result;

/// 所有 handler 共用的狀態。`Clone` 很便宜（都是 Arc / handle）。
#[derive(Clone)]
pub struct ServeState {
    pub metrics: Arc<Metrics>,
    pub daemon: DaemonHandle,
    pub ui: Arc<UiConfig>,
}

/// Web UI 的設定：啟動時算好一次。
pub struct UiConfig {
    /// `[serve] password_file` 的內容；`None` = UI 唯讀、不用登入、POST 一律 403。
    pub password: Option<Zeroizing<String>>,
    /// 完整的 Host 白名單：綁定位址的幾種 loopback 寫法 + 設定檔的清單。
    pub allowed_hosts: HashSet<String>,
    /// 一次只開一個 repo（每次開都要跑 Argon2，約 0.2–1.5 s；不把 key 留在記憶體裡）。
    pub open_gate: Semaphore,
}

/// 讀 UI 密碼檔、依綁定位址算 Host 白名單。
pub fn ui_config(cfg: &Config, bound: SocketAddr) -> Result<UiConfig> {
    let password = cfg.read_ui_password()?;
    let port = bound.port();
    let mut allowed_hosts: HashSet<String> = HashSet::new();
    allowed_hosts.insert(bound.to_string());
    allowed_hosts.insert(format!("localhost:{port}"));
    allowed_hosts.insert(format!("127.0.0.1:{port}"));
    allowed_hosts.insert(format!("[::1]:{port}"));
    if let Some(s) = &cfg.serve {
        allowed_hosts.extend(s.allowed_hosts.iter().cloned());
    }
    Ok(UiConfig {
        password,
        allowed_hosts,
        open_gate: Semaphore::new(1),
    })
}

/// `static/` 整個包進 binary（htmx.min.js、style.css、htmx.LICENSE）。
#[derive(rust_embed::Embed)]
#[folder = "static/"]
struct Assets;

pub async fn serve_http(
    state: ServeState,
    listener: TcpListener,
    shutdown: watch::Receiver<bool>,
) -> std::io::Result<()> {
    // UI 的路由掛在自己的 router 上，`route_layer` 只包這些路由（沒對到的 404 不經過門）。
    let ui = axum::Router::new()
        .route("/", axum::routing::get(index))
        .route("/ui/status", axum::routing::get(status_partial))
        .route("/ui/snapshots", axum::routing::get(snapshots_partial))
        .route("/ui/jobs/backup", axum::routing::post(trigger_backup))
        .route("/static/{file}", axum::routing::get(static_file))
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            ui_guard,
        ));
    let app = axum::Router::new()
        .route("/metrics", axum::routing::get(metrics_handler))
        .route("/healthz", axum::routing::get(|| async { "ok" }))
        .merge(ui)
        .with_state(state);
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal(shutdown))
        .await
}

/// 等 watch channel 變 true。`wait_for` 會先看當前值（訊號已發過就立即返回）；
/// sender 消失（`Err`）也視為關機。
async fn shutdown_signal(mut rx: watch::Receiver<bool>) {
    let _ = rx.wait_for(|shutdown| *shutdown).await;
}

/// prometheus-client 輸出的是 OpenMetrics 文字格式（結尾有 `# EOF`）；
/// 舊的 text/plain 0.0.4 抓取器也讀得懂（`#` 開頭當註解）。
async fn metrics_handler(State(state): State<ServeState>) -> impl IntoResponse {
    (
        [(
            header::CONTENT_TYPE,
            "application/openmetrics-text; version=1.0.0; charset=utf-8",
        )],
        state.metrics.render(),
    )
}

// ---- 中介層：Host → auth → POST 規則 ----------------------------------------------------

/// UI 路由的門（模組說明有講順序與理由）。拒絕都回純文字。
async fn ui_guard(State(state): State<ServeState>, req: Request, next: Next) -> Response {
    let ui = &state.ui;
    // 1. Host：逐字比對
    let host_ok = req
        .headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|h| ui.allowed_hosts.contains(h));
    if !host_ok {
        return (
            StatusCode::MISDIRECTED_REQUEST,
            "host not allowed; add it to [serve] allowed_hosts\n",
        )
            .into_response();
    }
    // 2. Basic auth（有設密碼才要）
    if let Some(expected) = &ui.password {
        let given = req
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(basic_password);
        let ok = given.is_some_and(|g| blake3::hash(&g) == blake3::hash(expected.as_bytes()));
        if !ok {
            return (
                StatusCode::UNAUTHORIZED,
                [(header::WWW_AUTHENTICATE, "Basic realm=\"kist\"")],
                "authentication required\n",
            )
                .into_response();
        }
    }
    // 3. POST：沒密碼不准動；CSRF
    if req.method() == Method::POST {
        if ui.password.is_none() {
            return (
                StatusCode::FORBIDDEN,
                "web UI actions are disabled: set [serve] password_file\n",
            )
                .into_response();
        }
        let hx = header_is(req.headers(), "hx-request", "true");
        let cross_site = header_is(req.headers(), "sec-fetch-site", "cross-site");
        if !hx || cross_site {
            return (StatusCode::FORBIDDEN, "cross-site request rejected\n").into_response();
        }
    }
    next.run(req).await
}

fn header_is(headers: &HeaderMap, name: &str, want: &str) -> bool {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v == want)
}

/// `Authorization: Basic base64(user:password)` 裡的 password 部分；格式不對 → `None`。
/// scheme 不分大小寫（RFC 7235）；帳號不看。
fn basic_password(value: &str) -> Option<Zeroizing<Vec<u8>>> {
    let (scheme, rest) = value.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("basic") {
        return None;
    }
    let decoded = Zeroizing::new(base64_decode(rest.trim())?);
    let colon = decoded.iter().position(|b| *b == b':')?;
    Some(Zeroizing::new(decoded[colon + 1..].to_vec()))
}

/// 最小的 RFC 4648 base64 解碼（標準字母表）：接受有沒有 `=` 補齊都可以、
/// 拒絕字母表以外的字元與不可能的長度（餘 1）。只給 Basic auth 用，不需要更多。
fn base64_decode(input: &str) -> Option<Vec<u8>> {
    let bytes = input.as_bytes();
    let body = bytes
        .strip_suffix(b"==")
        .or_else(|| bytes.strip_suffix(b"="))
        .unwrap_or(bytes);
    if body.len() % 4 == 1 {
        return None;
    }
    let mut out = Vec::with_capacity(body.len() * 3 / 4);
    let mut acc: u32 = 0;
    let mut bits: u32 = 0;
    for &c in body {
        let v: u32 = match c {
            b'A'..=b'Z' => u32::from(c - b'A'),
            b'a'..=b'z' => u32::from(c - b'a') + 26,
            b'0'..=b'9' => u32::from(c - b'0') + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        };
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(u8::try_from((acc >> bits) & 0xff).ok()?);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

// ---- 頁面 ------------------------------------------------------------------------------

/// htmx 2 預設不把 4xx 換進頁面；409（已排隊 / 未設定）帶的是含 notice 的 status partial，要換進去。
const HTMX_CONFIG: &str = r#"{"responseHandling":[{"code":"204","swap":false},{"code":"409","swap":true},{"code":"[23]..","swap":true},{"code":"[45]..","swap":false,"error":true}]}"#;

/// 首頁整頁；status partial 直接嵌在裡面（原 askama 模板的 `{% include %}`）。
fn index_page(repo: &str, hostname: &str, status: &StatusView) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { "kist" }
                meta name="htmx-config" content=(HTMX_CONFIG);
                link rel="stylesheet" href="/static/style.css";
                script src="/static/htmx.min.js" {}
            }
            body {
                header {
                    h1 { "kist" }
                    p.meta { "repo " code { (repo) } " on " code { (hostname) } }
                }
                main {
                    section {
                        h2 { "Status" }
                        div #status hx-get="/ui/status" hx-trigger="every 2s" hx-swap="innerHTML" {
                            (render_status(status))
                        }
                    }
                    section {
                        h2 {
                            "Snapshots "
                            button.small hx-get="/ui/snapshots" hx-target="#snapshots" hx-swap="innerHTML" {
                                "Refresh"
                            }
                        }
                        div #snapshots hx-get="/ui/snapshots" hx-trigger="load" hx-swap="innerHTML" {
                            "Loading…"
                        }
                    }
                }
            }
        }
    }
}

/// 首頁的 Status 區塊，也是 htmx 每 2 秒輪詢與「Run backup now」換進來的 partial。
fn render_status(status: &StatusView) -> Markup {
    html! {
        @if let Some(n) = &status.notice {
            p.notice { (n) }
        }
        @if let Some(r) = &status.running {
            p.running { "Running " strong { (r.job) } " since " (r.started) " (" (r.elapsed) " s)" }
            @if let Some(p) = &r.progress {
                table.progress {
                    tr { th { "phase" } td { (p.phase) } }
                    tr { th { "files" } td { (p.files) } }
                    tr { th { "dirs" } td { (p.dirs) } }
                    tr { th { "bytes total" } td { (p.bytes_total) } }
                    tr { th { "bytes new" } td { (p.bytes_new) } }
                    tr { th { "errors" } td { (p.errors) } }
                    @if let Some(c) = &p.current {
                        tr { th { "current" } td.path { (c) } }
                    }
                }
            }
        } @else {
            p.idle { "Idle." }
        }
        @if let Some(q) = &status.queued {
            p.queued { "queued: " (q) }
        }
        @if status.can_run_backup {
            @if status.actions_enabled {
                p {
                    button hx-post="/ui/jobs/backup" hx-target="#status" hx-swap="innerHTML" {
                        "Run backup now"
                    }
                }
            } @else {
                p.disabled { "Actions are disabled: set [serve] password_file" }
            }
        }
        h3 { "Schedules" }
        table {
            thead {
                tr { th { "job" } th { "cron" } th { "next run" } }
            }
            tbody {
                @for s in &status.schedules {
                    tr { td { (s.job) } td { code { (s.cron) } } td { (s.next) } }
                }
            }
        }
        h3 { "Recent jobs" }
        @if status.history.is_empty() {
            p.idle { "No jobs have run since start." }
        } @else {
            table {
                thead {
                    tr { th { "started" } th { "job" } th { "status" } th { "duration" } th { "error" } }
                }
                tbody {
                    @for h in &status.history {
                        tr {
                            td { (h.started) }
                            td { (h.job) }
                            td class=(h.class) { (h.status) }
                            td { (h.duration) }
                            td.path { @if let Some(e) = &h.error { (e) } }
                        }
                    }
                }
            }
        }
    }
}

/// snapshots 列表的 partial；`error` 是開 repo / 列表失敗的說明文字。
fn render_snapshots(error: Option<&str>, rows: &[SnapshotRow]) -> Markup {
    html! {
        @if let Some(e) = error {
            p.bad { (e) }
        } @else if rows.is_empty() {
            p.idle { "No snapshots yet." }
        } @else {
            table {
                thead {
                    tr { th { "client" } th { "time" } th { "hostname" } th { "files" } th { "size" } th { "paths" } }
                }
                tbody {
                    @for r in rows {
                        tr {
                            td { code { (r.client) } }
                            td { (r.time) }
                            td { (r.hostname) }
                            td { (r.files) }
                            td { (r.size) }
                            td.path { (r.paths) }
                        }
                    }
                }
            }
        }
    }
}

/// 渲染要的都先算成字串，maud 函式裡只有迴圈與 if。
struct StatusView {
    notice: Option<String>,
    running: Option<RunningView>,
    /// 排隊中的工作名稱（逗號分隔）；沒有就 `None`。
    queued: Option<String>,
    /// 設定裡有 `[backup]` 才顯示按鈕（或「已停用」的說明）。
    can_run_backup: bool,
    /// 有設 UI 密碼才開放動作。
    actions_enabled: bool,
    schedules: Vec<ScheduleRow>,
    history: Vec<HistoryRow>,
}

struct RunningView {
    job: String,
    started: String,
    /// 已經跑了幾秒（整數字串）。
    elapsed: String,
    progress: Option<ProgressView>,
}

struct ProgressView {
    phase: String,
    files: String,
    dirs: String,
    bytes_total: String,
    bytes_new: String,
    errors: String,
    current: Option<String>,
}

struct ScheduleRow {
    job: String,
    cron: String,
    /// 下一次時間（RFC 3339）、`manual only`（沒排程）或 `never`（cron 找不到下一次）。
    next: String,
}

struct HistoryRow {
    started: String,
    job: String,
    status: String,
    /// CSS class：`ok` / `bad`。
    class: String,
    duration: String,
    error: Option<String>,
}

struct SnapshotRow {
    /// client id 的前 8 個 hex。
    client: String,
    time: String,
    hostname: String,
    files: String,
    size: String,
    paths: String,
}

/// maud 在編譯期產生 HTML，沒有執行期錯誤；這裡只是補上狀態碼。
fn render(status: StatusCode, markup: Markup) -> Response {
    (status, markup).into_response()
}

async fn index(State(state): State<ServeState>) -> Response {
    let status = status_view(&state, None);
    let markup = index_page(&state.daemon.config().repo, &client_id::hostname(), &status);
    render(StatusCode::OK, markup)
}

async fn status_partial(State(state): State<ServeState>) -> Response {
    let markup = render_status(&status_view(&state, None));
    render(StatusCode::OK, markup)
}

/// 「Run backup now」。排進去了 → 200 的 status partial；排不進去（已有一件在排隊、
/// 沒有 `[backup]`、daemon 已停）→ 409 + 帶 notice 的同一個 partial。
async fn trigger_backup(State(state): State<ServeState>) -> Response {
    let (status, notice) = match state.daemon.trigger(JobKind::Backup) {
        Ok(()) => (StatusCode::OK, None),
        Err(e) => (StatusCode::CONFLICT, Some(e.to_string())),
    };
    let markup = render_status(&status_view(&state, notice));
    render(status, markup)
}

/// snapshots 列表。開 repo 失敗也回 200（partial 裡顯示錯誤文字，htmx 才會換進頁面）。
async fn snapshots_partial(State(state): State<ServeState>) -> Response {
    let markup = match list_snapshots(&state).await {
        Ok(rows) => render_snapshots(None, &rows),
        Err(e) => render_snapshots(Some(&e), &[]),
    };
    render(StatusCode::OK, markup)
}

/// 在 semaphore 裡開 repo、列 snapshot；新的在前。
async fn list_snapshots(state: &ServeState) -> std::result::Result<Vec<SnapshotRow>, String> {
    let _permit = state
        .ui
        .open_gate
        .acquire()
        .await
        .map_err(|e| e.to_string())?;
    let repo = open_repo(state.daemon.config())
        .await
        .map_err(|e| e.to_string())?;
    let infos = repo.list_snapshots().await.map_err(|e| e.to_string())?;
    Ok(infos.iter().rev().map(snapshot_row).collect())
}

/// snapshot 的 i64 奈秒 → 人類可讀的 UTC 時間。
fn format_ns_time(ns: i64) -> String {
    time::OffsetDateTime::from_unix_timestamp_nanos(i128::from(ns))
        .map(|t| {
            t.format(&time::format_description::well_known::Rfc3339)
                .unwrap_or_default()
        })
        .unwrap_or_default()
}

fn snapshot_row(info: &SnapshotInfo) -> SnapshotRow {
    let s = &info.snapshot;
    SnapshotRow {
        client: info.client_hex().chars().take(8).collect(),
        time: format_ns_time(s.time_ns),
        hostname: s.host.clone(),
        files: s.stats.files.to_string(),
        size: human_bytes(s.stats.bytes),
        paths: s
            .roots
            .iter()
            .map(|r| String::from_utf8_lossy(r.path.as_slice()).into_owned())
            .collect::<Vec<_>>()
            .join(", "),
    }
}

async fn static_file(Path(file): Path<String>) -> Response {
    let Some(asset) = Assets::get(&file) else {
        return (StatusCode::NOT_FOUND, "not found\n").into_response();
    };
    let content_type = match file.rsplit('.').next() {
        Some("js") => "application/javascript",
        Some("css") => "text/css",
        _ => "application/octet-stream",
    };
    (
        [
            (header::CONTENT_TYPE, content_type),
            (header::CACHE_CONTROL, "public, max-age=86400"),
        ],
        asset.data.into_owned(),
    )
        .into_response()
}

/// 把 daemon 的狀態整理成模板要的字串。
fn status_view(state: &ServeState, notice: Option<String>) -> StatusView {
    let cfg = state.daemon.config();
    let st = state.daemon.state();
    let running = st.running.as_ref().map(|r| RunningView {
        job: r.job.name().to_owned(),
        started: trim_fraction(&r.started),
        elapsed: format!("{:.0}", (unix_now() - r.started_unix).max(0.0)),
        progress: r.progress.as_ref().map(|p| ProgressView {
            phase: p.phase.to_owned(),
            files: p.stats.files.to_string(),
            dirs: p.stats.dirs.to_string(),
            bytes_total: human_bytes(p.stats.bytes),
            bytes_new: human_bytes(p.report.bytes_stored),
            errors: p.report.errors.to_string(),
            current: p.current.clone(),
        }),
    });
    let queued = if st.queued.is_empty() {
        None
    } else {
        Some(
            st.queued
                .iter()
                .map(|k| k.name())
                .collect::<Vec<_>>()
                .join(", "),
        )
    };
    // 排程表：設定裡有的工作都列；沒寫 schedule 的是 manual only
    let crons = state.daemon.schedules();
    let nexts = state.daemon.next_runs(OffsetDateTime::now_utc());
    let mut schedules = Vec::new();
    for kind in [JobKind::Backup, JobKind::Forget, JobKind::Prune] {
        if !configured(cfg, kind) {
            continue;
        }
        let cron = crons
            .iter()
            .find(|(k, _)| *k == kind)
            .map(|(_, c)| c.clone());
        let next = nexts.iter().find(|(k, _)| *k == kind).and_then(|(_, t)| *t);
        let next = match (&cron, next) {
            (None, _) => "manual only".to_owned(),
            (Some(_), Some(t)) => format_time(t),
            (Some(_), None) => "never".to_owned(),
        };
        schedules.push(ScheduleRow {
            job: kind.name().to_owned(),
            cron: cron.unwrap_or_else(|| "-".to_owned()),
            next,
        });
    }
    let history = st
        .history
        .iter()
        .map(|o| HistoryRow {
            started: trim_fraction(&o.started),
            job: o.job.name().to_owned(),
            status: o.status.name().to_owned(),
            class: match o.status {
                JobStatus::Success => "ok",
                JobStatus::Incomplete | JobStatus::Failure => "bad",
            }
            .to_owned(),
            duration: format!("{:.1} s", o.duration_secs),
            error: o.error.clone(),
        })
        .collect();
    StatusView {
        notice,
        running,
        queued,
        can_run_backup: cfg.backup.is_some(),
        actions_enabled: state.ui.password.is_some(),
        schedules,
        history,
    }
}

fn format_time(t: OffsetDateTime) -> String {
    t.format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

/// RFC 3339 去掉小數秒（`2026-09-05T01:59:31.133849068Z` → `2026-09-05T01:59:31Z`），給人看的。
fn trim_fraction(rfc3339: &str) -> String {
    match rfc3339.split_once('.') {
        Some((head, tail)) => {
            let rest = tail.trim_start_matches(|c: char| c.is_ascii_digit());
            format!("{head}{rest}")
        }
        None => rfc3339.to_owned(),
    }
}

/// 1023 B、1.5 KiB、2.0 MiB… 給人看的。
fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_decodes_valid_input() {
        assert_eq!(base64_decode(""), Some(Vec::new()));
        assert_eq!(base64_decode("Zg=="), Some(b"f".to_vec()));
        assert_eq!(base64_decode("Zm8="), Some(b"fo".to_vec()));
        assert_eq!(base64_decode("Zm9v"), Some(b"foo".to_vec()));
        assert_eq!(base64_decode("Zm9vYg=="), Some(b"foob".to_vec()));
        assert_eq!(base64_decode("Zm9vYmE="), Some(b"fooba".to_vec()));
        assert_eq!(base64_decode("Zm9vYmFy"), Some(b"foobar".to_vec()));
        // `printf 'u:ui secret' | base64`
        assert_eq!(
            base64_decode("dTp1aSBzZWNyZXQ="),
            Some(b"u:ui secret".to_vec())
        );
        // `+` 與 `/`
        assert_eq!(base64_decode("+/8="), Some(vec![0xfb, 0xff]));
    }

    #[test]
    fn base64_accepts_missing_padding() {
        assert_eq!(base64_decode("Zg"), Some(b"f".to_vec()));
        assert_eq!(base64_decode("Zm8"), Some(b"fo".to_vec()));
        assert_eq!(
            base64_decode("dTp1aSBzZWNyZXQ"),
            Some(b"u:ui secret".to_vec())
        );
    }

    #[test]
    fn base64_rejects_invalid_input() {
        assert_eq!(base64_decode("%%%not-base64"), None);
        assert_eq!(base64_decode("Zm9v Zg=="), None);
        assert_eq!(base64_decode("Zm9-"), None);
        // 餘 1 的長度不可能是 base64
        assert_eq!(base64_decode("Z"), None);
        assert_eq!(base64_decode("Zm9vZ"), None);
    }

    #[test]
    fn basic_password_extracts_password_part() {
        let pw = basic_password("Basic dTp1aSBzZWNyZXQ=").map(|p| p.to_vec());
        assert_eq!(pw, Some(b"ui secret".to_vec()));
        // scheme 不分大小寫；帳號可以是空的
        let pw = basic_password("basic OnB3").map(|p| p.to_vec());
        assert_eq!(pw, Some(b"pw".to_vec()));
        // 沒有冒號、錯的 scheme、壞的 base64
        assert!(basic_password("Basic dXNlcg==").is_none());
        assert!(basic_password("Bearer dTp1aSBzZWNyZXQ=").is_none());
        assert!(basic_password("Basic %%%").is_none());
        assert!(basic_password("Basic").is_none());
    }

    #[test]
    fn allowed_hosts_contain_loopback_forms_and_config_entries() {
        let cfg = Config::parse(
            "repo = \"/x\"\npassword_file = \"/p\"\n[backup]\npaths = [\"/e\"]\n[serve]\nallowed_hosts = [\"backup.example.internal:9898\"]\n",
        )
        .unwrap_or_else(|e| panic!("{e}"));
        let bound: SocketAddr = "127.0.0.1:8080".parse().unwrap_or_else(|e| panic!("{e}"));
        let ui = ui_config(&cfg, bound).unwrap_or_else(|e| panic!("{e}"));
        for host in [
            "127.0.0.1:8080",
            "localhost:8080",
            "[::1]:8080",
            "backup.example.internal:9898",
        ] {
            assert!(ui.allowed_hosts.contains(host), "{host}");
        }
        assert!(!ui.allowed_hosts.contains("localhost"));
        assert!(!ui.allowed_hosts.contains("127.0.0.1:8081"));
        assert!(ui.password.is_none());

        // 綁在 0.0.0.0：綁定位址本身也在名單裡（四種 loopback 之外的第五個）
        let bound: SocketAddr = "0.0.0.0:9898".parse().unwrap_or_else(|e| panic!("{e}"));
        let ui = ui_config(&cfg, bound).unwrap_or_else(|e| panic!("{e}"));
        assert!(ui.allowed_hosts.contains("0.0.0.0:9898"));
        assert!(ui.allowed_hosts.contains("[::1]:9898"));
    }

    #[test]
    fn trim_fraction_keeps_offset() {
        assert_eq!(
            trim_fraction("2026-09-05T01:59:31.133849068Z"),
            "2026-09-05T01:59:31Z"
        );
        assert_eq!(
            trim_fraction("2026-09-05T09:59:31.5+08:00"),
            "2026-09-05T09:59:31+08:00"
        );
        assert_eq!(
            trim_fraction("2026-09-05T01:59:31Z"),
            "2026-09-05T01:59:31Z"
        );
        assert_eq!(trim_fraction(""), "");
    }

    #[test]
    fn human_bytes_is_readable() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(1023), "1023 B");
        assert_eq!(human_bytes(1024), "1.0 KiB");
        assert_eq!(human_bytes(1536), "1.5 KiB");
        assert_eq!(human_bytes(5 * 1024 * 1024), "5.0 MiB");
        assert_eq!(human_bytes(3 * 1024 * 1024 * 1024 * 1024), "3.0 TiB");
        assert_eq!(human_bytes(3000 * 1024 * 1024 * 1024 * 1024), "3000.0 TiB");
    }
}
