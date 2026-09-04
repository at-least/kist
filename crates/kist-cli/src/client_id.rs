//! 這台機器的 client id：16 bytes 亂數，第一次用時產生並存成 hex 檔。
//!
//! 預設位置是使用者資料目錄下的 `kist/client-id`（Linux：`~/.local/share/kist/client-id`）。
//! 刻意不用 hostname：改機器名稱不該變成一個新的 client（會影響日後 GC 的活躍判定）。

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

pub fn load_or_create(explicit: Option<&Path>) -> Result<[u8; 16]> {
    let path = match explicit {
        Some(p) => p.to_path_buf(),
        None => default_path()?,
    };
    if path.is_file() {
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("cannot read client id file {}", path.display()))?;
        let bytes = hex::decode(text.trim())
            .with_context(|| format!("client id file {} is not valid hex", path.display()))?;
        let id: [u8; 16] = bytes
            .try_into()
            .map_err(|_| anyhow::anyhow!("client id file {} must hold 16 bytes", path.display()))?;
        return Ok(id);
    }
    let id = kist_crypto::random_bytes::<16>();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("cannot create {}", parent.display()))?;
    }
    std::fs::write(&path, format!("{}\n", hex::encode(id)))
        .with_context(|| format!("cannot write client id file {}", path.display()))?;
    Ok(id)
}

fn default_path() -> Result<PathBuf> {
    let Some(base) = dirs::data_local_dir() else {
        bail!("cannot determine the local data directory; pass --client-id-file");
    };
    Ok(base.join("kist").join("client-id"))
}
