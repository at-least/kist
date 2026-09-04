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
    let (ok, stdout, stderr) = env.kist(&["backup", src.to_str().unwrap()]);
    std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o644)).unwrap();
    if nix_is_root() {
        return; // root 讀得到所有檔案，這個測試沒有意義
    }
    assert!(!ok, "應以非 0 結束\nstdout: {stdout}\nstderr: {stderr}");
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
