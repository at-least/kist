//! `kist serve` 的 Web UI：Host 檢查（DNS rebinding）、HTTP Basic auth、POST 的 CSRF 規則、
//! 靜態檔、狀態與 snapshots 的 partial、「Run backup now」真的會跑 backup。
//! `/metrics` 與 `/healthz` 不受這些規則影響。

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;

use kist_app::server::{serve_http, ui_config, ServeState};
use kist_app::{Config, Daemon, DaemonHandle};
use kist_core::{InitOptions, Repository};
use tokio::sync::watch;

const PASSWORD: &str = "ui http test password";
const UI_PASSWORD: &str = "ui secret";

fn write_password(dir: &Path) -> std::path::PathBuf {
    let p = dir.join("password");
    std::fs::write(&p, format!("{PASSWORD}\n")).unwrap();
    p
}

async fn init_repo(dir: &Path) -> std::path::PathBuf {
    let repo = dir.join("repo");
    let backend = kist_backend::Backend::local(&repo).unwrap();
    let opts = InitOptions {
        kdf_cost: kist_crypto::KdfCost {
            m_cost_kib: 8,
            t_cost: 1,
            p_cost: 1,
        },
        ..InitOptions::default()
    };
    Repository::init(backend, PASSWORD.as_bytes(), opts)
        .await
        .unwrap();
    repo
}

fn config_text(repo: &Path, password: &Path, dir: &Path, extra: &str) -> String {
    format!(
        r#"
repo = {repo:?}
password_file = {password:?}
client_id_file = {cid:?}
cache_dir = {cache:?}
timezone = "utc"
{extra}
"#,
        repo = repo.display().to_string(),
        password = password.display().to_string(),
        cid = dir.join("client-id").display().to_string(),
        cache = dir.join("cache").display().to_string(),
    )
}

