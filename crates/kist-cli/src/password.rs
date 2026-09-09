//! 取得 repo 密碼，優先順序：`--password-file` → `KIST_PASSWORD` 環境變數 → 終端機互動輸入。

use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use zeroize::Zeroizing;

/// 回傳的字串離開作用域時會被清零。
pub fn obtain(password_file: &Option<PathBuf>, confirm: bool) -> Result<Zeroizing<String>> {
    if let Some(path) = password_file {
        let content = Zeroizing::new(
            std::fs::read_to_string(path)
                .with_context(|| format!("cannot read password file {}", path.display()))?,
        );
        let first = Zeroizing::new(content.lines().next().unwrap_or("").to_owned());
        if first.is_empty() {
            bail!("password file {} is empty", path.display());
        }
        return Ok(first);
    }
    if let Ok(pw) = std::env::var("KIST_PASSWORD") {
        let pw = Zeroizing::new(pw);
        if pw.is_empty() {
            bail!("KIST_PASSWORD is set but empty");
        }
        return Ok(pw);
    }
    let pw = Zeroizing::new(
        rpassword::prompt_password("repository password: ")
            .context("cannot read password from terminal (use --password-file or KIST_PASSWORD)")?,
    );
    if pw.is_empty() {
        bail!("password must not be empty");
    }
    if confirm {
        let again = Zeroizing::new(rpassword::prompt_password("confirm password: ")?);
        if *again != *pw {
            bail!("passwords do not match");
        }
    }
    Ok(pw)
}
