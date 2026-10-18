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
        let (code, stdout, stderr) = self.kist_code(args);
        (code == Some(0), stdout, stderr)
    }

    /// 同 [`Env::kist`]，但回傳結束碼本身（區分 1 的失敗與 3 的不完整）。
    fn kist_code(&self, args: &[&str]) -> (Option<i32>, String, String) {
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
            out.status.code(),
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

    /// 同 [`Env::kist_code`]，標準輸入是 `input`（用到它的測試只在 unix 跑）。
    #[cfg(unix)]
    fn kist_stdin(&self, args: &[&str], input: &[u8]) -> (Option<i32>, String, String) {
        use std::io::Write;
        let mut child = Command::new(env!("CARGO_BIN_EXE_kist"))
            .args(args)
            .env("KIST_REPO", self.repo())
            .env("KIST_PASSWORD", "cli test password")
            .env("KIST_CLIENT_ID_FILE", self.dir.path().join("client-id"))
            .env("KIST_CACHE_DIR", self.dir.path().join("cache"))
            .env_remove("RUST_LOG")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(input).unwrap();
        let out = child.wait_with_output().unwrap();
        (
            out.status.code(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }
}

/// `dir` 底下所有的檔案（遞迴），排好。
fn files_under(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                stack.push(p);
            } else {
                out.push(p);
            }
        }
    }
    out.sort();
    out
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

    let pack = files_under(&env.repo().join("packs"))[0].clone();
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

