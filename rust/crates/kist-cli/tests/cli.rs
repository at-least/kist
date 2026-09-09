//! 命令列的端到端測試：用真正的 binary 跑 init → backup → snapshots → restore → check。

use std::path::{Path, PathBuf};
use std::process::Command;

use rand::{RngExt, SeedableRng};

struct Env {
    dir: tempfile::TempDir,
}

impl Env {
    fn new() -> Self {
        Self {
            dir: tempfile::tempdir().unwrap(),
        }
    }

    fn repo(&self) -> PathBuf {
        self.dir.path().join("repo")
    }

    /// 執行 kist，回傳 (exit ok, stdout, stderr)。密碼與 client id 檔都走環境變數，不碰使用者家目錄。
    fn kist(&self, args: &[&str]) -> (bool, String, String) {
        let out = Command::new(env!("CARGO_BIN_EXE_kist"))
            .args(args)
            .env("KIST_REPO", self.repo())
            .env("KIST_PASSWORD", "cli test password")
            .env("KIST_CLIENT_ID_FILE", self.dir.path().join("client-id"))
            .env("KIST_CACHE_DIR", self.dir.path().join("cache"))
            .env_remove("RUST_LOG")
            .output()
            .unwrap();
        (
            out.status.success(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }

    fn ok(&self, args: &[&str]) -> String {
        let (ok, stdout, stderr) = self.kist(args);
        assert!(
            ok,
            "kist {args:?} failed\nstdout: {stdout}\nstderr: {stderr}"
        );
        stdout
    }

    fn fails(&self, args: &[&str]) -> String {
        let (ok, stdout, stderr) = self.kist(args);
        assert!(!ok, "kist {args:?} should have failed\nstdout: {stdout}");
        stderr
    }
}

fn make_source(root: &Path) {
    let mut rng = rand::rngs::StdRng::seed_from_u64(42);
    let mut big = vec![0u8; 3 * 1024 * 1024];
    rng.fill(&mut big[..]);
    std::fs::create_dir_all(root.join("sub")).unwrap();
    std::fs::write(root.join("a.txt"), b"hello\n").unwrap();
    std::fs::write(root.join("big.bin"), big).unwrap();
    std::fs::write(root.join("sub/b.txt"), b"world\n").unwrap();
}

#[test]
fn full_workflow() {
    let env = Env::new();
    let src = env.dir.path().join("src");
    make_source(&src);

    env.ok(&["init"]);
    assert!(env.repo().join("config").is_file());
    let err = env.fails(&["init"]);
    assert!(err.contains("already exists"), "{err}");
    let out = Command::new(env!("CARGO_BIN_EXE_kist"))
        .args(["init"])
        .env("KIST_REPO", env.repo())
        .env("KIST_PASSWORD", "cli test password")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1), "硬失敗是 exit 1");

    let out = env.ok(&["backup", src.to_str().unwrap()]);
    assert!(out.contains("snapshot"), "{out}");

    let out = env.ok(&["snapshots"]);
    assert_eq!(
        out.lines()
            .filter(|l| l.contains(src.to_str().unwrap()))
            .count(),
        1,
        "{out}"
    );

    let target = env.dir.path().join("out");
    env.ok(&["restore", "latest", target.to_str().unwrap()]);
    let restored = target.join(src.strip_prefix("/").unwrap_or(&src));
    assert_eq!(std::fs::read(restored.join("a.txt")).unwrap(), b"hello\n");
    assert_eq!(
        std::fs::read(restored.join("big.bin")).unwrap(),
        std::fs::read(src.join("big.bin")).unwrap()
    );
    assert_eq!(
        std::fs::read(restored.join("sub/b.txt")).unwrap(),
        b"world\n"
    );

    let out = env.ok(&["check"]);
    assert!(out.contains("no errors"), "{out}");
    let out = env.ok(&["check", "--read-data"]);
    assert!(out.contains("no errors"), "{out}");
}

#[test]
fn check_fails_on_corrupted_pack() {
    let env = Env::new();
    let src = env.dir.path().join("src");
    make_source(&src);
    env.ok(&["init"]);
    env.ok(&["backup", src.to_str().unwrap()]);

    let packs_dir = env.repo().join("packs");
    let pack = std::fs::read_dir(&packs_dir)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let mut bytes = std::fs::read(&pack).unwrap();
    bytes[200] ^= 0xff;
    std::fs::write(&pack, bytes).unwrap();

    let err = env.fails(&["check", "--read-data"]);
    assert!(
        err.contains(pack.file_name().unwrap().to_str().unwrap()),
        "{err}"
    );
}

