//! `kist` 命令列入口。
//!
//! M0 只提供 `kist version`；其他命令會在後續里程碑加入。

#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used)]

use anyhow::Result;
use clap::{Parser, Subcommand};

/// kist：去重、加密、可多台機器共用 repo 的備份工具。
#[derive(Debug, Parser)]
#[command(name = "kist", version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// 顯示版本資訊。
    Version,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Version => {
            println!("{} {}", env!("CARGO_BIN_NAME"), env!("CARGO_PKG_VERSION"));
        }
    }
    Ok(())
}