/// 密碼檔的規則 CLI 與 `kist run` 共用一份（ADR 019 A11）：取第一行、去掉結尾的 `\r`。
/// 檔案內容是「密碼 + `\r`」、後面沒有 `\n` 時，daemon 讀成不含 `\r` 的密碼；CLI 以前
/// 保留 `\r`，於是 daemon 每晚備份正常，拿同一個檔跑 CLI 卻被告知密碼錯。
#[test]
fn password_file_trailing_cr_is_read_like_the_daemon() {
    let env = Env::new();
    env.ok(&["init"]); // repo 密碼是 KIST_PASSWORD 的 "cli test password"
    let src = env.dir.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("a.txt"), b"hello\n").unwrap();
    let pw = env.dir.path().join("pw.txt");
    std::fs::write(&pw, "cli test password\r").unwrap();

    // daemon 這條路：kist run --once 讀同一個密碼檔，能開 repo、寫出 snapshot。
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
        .args(["run", "--once", "--config", cfg.to_str().unwrap()])
        .env_remove("KIST_REPO")
        .env_remove("KIST_PASSWORD")
        .env_remove("KIST_CLIENT_ID_FILE")
        .env_remove("KIST_CACHE_DIR")
        .env_remove("RUST_LOG")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "run --once: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // CLI 這條路：同一個檔交給 --password-file，必須讀出同一個密碼。
    let out = env.ok(&["snapshots", "--password-file", pw.to_str().unwrap()]);
    assert_eq!(
        out.lines()
            .filter(|l| l.contains(src.to_str().unwrap()))
            .count(),
        1,
        "{out}"
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

/// ADR 019 A30：還原目標處擋著使用者自己的 symlink：那一項失敗、結束碼 3（與
/// 以前相同），訊息點名擋路的本機路徑，不說 repo 物件損壞。
#[cfg(unix)]
#[test]
fn restore_with_a_symlink_in_the_way_exits_3_without_blaming_the_repo() {
    let env = Env::new();
    let src = env.dir.path().join("src");
    make_source(&src);
    env.ok(&["init"]);
    env.ok(&["backup", src.to_str().unwrap()]);
    let target = env.dir.path().join("out");
    let restored = target.join(src.strip_prefix("/").unwrap_or(&src));
    std::fs::create_dir_all(&restored).unwrap();
    let link = restored.join("a.txt");
    std::os::unix::fs::symlink(env.dir.path().join("elsewhere"), &link).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_kist"))
        .args(["restore", "latest", target.to_str().unwrap()])
        .env("KIST_REPO", env.repo())
        .env("KIST_PASSWORD", "cli test password")
        .env("KIST_CLIENT_ID_FILE", env.dir.path().join("client-id"))
        .env("KIST_CACHE_DIR", env.dir.path().join("cache"))
        .env_remove("RUST_LOG")
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert_eq!(out.status.code(), Some(3), "stderr: {stderr}");
    assert!(
        stderr.contains(&link.display().to_string()),
        "要點名擋路的路徑：{stderr}"
    );
    assert!(!stderr.contains("corrupt"), "本機擋路被報成損壞：{stderr}");
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
    // v4 不覆寫 config，不再提醒開 versioning；提醒的是留一份 config 與密碼（所有後端一樣）
    assert!(
        stderr.contains("keep a copy of the `config` object"),
        "init 要提醒留一份 config：{stderr}"
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

/// `rebuild`：最新一代的 map 分片遺失 → check 回報、rebuild 從讀得全的一代重建，之後 check 乾淨。
#[test]
fn rebuild_command() {
    let env = Env::new();
    let src = env.dir.path().join("src");
    make_source(&src);
    env.ok(&["init"]);
    env.ok(&["backup", src.to_str().unwrap()]);
    env.ok(&["prune"]);
    let out = env.ok(&["rebuild"]);
    assert!(out.contains("intact"), "{out}");
    for f in files_under(&env.repo().join("maint/map")) {
        std::fs::remove_file(f).unwrap();
    }
    env.fails(&["check"]);
    let out = env.ok(&["rebuild"]);
    assert!(out.contains("rebuilt the map as generation 2"), "{out}");
    env.ok(&["check", "--read-data"]);
    let out = env.ok(&["settings"]);
    assert!(out.contains("hold:"), "{out}");
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

    // 每個 client 與 paths 最新的一份是下一次 backup 的 parent：要明說 --retire
    let err = env.fails(&["forget", "latest"]);
    assert!(err.contains("--retire"), "{err}");
    let out = env.ok(&["forget", "latest", "--retire"]);
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

/// prune 走真的時間：刪除要等兩個 grace（至少 2 小時），這裡只驗每一輪寫出新的一代、收錄新的 pack、什麼都
/// 不刪、之後 check 乾淨；刪除的路徑在 kist-core 以虛擬時鐘測（tests/v4_prune.rs）。v3 的選項不再接受。
#[test]
fn prune_writes_a_generation_per_run() {
    let env = Env::new();
    let src = env.dir.path().join("src");
    make_source(&src);
    env.ok(&["init"]);
    env.ok(&["backup", src.to_str().unwrap()]);
    std::fs::remove_file(src.join("big.bin")).unwrap();
    env.ok(&["backup", src.to_str().unwrap()]);
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
    let out = env.ok(&["prune"]);
    assert!(out.contains("generation 1 written (from 0)"), "{out}");
    assert!(out.contains("new pack(s) recorded"), "{out}");
    assert!(out.contains("deleted 0 pack(s)"), "{out}");
    let out = env.ok(&["prune", "--repack-below", "0"]);
    assert!(out.contains("generation 2 written (from 1)"), "{out}");
    env.ok(&["check", "--read-data"]);
    for old in [["prune", "--grace", "0s"], ["prune", "--dry-run", ""]] {
        let args: Vec<&str> = old.iter().copied().filter(|a| !a.is_empty()).collect();
        let err = env.fails(&args);
        assert!(err.contains("unexpected argument"), "{err}");
    }
    // forget --prune 一次做完
    let out = env.ok(&["forget", "--keep-last", "1", "--prune"]);
    assert!(
        out.contains("kept 1") && out.contains("generation 3 written"),
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
    assert!(v["snapshot"].as_str().unwrap().starts_with("snapshots/"));
    assert_eq!(v["stats"]["files"], 3);
    assert_eq!(v["roots"].as_array().unwrap().len(), 1);
    assert_eq!(v["roots"][0]["path"], src.to_str().unwrap());
    assert_eq!(v["roots"][0]["type"], "dir");
    assert!(v["parent"].is_null());
    assert_eq!(v["generation"], 0);

    let v: Value = serde_json::from_str(&env.ok(&["snapshots", "--json"])).unwrap();
    assert_eq!(v.as_array().unwrap().len(), 1);
    assert_eq!(v[0]["client"].as_str().unwrap().len(), 32);
    assert_eq!(v[0]["paths"][0], src.to_str().unwrap());
    assert_eq!(v[0]["stats"]["files"], 3);
    assert!(v[0]["key"].as_str().unwrap().contains('/'));
    assert_eq!(v[0]["roots"][0]["type"], "dir");

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
    assert_eq!(v["prune"]["generation"], 1);
    assert_eq!(v["prune"]["undeleted"], Value::Array(vec![]));
    assert!(v.get("removed").is_none(), "top-level 要是空物件以外的組合");

    let v: Value = serde_json::from_str(&env.ok(&["prune", "--json"])).unwrap();
    assert_eq!(v["generation"], 2);
    assert_eq!(v["undeleted"], Value::Array(vec![]));

    let v: Value = serde_json::from_str(&env.ok(&["rebuild", "--json"])).unwrap();
    assert!(v["generation"].is_null(), "the map is intact: {v}");
    assert_eq!(v["lost"], 2);

    let v: Value = serde_json::from_str(&env.ok(&["settings", "--json"])).unwrap();
    assert_eq!(v["generation"], 2);
    assert_eq!(v["settings"]["grace"], 72 * 3600);
    let v: Value =
        serde_json::from_str(&env.ok(&["settings", "--json", "--grace", "100h"])).unwrap();
    assert_eq!(v["generation"], 3);
    assert_eq!(v["settings"]["grace"], 100 * 3600);

    let v: Value = serde_json::from_str(&env.ok(&["key", "list", "--json"])).unwrap();
    assert_eq!(v.as_array().unwrap().len(), 1);
    assert_eq!(v[0]["current"], true);

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
    env.ok(&["backup", src.to_str().unwrap()]);

    // packs/ 讀不到 → forget 照常刪 snapshot（它只讀 snapshot 與 roots），接著的 prune 列 packs/ 時失敗
    if nix_is_root() {
        return; // root 讀得到 0o000 的目錄
    }
    let packs = env.repo().join("packs");
    std::fs::set_permissions(&packs, std::fs::Permissions::from_mode(0o000)).unwrap();
    let (ok, stdout, stderr) = env.kist(&["forget", "--json", "--keep-last", "1", "--prune"]);
    std::fs::set_permissions(&packs, std::fs::Permissions::from_mode(0o755)).unwrap();

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

/// `check --json --repair` 的 stdout 必須整份是 JSON（README：「結果以 JSON 印到
/// stdout」）：修復成功的人類可讀行（`repaired pack …`）不能混在 JSON 前面，
/// 否則 `jq`／監控腳本直接解析失敗。
#[test]
fn check_json_repair_output_is_pure_json() {
    let env = Env::new();
    let src = env.dir.path().join("src");
    make_source(&src);
    env.ok(&["init"]);
    env.ok(&["backup", "--parity", "2", src.to_str().unwrap()]);

    // 本機的 replicas=1：mpack 有 .r1。拿掉一份 → check --repair 從主體建立（只在不存在時建立）
    let r1 = files_under(&env.repo().join("mpacks"))
        .into_iter()
        .find(|p| p.to_str().unwrap().ends_with(".r1"))
        .expect("an mpack .r1");
    std::fs::remove_file(&r1).unwrap();
    let (ok, stdout, stderr) = env.kist(&["check", "--json", "--repair"]);
    assert!(
        ok,
        "check --repair 應該成功\nstdout: {stdout}\nstderr: {stderr}"
    );
    let report: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("stdout 不是純 JSON（{e}）：\n{stdout}"));
    let name = r1.file_name().unwrap().to_str().unwrap();
    assert!(
        report["repaired"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v.as_str().is_some_and(|s| s.contains(name))),
        "{report}"
    );
    assert!(r1.is_file(), "recreated");

    // 弄壞一個資料 pack：v4 不覆寫，check 回報錯誤，parity 修得回就列在 repairable
    let pack = files_under(&env.repo().join("packs"))[0].clone();
    let mut data = std::fs::read(&pack).unwrap();
    let mid = data.len() / 2;
    data[mid] ^= 0x01;
    std::fs::write(&pack, &data).unwrap();
    let (ok, stdout, _) = env.kist(&["check", "--json", "--read-data"]);
    assert!(!ok);
    let report: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let id = pack.file_name().unwrap().to_str().unwrap();
    assert!(
        report["repairable"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v.as_str().is_some_and(|s| s.contains(id))),
        "{report}"
    );
}

/// 審查修正：清單與樣式檔的讀法。文字清單與樣式檔開頭的 UTF-8 BOM 去掉、CRLF 的 `\r`
/// 去掉（verbatim 也是）；raw 的空名字與少了結尾 NUL 是錯；`-`（標準輸入）只能給一次，樣式檔
/// 也可以是 `-`；一條樣式都沒有的 include 樣式檔是錯（不然 include 是空的、整份還原）。
#[cfg(unix)]
#[test]
fn list_and_pattern_files_are_read_strictly() {
    use serde_json::Value;
    use std::os::unix::ffi::OsStrExt;

    let env = Env::new();
    let one = env.dir.path().join("data/one");
    let two = env.dir.path().join("data/two");
    for dir in [&one, &two] {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join("f"), b"f").unwrap();
    }
    std::fs::write(one.join("x.tmp"), b"tmp").unwrap();
    let (one_b, two_b) = (one.as_os_str().as_bytes(), two.as_os_str().as_bytes());
    let file = |name: &str, bytes: &[u8]| {
        let path = env.dir.path().join(name);
        std::fs::write(&path, bytes).unwrap();
        path.to_str().unwrap().to_owned()
    };
    let snapshots = || {
        let v: Value = serde_json::from_str(&env.ok(&["snapshots", "--json"])).unwrap();
        v.as_array().unwrap().len()
    };
    env.ok(&["init"]);

    // BOM＋CRLF：--files-from 與 --files-from-verbatim 都讀到這兩個路徑。
    let list = file(
        "bom-crlf.txt",
        &[b"\xef\xbb\xbf", one_b, b"\r\n", two_b, b"\r\n"].concat(),
    );
    for flag in ["--files-from", "--files-from-verbatim"] {
        let v: Value = serde_json::from_str(&env.ok(&["backup", "--json", flag, &list])).unwrap();
        assert_eq!(v["roots"].as_array().unwrap().len(), 2, "{flag}: {v}");
    }

    // 樣式檔：BOM 之後的第一行就是樣式；也可以從標準輸入讀。
    let patterns = file("exclude.txt", b"\xef\xbb\xbf*.tmp\r\n");
    let one_s = one.to_str().unwrap();
    let v: Value =
        serde_json::from_str(&env.ok(&["backup", "--json", "--exclude-file", &patterns, one_s]))
            .unwrap();
    assert_eq!(v["report"]["excluded"], 1, "{v}");
    let (code, out, err) = env.kist_stdin(
        &["backup", "--json", "--exclude-file", "-", one_s],
        b"*.tmp\n",
    );
    assert_eq!(code, Some(0), "{err}");
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["report"]["excluded"], 1, "{v}");

    // raw：空名字、少了結尾的 NUL 都是錯；`-` 給兩次也是。都不留 snapshot。
    let before = snapshots();
    let raw_empty = file("raw-empty", &[one_b, b"\0\0", two_b, b"\0"].concat());
    let raw_open = file("raw-open", &[one_b, b"\0", two_b].concat());
    for (args, needle) in [
        (vec!["backup", "--files-from-raw", &raw_empty], "empty name"),
        (vec!["backup", "--files-from-raw", &raw_open], "NUL"),
        (
            vec!["backup", "--files-from", "-", "--files-from-verbatim", "-"],
            "standard input",
        ),
        (
            vec!["backup", "--files-from", "-", "--exclude-file", "-"],
            "standard input",
        ),
    ] {
        let (code, _, err) = env.kist_stdin(&args, &[one_b, b"\n"].concat());
        assert_eq!(code, Some(1), "{args:?}: {err}");
        assert!(err.contains(needle), "{args:?}: {err}");
    }
    assert_eq!(snapshots(), before);

    // 一條樣式都沒有的 include 樣式檔：結束碼 1，目標不建。exclude 的空樣式檔照常。
    let empty = file("empty.txt", b"");
    let comments = file("comments.txt", b"# none\n\n   \n");
    for (flag, list) in [
        ("--include-file", &empty),
        ("--include-file", &comments),
        ("--iinclude-file", &empty),
    ] {
        let target = env.dir.path().join("never");
        let (code, _, err) =
            env.kist_code(&["restore", "latest", target.to_str().unwrap(), flag, list]);
        assert_eq!(code, Some(1), "{flag} {list}: {err}");
        assert!(err.contains("no patterns"), "{err}");
        assert!(!target.exists());
    }
    let target = env.dir.path().join("out");
    env.ok(&[
        "restore",
        "latest",
        target.to_str().unwrap(),
        "--exclude-file",
        &empty,
    ]);
}

/// `backup --exclude`／`--exclude-file`（ADR 020）：樣式檔一行一條，空行與 `#` 開頭的行略過、
/// 前後空白去掉；寫錯的樣式在碰 repo 之前就以 exit 1 結束，並點名出處。
#[test]
fn backup_excludes_from_flags_and_files() {
    use serde_json::Value;

    let env = Env::new();
    let src = env.dir.path().join("src");
    make_source(&src);
    std::fs::write(src.join("junk.tmp"), b"junk").unwrap();
    std::fs::create_dir_all(src.join("sub/cache")).unwrap();
    std::fs::write(src.join("sub/cache/c"), b"cache").unwrap();
    let list = env.dir.path().join("exclude.txt");
    std::fs::write(&list, "# comment\n\n  cache  \r\n").unwrap();

    env.ok(&["init"]);
    let v: Value = serde_json::from_str(&env.ok(&[
        "backup",
        "--json",
        "--exclude",
        "*.tmp",
        "--exclude-file",
        list.to_str().unwrap(),
        src.to_str().unwrap(),
    ]))
    .unwrap();
    assert_eq!(v["report"]["excluded"], 2, "{v}");
    assert_eq!(v["stats"]["files"], 3, "{v}");

    let out = env.ok(&[
        "backup",
        "--exclude",
        "*.tmp",
        "--exclude-file",
        list.to_str().unwrap(),
        src.to_str().unwrap(),
    ]);
    assert!(out.contains("excluded: 2"), "{out}");

    let target = env.dir.path().join("out");
    env.ok(&["restore", "latest", target.to_str().unwrap()]);
    let restored = target.join(src.strip_prefix("/").unwrap_or(&src));
    assert!(restored.join("a.txt").is_file());
    assert!(restored.join("sub/b.txt").is_file());
    assert!(!restored.join("junk.tmp").exists());
    assert!(!restored.join("sub/cache").exists());

    let (code, _, err) = env.kist_code(&["backup", "--exclude", "[oops", src.to_str().unwrap()]);
    assert_eq!(code, Some(1), "寫錯的樣式是硬失敗：{err}");
    assert!(err.contains("invalid pattern \"[oops\""), "{err}");
    std::fs::write(&list, "ok\n[unclosed\n").unwrap();
    let (code, _, err) = env.kist_code(&[
        "backup",
        "--exclude-file",
        list.to_str().unwrap(),
        src.to_str().unwrap(),
    ]);
    assert_eq!(code, Some(1), "{err}");
    assert!(
        err.contains(&format!("{}:2", list.display())),
        "要點名檔案與行號：{err}"
    );
    assert!(err.contains("unclosed"), "{err}");
    let snapshots: Value = serde_json::from_str(&env.ok(&["snapshots", "--json"])).unwrap();
    assert_eq!(
        snapshots.as_array().unwrap().len(),
        2,
        "寫錯樣式的 backup 不留 snapshot"
    );
}

/// `restore --include`／`--exclude`（ADR 020）：只還原選到的；沒比對到任何東西的 include
/// 印警告、結束碼 3（選到的照樣還原）；寫錯的樣式在碰 repo 之前以結束碼 1 結束。
#[test]
fn restore_include_and_exclude() {
    use serde_json::Value;

    let env = Env::new();
    let src = env.dir.path().join("src");
    make_source(&src);
    env.ok(&["init"]);
    env.ok(&["backup", src.to_str().unwrap()]);
    let abs = std::fs::canonicalize(&src).unwrap();
    let rel = abs.strip_prefix("/").unwrap();

    // exclude 優先：sub 被選到，但裡面的 b.txt 被排除——sub 照樣建成空目錄。
    let target = env.dir.path().join("out");
    let sub = format!("{}/sub", abs.display());
    let out = env.ok(&[
        "restore",
        "latest",
        target.to_str().unwrap(),
        "--include",
        &sub,
        "--include",
        "*.bin",
        "--exclude",
        "*.txt",
    ]);
    assert!(out.contains("1 files, 1 dirs"), "{out}");
    assert!(target.join(rel).join("big.bin").is_file());
    assert!(target.join(rel).join("sub").is_dir());
    assert!(!target.join(rel).join("sub/b.txt").exists());
    assert!(!target.join(rel).join("a.txt").exists());

    let target = env.dir.path().join("out2");
    let (code, stdout, stderr) = env.kist_code(&[
        "restore",
        "--json",
        "latest",
        target.to_str().unwrap(),
        "--include",
        "*.txt",
        "--include",
        "*.docx",
    ]);
    assert_eq!(code, Some(3), "沒比對到的 include 是不完整：{stderr}");
    let v: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(v["files"], 2, "{v}");
    assert_eq!(v["unmatched_includes"], serde_json::json!(["*.docx"]));
    assert!(
        stderr.contains("\"*.docx\" selected nothing"),
        "要點名沒選到東西的樣式：{stderr}"
    );
    assert!(target.join(rel).join("a.txt").is_file());

    let (code, _, stderr) = env.kist_code(&[
        "restore",
        "latest",
        env.dir.path().join("out3").to_str().unwrap(),
        "--exclude",
        "a**",
    ]);
    assert_eq!(code, Some(1), "{stderr}");
    assert!(stderr.contains("--exclude"), "{stderr}");
    assert!(
        !env.dir.path().join("out3").exists(),
        "寫錯樣式時什麼都不建"
    );
}

/// 審查修正：還原有錯誤時，失敗的目錄底下沒有走到，沒選到東西的 include 不能說成「snapshot
/// 裡沒有」。訊息也不點名 `--include`（樣式可能來自樣式檔或 `--iinclude`）。
#[test]
fn unmatched_includes_after_errors_are_not_claimed_absent() {
    let env = Env::new();
    let src = env.dir.path().join("src");
    std::fs::create_dir_all(src.join("docs")).unwrap();
    std::fs::write(src.join("docs/a.pdf"), b"a").unwrap();
    std::fs::write(src.join("docs/b.txt"), b"b").unwrap();
    env.ok(&["init"]);
    env.ok(&["backup", src.to_str().unwrap()]);
    let abs = std::fs::canonicalize(&src).unwrap();
    let rel = abs.strip_prefix("/").unwrap();
    // docs 要建的地方擋著一個一般檔：a.pdf 之後 docs 整個放棄，b.txt 沒走到。
    let target = env.dir.path().join("out");
    std::fs::create_dir_all(target.join(rel)).unwrap();
    std::fs::write(target.join(rel).join("docs"), b"in the way").unwrap();
    let (code, _, stderr) = env.kist_code(&[
        "restore",
        "latest",
        target.to_str().unwrap(),
        "--include",
        "*.pdf",
        "--include",
        "*.txt",
    ]);
    assert_eq!(code, Some(3), "{stderr}");
    assert!(
        stderr.contains(
            "include pattern \"*.txt\" selected nothing: no match among the items reached"
        ),
        "{stderr}"
    );
    assert!(!stderr.contains("no match in this snapshot"), "{stderr}");
    assert!(!stderr.contains("--include"), "{stderr}");
}

/// `--exclude-caches`、`--exclude-if-present`、`--exclude-larger-than`（ADR 021）端到端。
#[test]
fn backup_excludes_by_marker_and_size() {
    use serde_json::Value;

    let env = Env::new();
    let src = env.dir.path().join("src");
    make_source(&src);
    std::fs::create_dir_all(src.join("cache")).unwrap();
    std::fs::write(
        src.join("cache/CACHEDIR.TAG"),
        "Signature: 8a477f597d28d172789f06886806bc55\n",
    )
    .unwrap();
    std::fs::write(src.join("cache/blob"), b"cached").unwrap();
    std::fs::create_dir_all(src.join("scratch")).unwrap();
    std::fs::write(src.join("scratch/.nobackup"), b"").unwrap();
    std::fs::write(src.join("scratch/tmp"), b"tmp").unwrap();
    env.ok(&["init"]);
    let v: Value = serde_json::from_str(&env.ok(&[
        "backup",
        "--json",
        "--exclude-caches",
        "--exclude-if-present",
        ".nobackup",
        "--exclude-larger-than",
        "1M",
        src.to_str().unwrap(),
    ]))
    .unwrap();
    // cache/blob、scratch/tmp、big.bin（3 MiB）
    assert_eq!(v["report"]["excluded"], 3, "{v}");

    let target = env.dir.path().join("out");
    env.ok(&["restore", "latest", target.to_str().unwrap()]);
    let restored = target.join(src.strip_prefix("/").unwrap_or(&src));
    assert!(restored.join("cache/CACHEDIR.TAG").is_file());
    assert!(!restored.join("cache/blob").exists());
    assert!(restored.join("scratch/.nobackup").is_file());
    assert!(!restored.join("scratch/tmp").exists());
    assert!(!restored.join("big.bin").exists());
    assert!(restored.join("a.txt").is_file());

    let err = env.fails(&[
        "backup",
        "--exclude-larger-than",
        "1MB",
        src.to_str().unwrap(),
    ]);
    assert!(err.contains("unknown unit"), "{err}");
    let err = env.fails(&[
        "backup",
        "--exclude-if-present",
        "a/b",
        src.to_str().unwrap(),
    ]);
    assert!(err.contains("single file name"), "{err}");
}

/// `--one-file-system`：在 user＋mount namespace 裡把 tmpfs 掛到來源底下再 backup（出了
/// namespace 掛載就不見，snapshot 留在 repo）。掛載點照備成空目錄、計一個 excluded；不加旗標的
/// 對照組會走進去。沒有 unshare 或不准 unprivileged namespace 的環境印出原因後略過。
#[cfg(target_os = "linux")]
#[test]
fn one_file_system_keeps_mount_points_empty() {
    use serde_json::Value;

    let env = Env::new();
    let src = env.dir.path().join("src");
    std::fs::create_dir_all(src.join("mnt")).unwrap();
    std::fs::write(src.join("a.txt"), b"a").unwrap();
    env.ok(&["init"]);
    // extra：另外指名的來源（在 namespace 裡才存在，掛載之後才建）。
    let backup_in_namespace = |flags: &str, extra: &str| {
        let script = format!(
            "mount -t tmpfs none '{mnt}' && echo inner > '{mnt}/f' && mkdir '{mnt}/deep' \
             && echo deeper > '{mnt}/deep/g' && '{kist}' backup --json {flags} '{src}' {extra}",
            mnt = src.join("mnt").display(),
            kist = env!("CARGO_BIN_EXE_kist"),
            src = src.display(),
        );
        Command::new("unshare")
            .args(["--user", "--map-root-user", "--mount", "sh", "-c", &script])
            .env("KIST_REPO", env.repo())
            .env("KIST_PASSWORD", "cli test password")
            .env("KIST_CLIENT_ID_FILE", env.dir.path().join("client-id"))
            .env("KIST_CACHE_DIR", env.dir.path().join("cache"))
            .env_remove("RUST_LOG")
            .output()
    };
    let out = match backup_in_namespace("--one-file-system", "") {
        Ok(out) if out.status.success() => out,
        Ok(out) => {
            eprintln!(
                "skipping: backup in a user+mount namespace failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            return;
        }
        Err(e) => {
            eprintln!("skipping: cannot run unshare: {e}");
            return;
        }
    };
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["report"]["excluded"], 1, "{v}");
    let target = env.dir.path().join("out");
    env.ok(&["restore", "latest", target.to_str().unwrap()]);
    let restored = target.join(src.strip_prefix("/").unwrap_or(&src));
    assert!(restored.join("mnt").is_dir(), "掛載點本身照備");
    assert!(!restored.join("mnt/f").exists(), "掛載點底下不走");
    assert!(restored.join("a.txt").is_file());

    // 對照組：不加旗標就會走進掛載點（證明上面的 tmpfs 真的掛上了）。
    let out = backup_in_namespace("", "").unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let target = env.dir.path().join("out2");
    env.ok(&["restore", "latest", target.to_str().unwrap()]);
    let restored = target.join(src.strip_prefix("/").unwrap_or(&src));
    assert_eq!(std::fs::read(restored.join("mnt/f")).unwrap(), b"inner\n");

    // 審查修正：掛載點本身也指名時整個照備，它是自己的邊界。
    let mnt = src.join("mnt");
    let out = backup_in_namespace("--one-file-system", &format!("'{}'", mnt.display())).unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["report"]["excluded"], 0, "{v}");
    let target = env.dir.path().join("out3");
    env.ok(&["restore", "latest", target.to_str().unwrap()]);
    let restored = target.join(src.strip_prefix("/").unwrap_or(&src));
    assert_eq!(std::fs::read(restored.join("mnt/f")).unwrap(), b"inner\n");
    assert_eq!(
        std::fs::read(restored.join("mnt/deep/g")).unwrap(),
        b"deeper\n"
    );

    // 指名的路徑在掛載點更底下時，只沿著通往它的方向穿過掛載點。
    let deep = src.join("mnt/deep");
    let out = backup_in_namespace("--one-file-system", &format!("'{}'", deep.display())).unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["report"]["excluded"], 1, "掛載點裡的 f：{v}");
    let target = env.dir.path().join("out4");
    env.ok(&["restore", "latest", target.to_str().unwrap()]);
    let restored = target.join(src.strip_prefix("/").unwrap_or(&src));
    assert_eq!(
        std::fs::read(restored.join("mnt/deep/g")).unwrap(),
        b"deeper\n"
    );
    assert!(!restored.join("mnt/f").exists(), "掛載點的其他內容照樣不備");
}

/// `restore SNAPSHOT:PATH`、`--dry-run`、`--include-file`（ADR 022）。
#[test]
fn restore_subtree_dry_run_and_pattern_files() {
    use serde_json::Value;

    let env = Env::new();
    let src = env.dir.path().join("src");
    make_source(&src);
    env.ok(&["init"]);
    env.ok(&["backup", src.to_str().unwrap()]);
    let abs = std::fs::canonicalize(&src).unwrap();

    // SNAPSHOT:PATH：那個目錄的內容直接放進目標。
    let target = env.dir.path().join("sub-only");
    let spec = format!("latest:{}/sub", abs.display());
    env.ok(&["restore", &spec, target.to_str().unwrap()]);
    assert_eq!(std::fs::read(target.join("b.txt")).unwrap(), b"world\n");
    assert_eq!(std::fs::read_dir(&target).unwrap().count(), 1);

    // --dry-run：列出會寫的東西，目標連建都不建。
    let target = env.dir.path().join("dry");
    let list = env.dir.path().join("include.txt");
    std::fs::write(&list, "# text files only\n*.txt\n").unwrap();
    let out = env.ok(&[
        "restore",
        "--dry-run",
        "latest",
        target.to_str().unwrap(),
        "--include-file",
        list.to_str().unwrap(),
    ]);
    let rel = abs.strip_prefix("/").unwrap();
    let file = |p: &str| format!("file {}", target.join(rel).join(p).display());
    assert!(out.contains(&file("a.txt")), "{out}");
    assert!(out.contains(&file("sub/b.txt")), "{out}");
    assert!(
        out.contains(&format!("dir  {}", target.join(rel).join("sub").display())),
        "{out}"
    );
    assert!(!out.contains("big.bin"), "{out}");
    assert!(out.contains("would restore"), "{out}");
    assert!(!target.exists(), "dry run 不建目標");
    let v: Value = serde_json::from_str(&env.ok(&[
        "restore",
        "--json",
        "--dry-run",
        "latest",
        target.to_str().unwrap(),
        "--include",
        "*.txt",
    ]))
    .unwrap();
    assert_eq!(
        (v["dry_run"].clone(), v["files"].clone(), v["dirs"].clone()),
        (Value::Bool(true), 2.into(), 1.into()),
        "{v}"
    );
    let v: Value = serde_json::from_str(&env.ok(&[
        "restore",
        "--json",
        "latest",
        env.dir.path().join("real").to_str().unwrap(),
    ]))
    .unwrap();
    assert_eq!(v["dry_run"], false);

    for (spec, needle) in [
        (
            format!("latest:{}/nope", abs.display()),
            "not in this snapshot",
        ),
        ("latest:relative/path".to_owned(), "starts with `/`"),
        (format!("latest:{}/a.txt", abs.display()), "not a directory"),
    ] {
        let (code, _, err) =
            env.kist_code(&["restore", &spec, env.dir.path().join("x").to_str().unwrap()]);
        assert_eq!(code, Some(1), "{spec}: {err}");
        assert!(err.contains(needle), "{spec}: {err}");
    }
}

/// 反向樣式與不分大小寫的旗標（ADR 023）：backup 只備 sub（`!` 加回來），restore 用不分
/// 大小寫的 include 選檔。
#[test]
fn negated_and_case_insensitive_patterns() {
    let env = Env::new();
    let src = env.dir.path().join("src");
    make_source(&src);
    std::fs::write(src.join("sub/C.TXT"), b"upper").unwrap();
    let abs = std::fs::canonicalize(&src).unwrap();
    env.ok(&["init"]);
    let all = format!("{}/*", abs.display());
    let keep = format!("!{}/sub", abs.display());
    let out = env.ok(&[
        "backup",
        "--exclude",
        &all,
        "--exclude",
        &keep,
        "--iexclude",
        "c.txt",
        src.to_str().unwrap(),
    ]);
    assert!(
        out.contains("excluded: 3"),
        "a.txt、big.bin、sub/C.TXT：{out}"
    );

    let target = env.dir.path().join("out");
    env.ok(&[
        "restore",
        "latest",
        target.to_str().unwrap(),
        "--iinclude",
        "*.TXT",
    ]);
    let restored = target.join(abs.strip_prefix("/").unwrap());
    assert!(restored.join("sub/b.txt").is_file());
    assert!(!restored.join("a.txt").exists());
    assert!(!restored.join("sub/C.TXT").exists());
    let err = env.fails(&["backup", "--iexclude", "[x", src.to_str().unwrap()]);
    assert!(err.contains("--iexclude"), "{err}");
}

/// `--files-from`、`--files-from-verbatim`、`--files-from-raw`（ADR 024）：來源路徑從檔案讀，
/// 可以和命令列的路徑一起給；`-` 是標準輸入。三種的差別只在怎麼切行。
#[cfg(unix)]
#[test]
fn backup_reads_source_paths_from_files() {
    use serde_json::Value;
    use std::io::Write;
    use std::os::unix::ffi::OsStrExt;

    let env = Env::new();
    let base = env.dir.path().join("data");
    for name in [
        "one",
        "two",
        "trailing space ",
        "new\nline",
        "three",
        "four",
    ] {
        std::fs::create_dir_all(base.join(name)).unwrap();
        std::fs::write(base.join(name).join("f"), name).unwrap();
    }
    env.ok(&["init"]);
    let roots = |v: &Value| -> Vec<String> {
        v["roots"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| {
                r["path"]
                    .as_str()
                    .unwrap()
                    .rsplit('/')
                    .next()
                    .unwrap()
                    .to_owned()
            })
            .collect()
    };

    // 一般：去掉前後空白，空行與 `#` 略過。
    let plain = env.dir.path().join("plain.txt");
    std::fs::write(
        &plain,
        format!(
            "# my list\n\n  {}  \r\n{}\n",
            base.join("one").display(),
            base.join("two").display()
        ),
    )
    .unwrap();
    let v: Value = serde_json::from_str(&env.ok(&[
        "backup",
        "--json",
        "--files-from",
        plain.to_str().unwrap(),
        base.join("three").to_str().unwrap(),
    ]))
    .unwrap();
    assert_eq!(roots(&v), ["one", "three", "two"]);

    // verbatim：一行就是一個路徑，空白照留（只略過空行）。
    let verbatim = env.dir.path().join("verbatim.txt");
    std::fs::write(
        &verbatim,
        format!("{}\n\n", base.join("trailing space ").display()),
    )
    .unwrap();
    let v: Value = serde_json::from_str(&env.ok(&[
        "backup",
        "--json",
        "--files-from-verbatim",
        verbatim.to_str().unwrap(),
    ]))
    .unwrap();
    assert_eq!(roots(&v), ["trailing space "]);

    // raw：NUL 結尾，名字裡可以有換行；從標準輸入讀。
    let mut raw = Vec::new();
    raw.extend_from_slice(base.join("new\nline").as_os_str().as_bytes());
    raw.push(0);
    raw.extend_from_slice(base.join("four").as_os_str().as_bytes());
    raw.push(0);
    let mut child = Command::new(env!("CARGO_BIN_EXE_kist"))
        .args(["backup", "--json", "--files-from-raw", "-"])
        .env("KIST_REPO", env.repo())
        .env("KIST_PASSWORD", "cli test password")
        .env("KIST_CLIENT_ID_FILE", env.dir.path().join("client-id"))
        .env("KIST_CACHE_DIR", env.dir.path().join("cache"))
        .env_remove("RUST_LOG")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(&raw).unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success());
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(roots(&v), ["four", "new\nline"]);

    // 讀完是空的、或列了不存在的路徑：結束碼 1，不留 snapshot。
    let empty = env.dir.path().join("empty.txt");
    std::fs::write(&empty, "# nothing\n").unwrap();
    let (code, _, err) = env.kist_code(&["backup", "--files-from", empty.to_str().unwrap()]);
    assert_eq!(code, Some(1), "{err}");
    assert!(err.contains("no source paths"), "{err}");
    let globbed = env.dir.path().join("globbed.txt");
    std::fs::write(&globbed, format!("{}/*\n", base.display())).unwrap();
    let (code, _, err) = env.kist_code(&["backup", "--files-from", globbed.to_str().unwrap()]);
    assert_eq!(code, Some(1), "沒有萬用字元展開：{err}");
    let snapshots: Value = serde_json::from_str(&env.ok(&["snapshots", "--json"])).unwrap();
    assert_eq!(snapshots.as_array().unwrap().len(), 3);
}