#[test]
fn wrong_password_and_missing_repo_fail_cleanly() {
    let env = Env::new();
    let err = env.fails(&["snapshots"]);
    assert!(err.contains("not a kist repository"), "{err}");

    env.ok(&["init"]);
    let out = Command::new(env!("CARGO_BIN_EXE_kist"))
        .args(["snapshots"])
        .env("KIST_REPO", env.repo())
        .env("KIST_PASSWORD", "wrong")
        .env("KIST_CLIENT_ID_FILE", env.dir.path().join("client-id"))
        .env("KIST_CACHE_DIR", env.dir.path().join("cache"))
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("wrong password"));
}

#[test]
fn password_file_and_client_id_file_are_honoured() {
    let env = Env::new();
    let pw = env.dir.path().join("pw.txt");
    std::fs::write(&pw, "from file\n").unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_kist"))
        .args(["init", "--password-file", pw.to_str().unwrap()])
        .env("KIST_REPO", env.repo())
        .env_remove("KIST_PASSWORD")
        .env("KIST_CLIENT_ID_FILE", env.dir.path().join("client-id"))
        .env("KIST_CACHE_DIR", env.dir.path().join("cache"))
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let src = env.dir.path().join("src");
    make_source(&src);
    let out = Command::new(env!("CARGO_BIN_EXE_kist"))
        .args([
            "backup",
            src.to_str().unwrap(),
            "--password-file",
            pw.to_str().unwrap(),
        ])
        .env("KIST_REPO", env.repo())
        .env_remove("KIST_PASSWORD")
        .env("KIST_CLIENT_ID_FILE", env.dir.path().join("client-id"))
        .env("KIST_CACHE_DIR", env.dir.path().join("cache"))
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let id = std::fs::read_to_string(env.dir.path().join("client-id")).unwrap();
    assert_eq!(id.trim().len(), 32, "client id 是 16 bytes 的 hex");
    let snaps: Vec<_> = std::fs::read_dir(env.repo().join("snapshots"))
        .unwrap()
        .collect();
    assert_eq!(snaps.len(), 1);
    assert_eq!(
        snaps[0].as_ref().unwrap().file_name().to_str().unwrap(),
        id.trim()
    );
}

/// 讀不到的檔案：snapshot 照寫、有警告、結束碼非 0（restic 的行為）。
#[cfg(unix)]
#[test]
fn backup_with_unreadable_file_writes_snapshot_but_exits_nonzero() {
    use std::os::unix::fs::PermissionsExt;
    let env = Env::new();
    let src = env.dir.path().join("src");
    make_source(&src);
    let secret = src.join("secret");
    std::fs::write(&secret, b"x").unwrap();
    std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o000)).unwrap();
    env.ok(&["init"]);
    let out = Command::new(env!("CARGO_BIN_EXE_kist"))
        .args(["backup", src.to_str().unwrap()])
        .env("KIST_REPO", env.repo())
        .env("KIST_PASSWORD", "cli test password")
        .env("KIST_CLIENT_ID_FILE", env.dir.path().join("client-id"))
        .env("KIST_CACHE_DIR", env.dir.path().join("cache"))
        .output()
        .unwrap();
    std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o644)).unwrap();
    if nix_is_root() {
        return; // root 讀得到所有檔案，這個測試沒有意義
    }
    let (stdout, stderr) = (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    );
    // restic 慣例：有略過但 snapshot 已寫出 → 3；硬失敗才是 1
    assert_eq!(
        out.status.code(),
        Some(3),
        "stdout: {stdout}\nstderr: {stderr}"
    );
    assert!(stdout.contains("snapshot"), "snapshot 仍要寫出：{stdout}");
    assert!(stderr.contains("secret"), "要警告哪個檔案被略過：{stderr}");
    assert!(stderr.contains("1 item"), "要說明略過數：{stderr}");
    let snaps = std::fs::read_dir(env.repo().join("snapshots"))
        .unwrap()
        .count();
    assert_eq!(snaps, 1);
}

