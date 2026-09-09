//! `kist version` 的行為測試：先有測試，再有實作。

use std::process::Command;

/// 執行編譯出來的 `kist` binary，回傳 (成功與否, stdout)。
fn run(args: &[&str]) -> (bool, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_kist"))
        .args(args)
        .output()
        .expect("執行 kist binary 失敗");
    (
        output.status.success(),
        String::from_utf8(output.stdout).expect("stdout 不是合法的 UTF-8"),
    )
}

#[test]
fn version_subcommand_prints_name_and_version() {
    let (ok, stdout) = run(&["version"]);
    assert!(ok, "`kist version` 應該成功結束");
    assert!(
        stdout.starts_with("kist "),
        "輸出應該以 `kist ` 開頭，實際是：{stdout:?}"
    );
    assert!(
        stdout.trim().ends_with(env!("CARGO_PKG_VERSION")),
        "輸出應該包含 crate 版本 {}，實際是：{stdout:?}",
        env!("CARGO_PKG_VERSION")
    );
}

#[test]
fn version_flag_matches_subcommand() {
    let (ok, flag) = run(&["--version"]);
    assert!(ok, "`kist --version` 應該成功結束");
    let (_, sub) = run(&["version"]);
    assert_eq!(flag, sub, "`--version` 與 `version` 的輸出應該一致");
}