/// init 可以直接指定 head 0 的 settings（M12 第 7 段第十步）。真實時鐘的 GC 驗收發現：init 之後再
/// `settings --grace 2h` 是調低，會開始一個 72 小時的 drain（format-v4.md §11.4），期間新的標記都以舊的 grace
/// 當 age——新 repo 想用短的 grace 只能在 init 時給。範圍同 `settings`，不合的什麼都不寫。
#[test]
fn init_takes_the_settings_of_generation_0_without_a_drain() {
    use serde_json::Value;

    let env = Env::new();
    env.ok(&[
        "init",
        "--grace",
        "2h",
        "--replicas",
        "0",
        "--pack-target",
        "1m",
        "--mpack-target",
        "64k",
    ]);
    let v: Value = serde_json::from_str(&env.ok(&["settings", "--json"])).unwrap();
    assert_eq!(v["generation"], 0);
    assert_eq!(v["settings"]["grace"], 2 * 3600);
    assert_eq!(v["settings"]["replicas"], 0);
    assert_eq!(v["settings"]["pack_target"], 1024 * 1024);
    assert_eq!(v["settings"]["mpack_target"], 64 * 1024);
    assert!(v["drain"].is_null(), "nothing was lowered: {v}");

    // 沒給的用預設（本機 replicas 1）
    let plain = Env::new();
    plain.ok(&["init", "--grace", "3h"]);
    let v: Value = serde_json::from_str(&plain.ok(&["settings", "--json"])).unwrap();
    assert_eq!(v["settings"]["grace"], 3 * 3600);
    assert_eq!(v["settings"]["replicas"], 1);
    assert_eq!(v["settings"]["pack_target"], 64 * 1024 * 1024);
    assert_eq!(v["settings"]["mpack_target"], 8 * 1024 * 1024);

    // 範圍同 settings：grace 低於 2 小時被擋在寫任何東西之前
    let bad = Env::new();
    let stderr = bad.fails(&["init", "--grace", "1h"]);
    assert!(stderr.contains("grace"), "{stderr}");
    assert!(
        !bad.repo().join("config").exists(),
        "a refused init writes nothing"
    );

    // 對照：init 之後調低才是 drain
    let later = Env::new();
    later.ok(&["init"]);
    let v: Value =
        serde_json::from_str(&later.ok(&["settings", "--json", "--grace", "2h"])).unwrap();
    assert_eq!(v["generation"], 1);
    assert_eq!(v["drain"]["from"], 1);
    assert_eq!(v["drain"]["old_grace"], 72 * 3600);
}