#[cfg(unix)]
fn nix_is_root() -> bool {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata("/proc/self")
        .map(|m| m.uid() == 0)
        .unwrap_or(false)
}

/// `--repo s3://…`：需要 MinIO（見 crates/kist-backend/tests/s3.rs），沒設環境變數就略過。
#[test]
fn s3_repo_url_works_end_to_end() {
    let (Some(endpoint), Some(bucket)) = (
        std::env::var("KIST_TEST_S3_ENDPOINT").ok(),
        std::env::var("KIST_TEST_S3_BUCKET").ok(),
    ) else {
        eprintln!("S3 env not set; skipped");
        return;
    };
    let env = Env::new();
    let src = env.dir.path().join("src");
    make_source(&src);
    let url = format!("s3://{bucket}/cli-{}", std::process::id());
    let run = |args: &[&str]| {
        let out = Command::new(env!("CARGO_BIN_EXE_kist"))
            .args(args)
            .env("KIST_REPO", &url)
            .env("KIST_PASSWORD", "cli test password")
            .env("KIST_CLIENT_ID_FILE", env.dir.path().join("client-id"))
            .env("KIST_CACHE_DIR", env.dir.path().join("cache"))
            .env("AWS_ENDPOINT", &endpoint)
            .env("AWS_ALLOW_HTTP", "true")
            .env("AWS_DEFAULT_REGION", "us-east-1")
            .output()
            .unwrap();
        (
            out.status.code(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    };
    let (code, _, stderr) = run(&["init"]);
    assert_eq!(code, Some(0), "{stderr}");
    assert!(
        stderr.contains("versioning"),
        "S3 repo 要提醒開 versioning：{stderr}"
    );
    let (code, stdout, stderr) = run(&["backup", src.to_str().unwrap()]);
    assert_eq!(code, Some(0), "{stdout}\n{stderr}");
    let target = env.dir.path().join("out");
    let (code, _, stderr) = run(&["restore", "latest", target.to_str().unwrap()]);
    assert_eq!(code, Some(0), "{stderr}");
    let restored = target.join(src.strip_prefix("/").unwrap_or(&src));
    assert_eq!(
        std::fs::read(restored.join("big.bin")).unwrap(),
        std::fs::read(src.join("big.bin")).unwrap()
    );
    let (code, stdout, stderr) = run(&["check", "--read-data"]);
    assert_eq!(code, Some(0), "{stdout}\n{stderr}");
}

#[test]
fn rebuild_index_command() {
    let env = Env::new();
    let src = env.dir.path().join("src");
    make_source(&src);
    env.ok(&["init"]);
    env.ok(&["backup", src.to_str().unwrap()]);
    for e in std::fs::read_dir(env.repo().join("indexes")).unwrap() {
        std::fs::remove_file(e.unwrap().path()).unwrap();
    }
    env.fails(&["check"]);
    let out = env.ok(&["rebuild-index"]);
    assert!(out.contains("pack"), "{out}");
    env.ok(&["check", "--read-data"]);
}

#[test]
fn forget_by_policy_and_by_id() {
    let env = Env::new();
    let src = env.dir.path().join("src");
    make_source(&src);
    env.ok(&["init"]);
    for _ in 0..3 {
        env.ok(&["backup", src.to_str().unwrap()]);
    }
    let err = env.fails(&["forget"]);
    assert!(err.contains("nothing to forget"), "{err}");

    let out = env.ok(&["forget", "--keep-last", "1", "--dry-run"]);
    assert!(out.contains("would remove 2 snapshot(s), kept 1"), "{out}");
    assert_eq!(
        env.ok(&["snapshots"]).lines().count(),
        4,
        "dry-run must not delete"
    );

    let out = env.ok(&["forget", "--keep-last", "1"]);
    assert!(out.contains("removed 2 snapshot(s), kept 1"), "{out}");
    assert!(out.contains("keep ") && out.contains("(last)"), "{out}");
    assert_eq!(env.ok(&["snapshots"]).lines().count(), 2);

    let out = env.ok(&["forget", "latest"]);
    assert!(out.contains("removed 1 snapshot(s)"), "{out}");
    assert!(env.ok(&["snapshots"]).contains("no snapshots"));

    let err = env.fails(&["forget", "--keep-within", "3x"]);
    assert!(err.contains("unknown unit"), "{err}");
    // 資料仍在，repo 一致（pack 只是沒人引用）
    env.ok(&["check", "--read-data"]);
}

#[test]
fn concurrent_backup_with_the_same_client_id_is_refused() {
    let env = Env::new();
    let src = env.dir.path().join("src");
    make_source(&src);
    env.ok(&["init"]);
    env.ok(&["backup", src.to_str().unwrap()]);
    // 模擬另一個還在跑的 backup：抓住同一個鎖
    let lock_path = env.dir.path().join("client-id.lock");
    let holder = std::fs::OpenOptions::new()
        .write(true)
        .open(&lock_path)
        .unwrap();
    holder.try_lock().unwrap();
    let err = env.fails(&["backup", src.to_str().unwrap()]);
    assert!(
        err.contains("another kist backup is already running"),
        "{err}"
    );
    drop(holder);
    env.ok(&["backup", src.to_str().unwrap()]);
}

#[test]
fn prune_marks_then_deletes() {
    fn packs(env: &Env) -> std::collections::BTreeSet<String> {
        std::fs::read_dir(env.repo().join("packs"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect()
    }
    let env = Env::new();
    let src = env.dir.path().join("src");
    make_source(&src);
    env.ok(&["init"]);
    env.ok(&["backup", src.to_str().unwrap()]);
    let original = packs(&env);
    std::fs::remove_file(src.join("big.bin")).unwrap();
    env.ok(&["backup", src.to_str().unwrap()]);
    // 拿掉第一個 snapshot：big.bin 的 chunk 沒人引用（跟小檔同一個 pack → 部分死亡 → repack）
    let out = env.ok(&["snapshots"]);
    let first_ts = out
        .lines()
        .nth(1)
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap()
        .to_owned();
    env.ok(&["forget", &first_ts]);

    // grace 0：立刻標記；repack 產生新 pack，舊 pack 還在（要等下一輪）。
    // 修改時間是整秒，標記不能跟物件同一秒：每次 prune 前等一下
    let settle = || std::thread::sleep(std::time::Duration::from_millis(1100));
    settle();
    let out = env.ok(&["prune", "--grace", "0s", "--clock-skew", "0s"]);
    assert!(out.contains("deleted 0 object"), "{out}");
    assert!(out.contains("repacked 1 pack"), "{out}");
    assert!(original.is_subset(&packs(&env)), "第一階段不能刪 pack");
    // 活躍 client 在標記後要有新 snapshot 才會刪
    settle();
    let out = env.ok(&["prune", "--grace", "0s", "--clock-skew", "0s"]);
    assert!(out.contains("deleted 0 object"), "{out}");
    assert!(!out.contains(", 0 held back"), "{out}");
    assert!(env.repo().join("gc").is_dir());
    env.ok(&["backup", src.to_str().unwrap()]);
    settle();
    let out = env.ok(&["prune", "--grace", "0s", "--clock-skew", "0s"]);
    assert!(!out.contains("deleted 0 object"), "{out}");
    assert!(
        original.is_disjoint(&packs(&env)),
        "被 repack 的舊 pack 應該刪掉了"
    );
    env.ok(&["check", "--read-data"]);

    // dry-run 不動任何東西
    let before: Vec<_> = std::fs::read_dir(env.repo().join("indexes"))
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    env.ok(&["prune", "--grace", "0s", "--clock-skew", "0s", "--dry-run"]);
    let after: Vec<_> = std::fs::read_dir(env.repo().join("indexes"))
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(before, after);

    // forget --prune 一次做完
    settle();
    let out = env.ok(&["forget", "--keep-last", "1", "--prune"]);
    assert!(
        out.contains("removed 1 snapshot(s)") && out.contains("live packs"),
        "{out}"
    );
    env.ok(&["check", "--read-data"]);
}

/// `--json`：每個有結果的命令都輸出可解析的 JSON；錯誤照舊走 stderr。
#[test]
fn json_flag_outputs_parseable_results() {
    use serde_json::Value;

    let env = Env::new();
    let src = env.dir.path().join("src");
    make_source(&src);

    env.ok(&["init"]);

    let v: Value =
        serde_json::from_str(&env.ok(&["backup", "--json", src.to_str().unwrap()])).unwrap();
    assert!(v["snapshot_key"]
        .as_str()
        .unwrap()
        .starts_with("snapshots/"));
    assert_eq!(v["stats"]["files"], 3);
    // v3：roots 取代 root——每個 root 帶 path 與 64 字元 hex 的 tree ID
    assert_eq!(v["roots"].as_array().unwrap().len(), 1);
    assert_eq!(
        v["roots"][0]["tree"].as_str().unwrap().len(),
        64,
        "roots[0].tree 是 64 字元 hex"
    );
    assert_eq!(v["roots"][0]["path"], src.to_str().unwrap());
    assert!(v["parent"].is_null());

    let v: Value = serde_json::from_str(&env.ok(&["snapshots", "--json"])).unwrap();
    assert_eq!(v.as_array().unwrap().len(), 1);
    assert_eq!(v[0]["client"].as_str().unwrap().len(), 32);
    assert_eq!(v[0]["paths"][0], src.to_str().unwrap());
    assert_eq!(v[0]["stats"]["files"], 3);
    assert!(v[0]["key"].as_str().unwrap().contains('/'));

    let target = env.dir.path().join("out");
    let v: Value =
        serde_json::from_str(&env.ok(&["restore", "--json", "latest", target.to_str().unwrap()]))
            .unwrap();
    assert_eq!(v["files"], 3);
    // v3：dirs 不含 roots 本身（root 是 path 不是 entry，規格 §9.1）——
    // v2 的合成根會把來源根也算一個，這裡只剩資料裡真的有的子目錄。
    assert_eq!(v["dirs"], 1);
    assert_eq!(v["errors"], Value::Array(vec![]));

    let v: Value = serde_json::from_str(&env.ok(&["check", "--json"])).unwrap();
    assert_eq!(v["errors"], Value::Array(vec![]));
    assert!(v["snapshots"].as_u64().unwrap() >= 1);

    let v: Value =
        serde_json::from_str(&env.ok(&["forget", "--json", "--keep-last", "1"])).unwrap();
    assert_eq!(v["dry_run"], false);
    assert_eq!(v["removed"], Value::Array(vec![]));
    assert_eq!(v["kept"].as_array().unwrap().len(), 1);
    assert!(!v["kept"][0]["reasons"].as_array().unwrap().is_empty());

    // forget --prune --json 必須是「一個」JSON 值，forget 與 prune 包在一起
    let v: Value =
        serde_json::from_str(&env.ok(&["forget", "--json", "--keep-last", "1", "--prune"]))
            .unwrap();
    assert_eq!(v["forget"]["dry_run"], false);
    assert_eq!(v["forget"]["kept"].as_array().unwrap().len(), 1);
    assert_eq!(v["prune"]["dry_run"], false);
    assert_eq!(v["prune"]["skipped"], Value::Array(vec![]));
    assert!(v.get("removed").is_none(), "top-level 要是空物件以外的組合");

    let v: Value = serde_json::from_str(&env.ok(&["prune", "--json"])).unwrap();
    assert_eq!(v["dry_run"], false);
    assert_eq!(v["skipped"], Value::Array(vec![]));

    let v: Value = serde_json::from_str(&env.ok(&["rebuild-index", "--json"])).unwrap();
    assert!(v["packs"].as_u64().unwrap() >= 1);

    // run --once --json：JobOutcome 陣列
    let pw = env.dir.path().join("pw.txt");
    std::fs::write(&pw, "cli test password\n").unwrap();
    let cfg = env.dir.path().join("kist.toml");
    std::fs::write(
        &cfg,
        format!(
            "repo = \"{}\"\npassword_file = \"{}\"\nclient_id_file = \"{}\"\ncache_dir = \"{}\"\n\n[backup]\npaths = [\"{}\"]\n",
            env.repo().display(),
            pw.display(),
            env.dir.path().join("client-id").display(),
            env.dir.path().join("cache").display(),
            src.display(),
        ),
    )
    .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_kist"))
        .args(["run", "--json", "--once", "--config", cfg.to_str().unwrap()])
        .env_remove("KIST_REPO")
        .env_remove("KIST_PASSWORD")
        .env_remove("KIST_CLIENT_ID_FILE")
        .env_remove("KIST_CACHE_DIR")
        .env_remove("RUST_LOG")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(out.status.success(), "stdout: {stdout}");
    let v: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(v.as_array().unwrap().len(), 1);
    assert_eq!(v[0]["job"], "backup");
    assert_eq!(v[0]["status"], "success");
    assert_eq!(v[0]["detail"]["stats"]["files"], 3);
}

/// `forget --json --prune` 在 prune 失敗時：snapshot 已經刪了，stdout 仍要有
/// forget 的結果（prune 為 null），結束碼 1、錯誤在 stderr。
#[cfg(unix)]
#[test]
fn forget_prune_json_still_reports_forget_when_prune_fails() {
    use serde_json::Value;
    use std::os::unix::fs::PermissionsExt;

    let env = Env::new();
    let src = env.dir.path().join("src");
    make_source(&src);
    env.ok(&["init"]);
    env.ok(&["backup", src.to_str().unwrap()]);
    // snapshot key 的時間是秒級，第二個 backup 要換一秒
    std::thread::sleep(std::time::Duration::from_millis(1100));
    env.ok(&["backup", src.to_str().unwrap()]);

    // trees/ 讀不到 → forget 照常刪 snapshot，接著的 prune 在規劃階段失敗
    let trees = env.repo().join("trees");
    std::fs::set_permissions(&trees, std::fs::Permissions::from_mode(0o000)).unwrap();
    let (ok, stdout, stderr) = env.kist(&["forget", "--json", "--keep-last", "1", "--prune"]);
    std::fs::set_permissions(&trees, std::fs::Permissions::from_mode(0o755)).unwrap();

    assert!(!ok, "prune should have failed\nstdout: {stdout}");
    assert!(
        stderr.contains("rror"),
        "error should go to stderr: {stderr}"
    );
    let v: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(v["forget"]["dry_run"], false);
    assert_eq!(v["forget"]["removed"].as_array().unwrap().len(), 1);
    assert!(v["prune"].is_null(), "{stdout}");
    // snapshot 真的刪了
    let v: Value = serde_json::from_str(&env.ok(&["snapshots", "--json"])).unwrap();
    assert_eq!(v.as_array().unwrap().len(), 1);
}

/// `kist serve`：起 daemon + HTTP，--http 127.0.0.1:0 時從 stderr 取得實際位址，
/// /metrics 能抓到 OpenMetrics 文字。
#[test]
fn serve_exposes_metrics() {
    use std::io::{BufRead, Read, Write};
    use std::net::SocketAddr;
    use std::process::{Command, Stdio};

    let env = Env::new();
    let src = env.dir.path().join("src");
    make_source(&src);
    env.ok(&["init"]);

    let pw = env.dir.path().join("pw.txt");
    std::fs::write(&pw, "cli test password\n").unwrap();
    let cfg = env.dir.path().join("kist.toml");
    // 早上三點的排程：測試期間不會真的跑到 backup
    std::fs::write(
        &cfg,
        format!(
            "repo = \"{}\"\npassword_file = \"{}\"\nclient_id_file = \"{}\"\ncache_dir = \"{}\"\n\n[backup]\npaths = [\"{}\"]\nschedule = \"0 3 * * *\"\n",
            env.repo().display(),
            pw.display(),
            env.dir.path().join("client-id").display(),
            env.dir.path().join("cache").display(),
            src.display(),
        ),
    )
    .unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_kist"))
        .args([
            "serve",
            "--config",
            cfg.to_str().unwrap(),
            "--http",
            "127.0.0.1:0",
        ])
        .env_remove("KIST_REPO")
        .env_remove("KIST_PASSWORD")
        .env_remove("KIST_CLIENT_ID_FILE")
        .env_remove("KIST_CACHE_DIR")
        .env_remove("RUST_LOG")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stderr = child.stderr.take().unwrap();
    let mut addr = None;
    let mut collected = String::new();
    let mut reader = std::io::BufReader::new(stderr);
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => break, // process 結束
            Ok(_) => {
                if let Some(rest) = line.trim().strip_prefix("listening on http://") {
                    addr = Some(rest.split(' ').next().unwrap().to_owned());
                    break;
                }
                collected.push_str(&line);
            }
        }
    }
    let addr: SocketAddr = addr
        .unwrap_or_else(|| panic!("no listening line; stderr:\n{collected}"))
        .parse()
        .unwrap();

    let mut s = std::net::TcpStream::connect(addr).unwrap();
    s.write_all(b"GET /metrics HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n")
        .unwrap();
    let mut resp = String::new();
    s.read_to_string(&mut resp).unwrap();
    assert!(resp.contains("HTTP/1.1 200 OK"), "{resp}");
    assert!(resp.contains("kist_backup_files 0"), "{resp}");

    child.kill().unwrap();
    child.wait().unwrap();
}

/// FUSE 掛載：起 `kist mount` 子程序，經 kernel 讀 snapshot，SIGTERM 乾淨卸載。
/// 需要 /dev/fuse + fusermount3 且 `KIST_TEST_FUSE=1`（tests/fuse-setup.sh）。
#[cfg(unix)]
#[test]
fn mount_round_trip() {
    if std::env::var("KIST_TEST_FUSE").ok().as_deref() != Some("1") {
        eprintln!("FUSE 掛載測試跳過（tests/fuse-setup.sh 未跑）");
        return;
    }
    let env = Env::new();
    let src = env.dir.path().join("src");
    make_source(&src);

    env.ok(&["init"]);
    env.ok(&["backup", src.to_str().unwrap()]);

    let mnt = env.dir.path().join("mnt");
    std::fs::create_dir_all(&mnt).unwrap();

    let child = Command::new(env!("CARGO_BIN_EXE_kist"))
        .args(["mount", mnt.to_str().unwrap()])
        .env("KIST_REPO", env.repo())
        .env("KIST_PASSWORD", "cli test password")
        .env("KIST_CLIENT_ID_FILE", env.dir.path().join("client-id"))
        .env("KIST_CACHE_DIR", env.dir.path().join("cache"))
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();

    // 等掛載生效：根目錄（FUSE）列出 client 目錄
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    let client_dir = loop {
        match std::fs::read_dir(&mnt) {
            Ok(mut rd) => {
                if let Some(entry) = rd.next() {
                    break entry.unwrap().path();
                }
            }
            Err(e) => {
                if std::time::Instant::now() > deadline {
                    panic!("client directory never appeared: {e}");
                }
            }
        }
        if std::time::Instant::now() > deadline {
            panic!("client directory never appeared (timeout)");
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    };

    // client → timestamp → tmp/<tmpname>/src
    let ts_dir = std::fs::read_dir(&client_dir)
        .unwrap()
        .next()
        .expect("至少一個 timestamp")
        .unwrap()
        .path();
    // 備份路徑是 <tempdir>/src：虛擬層級 = tmp/<tempdir 名>/src
    let tmpname = env.dir.path().file_name().unwrap();
    let snap = ts_dir.join("tmp").join(tmpname).join("src");

    // 小檔
    assert_eq!(
        std::fs::read_to_string(snap.join("a.txt")).unwrap(),
        "hello\n"
    );
    // 大檔（跨多 chunk）全檔讀
    let want = std::fs::read(src.join("big.bin")).unwrap();
    let got = std::fs::read(snap.join("big.bin")).unwrap();
    assert_eq!(got.len(), want.len());
    assert!(got == want, "全檔讀必須位元組一致");
    // 偏移讀
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(snap.join("big.bin")).unwrap();
    f.seek(SeekFrom::Start(65536)).unwrap();
    let mut buf = vec![0u8; 1000];
    f.read_exact(&mut buf).unwrap();
    drop(f); // 還開著檔案時 fusermount3 -u 會 EBUSY
    assert_eq!(buf, want[65536..66536]);
    // 子目錄
    assert_eq!(
        std::fs::read_to_string(snap.join("sub/b.txt")).unwrap(),
        "world\n"
    );
    // 唯讀：寫入必敗
    assert!(std::fs::write(snap.join("nope"), b"x").is_err());

    // SIGTERM → kist 自己 fusermount 卸載、exit 0（與手動 Ctrl-C 同一路徑）
    let st = Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .expect("kill");
    assert!(st.success(), "kill -TERM");
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "kist mount should exit 0 after SIGTERM; stderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    // 卸載後掛載點回到普通空目錄
    assert!(std::fs::read_dir(&mnt).unwrap().next().is_none());
}