/// 測試用的 base64 編碼（標準字母表、`=` 補齊）；server 端是解碼，這裡自己編一份，
/// 不拉 base64 crate。
fn base64_encode(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in input.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

fn basic_auth(password: &str) -> String {
    format!(
        "Basic {}",
        base64_encode(format!("u:{password}").as_bytes())
    )
}

/// 一個 HTTP/1.1 請求（`Connection: close`），回 (狀態行, 全部 header 小寫, body)。
/// 非 async 的 client 會把 #[tokio::test] 的單執行緒 runtime 擋死，所以用 tokio 的 TcpStream。
async fn request(
    addr: SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> (String, String, String) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut req = format!("{method} {path} HTTP/1.1\r\n");
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str(&format!(
        "Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    ));
    let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
    s.write_all(req.as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    s.read_to_end(&mut buf).await.unwrap();
    let text = String::from_utf8(buf).unwrap();
    let (head, body) = text
        .split_once("\r\n\r\n")
        .unwrap_or_else(|| panic!("no header/body split in response:\n{text}"));
    let (status, headers) = head.split_once("\r\n").unwrap_or((head, ""));
    (
        status.to_owned(),
        headers.to_ascii_lowercase(),
        body.to_owned(),
    )
}

struct Server {
    addr: SocketAddr,
    host: String,
    hostname: String,
    handle: DaemonHandle,
    shutdown: watch::Sender<bool>,
    daemon_task: tokio::task::JoinHandle<kist_app::Result<()>>,
    server_task: tokio::task::JoinHandle<std::io::Result<()>>,
    _dir: tempfile::TempDir,
}

impl Server {
    async fn stop(self) {
        self.shutdown.send(true).unwrap();
        self.daemon_task.await.unwrap().unwrap();
        self.server_task.await.unwrap().unwrap();
    }
}

/// 起一個完整的 serve：本機 repo、3 個檔案的來源、`[backup]` 無排程、可選的 `[serve]` 密碼。
async fn start(with_ui_password: bool) -> Server {
    let dir = tempfile::tempdir().unwrap();
    let repo = init_repo(dir.path()).await;
    let password = write_password(dir.path());
    let src = dir.path().join("src");
    std::fs::create_dir_all(src.join("sub")).unwrap();
    std::fs::write(src.join("a.txt"), b"hello").unwrap();
    std::fs::write(src.join("b.txt"), b"world").unwrap();
    std::fs::write(src.join("sub").join("c.txt"), b"!").unwrap();
    let mut extra = format!("[backup]\npaths = [{:?}]\n", src.display().to_string());
    if with_ui_password {
        let ui_pw = dir.path().join("ui-pw");
        std::fs::write(&ui_pw, format!("{UI_PASSWORD}\n")).unwrap();
        extra.push_str(&format!(
            "[serve]\npassword_file = {:?}\n",
            ui_pw.display().to_string()
        ));
    }
    let text = config_text(&repo, &password, dir.path(), &extra);
    let mut daemon = Daemon::new(Config::parse(&text).unwrap()).unwrap();
    let handle = daemon.handle();
    let metrics = daemon.metrics();
    let (shutdown, rx) = watch::channel(false);
    let daemon_rx = rx.clone();
    let daemon_task = tokio::spawn(async move { daemon.run(daemon_rx, |_| {}).await });

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let ui = ui_config(handle.config(), addr).unwrap();
    let state = ServeState {
        metrics,
        daemon: handle.clone(),
        ui: Arc::new(ui),
    };
    let server_task = tokio::spawn(serve_http(state, listener, rx));
    Server {
        addr,
        host: addr.to_string(),
        hostname: kist_app::client_id::hostname(),
        handle,
        shutdown,
        daemon_task,
        server_task,
        _dir: dir,
    }
}

#[test]
fn test_base64_encoder_matches_known_value() {
    // `printf 'u:ui secret' | base64`
    assert_eq!(base64_encode(b"u:ui secret"), "dTp1aSBzZWNyZXQ=");
    assert_eq!(base64_encode(b"anyone:wrong"), "YW55b25lOndyb25n");
}

/// 沒帶 auth → 401 + WWW-Authenticate: Basic。
#[tokio::test]
async fn ui_requires_basic_auth() {
    let s = start(true).await;
    let (status, headers, _) = request(s.addr, "GET", "/", &[("Host", &s.host)], "").await;
    assert!(status.contains("401"), "{status}");
    assert!(headers.contains("www-authenticate: basic"), "{headers}");
    // 密碼錯也是 401
    let (status, _, _) = request(
        s.addr,
        "GET",
        "/",
        &[("Host", &s.host), ("Authorization", &basic_auth("wrong"))],
        "",
    )
    .await;
    assert!(status.contains("401"), "{status}");
    // header 壞掉（不是合法 base64）也是 401
    let (status, _, _) = request(
        s.addr,
        "GET",
        "/",
        &[("Host", &s.host), ("Authorization", "Basic %%%not-base64")],
        "",
    )
    .await;
    assert!(status.contains("401"), "{status}");
    s.stop().await;
}

/// Host 不對 → 421（先於 auth）。
#[tokio::test]
async fn ui_rejects_unknown_host() {
    let s = start(true).await;
    let auth = basic_auth(UI_PASSWORD);
    let (status, _, body) = request(
        s.addr,
        "GET",
        "/",
        &[("Host", "evil.example:80"), ("Authorization", &auth)],
        "",
    )
    .await;
    assert!(status.contains("421"), "{status}");
    assert!(body.contains("allowed_hosts"), "{body}");
    // 沒有 Host 也是 421
    let (status, _, _) = request(s.addr, "GET", "/", &[("Authorization", &auth)], "").await;
    assert!(status.contains("421"), "{status}");
    // localhost 形式要過
    let (status, _, _) = request(
        s.addr,
        "GET",
        "/",
        &[
            ("Host", &format!("localhost:{}", s.addr.port())),
            ("Authorization", &auth),
        ],
        "",
    )
    .await;
    assert!(status.contains("200"), "{status}");
    s.stop().await;
}

/// 帶 auth → 完整頁面，有按鈕與 htmx 的輪詢。
#[tokio::test]
async fn ui_index_page() {
    let s = start(true).await;
    let auth = basic_auth(UI_PASSWORD);
    let (status, headers, body) = request(
        s.addr,
        "GET",
        "/",
        &[("Host", &s.host), ("Authorization", &auth)],
        "",
    )
    .await;
    assert!(status.contains("200"), "{status}");
    assert!(headers.contains("text/html"), "{headers}");
    assert!(body.contains("<title>kist</title>"), "{body}");
    assert!(body.contains("Run backup now"), "{body}");
    assert!(body.contains("hx-get=\"/ui/status\""), "{body}");
    assert!(body.contains("hx-get=\"/ui/snapshots\""), "{body}");
    assert!(body.contains("/static/htmx.min.js"), "{body}");
    assert!(body.contains("/static/style.css"), "{body}");
    assert!(body.contains(&s.hostname), "{body}");
    // 沒排程的 backup：manual only
    assert!(body.contains("manual only"), "{body}");
    s.stop().await;
}

/// 靜態檔：從 binary 裡的 rust-embed 拿，Content-Type 依副檔名。
#[tokio::test]
async fn ui_static_assets() {
    let s = start(true).await;
    let auth = basic_auth(UI_PASSWORD);
    let (status, headers, body) = request(
        s.addr,
        "GET",
        "/static/htmx.min.js",
        &[("Host", &s.host), ("Authorization", &auth)],
        "",
    )
    .await;
    assert!(status.contains("200"), "{status}");
    assert!(
        headers.contains("content-type: application/javascript"),
        "{headers}"
    );
    assert!(
        headers.contains("cache-control: public, max-age=86400"),
        "{headers}"
    );
    assert!(
        body.starts_with("var htmx"),
        "{}",
        &body[..40.min(body.len())]
    );

    let (status, headers, _) = request(
        s.addr,
        "GET",
        "/static/style.css",
        &[("Host", &s.host), ("Authorization", &auth)],
        "",
    )
    .await;
    assert!(status.contains("200"), "{status}");
    assert!(headers.contains("content-type: text/css"), "{headers}");

    let (status, _, _) = request(
        s.addr,
        "GET",
        "/static/nope.js",
        &[("Host", &s.host), ("Authorization", &auth)],
        "",
    )
    .await;
    assert!(status.contains("404"), "{status}");

    // 靜態檔一樣要 auth
    let (status, _, _) = request(
        s.addr,
        "GET",
        "/static/htmx.min.js",
        &[("Host", &s.host)],
        "",
    )
    .await;
    assert!(status.contains("401"), "{status}");
    s.stop().await;
}

/// POST 的 CSRF 規則：一定要 `HX-Request: true`，不能 `Sec-Fetch-Site: cross-site`。
#[tokio::test]
async fn ui_post_csrf_rules() {
    let s = start(true).await;
    let auth = basic_auth(UI_PASSWORD);
    let (status, _, body) = request(
        s.addr,
        "POST",
        "/ui/jobs/backup",
        &[("Host", &s.host), ("Authorization", &auth)],
        "",
    )
    .await;
    assert!(status.contains("403"), "{status}");
    assert!(body.contains("cross-site request rejected"), "{body}");

    let (status, _, body) = request(
        s.addr,
        "POST",
        "/ui/jobs/backup",
        &[
            ("Host", &s.host),
            ("Authorization", &auth),
            ("HX-Request", "true"),
            ("Sec-Fetch-Site", "cross-site"),
        ],
        "",
    )
    .await;
    assert!(status.contains("403"), "{status}");
    assert!(body.contains("cross-site request rejected"), "{body}");
    // 沒有觸發任何工作
    let st = s.handle.state();
    assert!(
        st.queued.is_empty() && st.running.is_none() && st.history.is_empty(),
        "{st:?}"
    );
    s.stop().await;
}

/// 按鈕真的會跑 backup：POST → 200 的 status partial；輪詢 /ui/status 直到成功；
/// 然後 /ui/snapshots 列出那個 snapshot（hostname、3 個檔案）。
#[tokio::test]
async fn ui_run_backup_and_list_snapshots() {
    tokio::time::timeout(std::time::Duration::from_secs(60), async {
        let s = start(true).await;
        let auth = basic_auth(UI_PASSWORD);
        let hdrs = [
            ("Host", s.host.as_str()),
            ("Authorization", auth.as_str()),
            ("HX-Request", "true"),
            ("Sec-Fetch-Site", "same-origin"),
        ];
        let (status, headers, body) = request(s.addr, "POST", "/ui/jobs/backup", &hdrs, "").await;
        assert!(status.contains("200"), "{status}\n{body}");
        assert!(headers.contains("text/html"), "{headers}");
        // partial 而不是整頁
        assert!(!body.contains("<html"), "{body}");

        let mut last = String::new();
        for _ in 0..300 {
            let (status, _, body) = request(s.addr, "GET", "/ui/status", &hdrs, "").await;
            assert!(status.contains("200"), "{status}");
            last = body;
            if last.contains("class=\"ok\"") && last.contains("success") {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        assert!(
            last.contains("class=\"ok\"") && last.contains("success"),
            "backup never showed as success:\n{last}"
        );
        assert!(last.contains("backup"), "{last}");
        // 跑完了：沒有 running、沒有 queued
        assert!(!last.contains("queued:"), "{last}");

        let (status, _, body) = request(s.addr, "GET", "/ui/snapshots", &hdrs, "").await;
        assert!(status.contains("200"), "{status}");
        assert!(body.contains(&s.hostname), "{body}");
        assert!(body.contains("<td>3</td>"), "{body}");
        assert!(!body.contains("No snapshots yet"), "{body}");
        s.stop().await;
    })
    .await
    .expect("test timed out");
}

/// /metrics 不受 Host / auth 規則影響（現有 scrape 設定不能壞）。
#[tokio::test]
async fn metrics_and_healthz_are_unguarded() {
    let s = start(true).await;
    let (status, headers, body) =
        request(s.addr, "GET", "/metrics", &[("Host", "whatever")], "").await;
    assert!(status.contains("200"), "{status}");
    assert!(
        headers.contains("application/openmetrics-text"),
        "{headers}"
    );
    assert!(body.contains("kist_backup_files"), "{body}");
    let (status, _, body) = request(s.addr, "GET", "/healthz", &[], "").await;
    assert!(status.contains("200"), "{status}");
    assert_eq!(body, "ok");
    s.stop().await;
}

/// 沒有 `[serve]`：頁面唯讀（不用登入）、按鈕換成說明文字、POST 一律 403。
#[tokio::test]
async fn ui_without_password_is_read_only() {
    let s = start(false).await;
    let (status, body) = {
        let (status, _, body) = request(s.addr, "GET", "/", &[("Host", &s.host)], "").await;
        (status, body)
    };
    assert!(status.contains("200"), "{status}");
    assert!(body.contains("Actions are disabled"), "{body}");
    assert!(!body.contains("Run backup now"), "{body}");

    let (status, _, body) = request(
        s.addr,
        "POST",
        "/ui/jobs/backup",
        &[("Host", &s.host), ("HX-Request", "true")],
        "",
    )
    .await;
    assert!(status.contains("403"), "{status}");
    assert!(body.contains("web UI actions are disabled"), "{body}");

    // 狀態 partial 也不用登入
    let (status, _, body) = request(s.addr, "GET", "/ui/status", &[("Host", &s.host)], "").await;
    assert!(status.contains("200"), "{status}");
    assert!(body.contains("Schedules"), "{body}");
    // snapshots：空 repo
    let (status, _, body) = request(s.addr, "GET", "/ui/snapshots", &[("Host", &s.host)], "").await;
    assert!(status.contains("200"), "{status}");
    assert!(body.contains("No snapshots yet"), "{body}");
    s.stop().await;
}

/// 第二次觸發（第一個還在排隊）→ 409 + notice；partial 還是要能顯示。
#[tokio::test]
async fn ui_second_trigger_is_queued_conflict() {
    tokio::time::timeout(std::time::Duration::from_secs(60), async {
        let s = start(true).await;
        let auth = basic_auth(UI_PASSWORD);
        let hdrs = [
            ("Host", s.host.as_str()),
            ("Authorization", auth.as_str()),
            ("HX-Request", "true"),
        ];
        // 連按兩次：第一次 200；第二次可能 200（第一件已被 daemon 接走）或 409（還在佇列）。
        // 連按到出現 409 為止（最多幾次），否則這條路徑就沒測到。
        let mut saw_409 = false;
        for _ in 0..20 {
            let (status, _, body) = request(s.addr, "POST", "/ui/jobs/backup", &hdrs, "").await;
            if status.contains("409") {
                assert!(body.contains("already queued"), "{body}");
                assert!(body.contains("Schedules"), "{body}");
                saw_409 = true;
                break;
            }
            assert!(status.contains("200"), "{status}\n{body}");
        }
        eprintln!("saw_409={saw_409}");
        s.stop().await;
    })
    .await
    .expect("test timed out");
}

/// UI 回應帶 Content-Security-Policy：maud 的輸出編碼是唯一防線，
/// CSP 把未來任何漏編碼的爆炸半徑壓到同源。
#[tokio::test]
async fn ui_responses_carry_content_security_policy() {
    let s = start(false).await;
    let (status, headers, _) = request(s.addr, "GET", "/", &[("Host", &s.host)], "").await;
    assert!(status.contains("200"), "{status}");
    assert!(
        headers.contains("content-security-policy:"),
        "UI 回應要帶 CSP header：{headers}"
    );
}
