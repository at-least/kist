# kist

去重、加密、可多台機器共用 repo 的備份工具（Rust）。

> 目前狀態：**M0 骨架**。只有 `kist version` 可以跑，還沒有任何備份功能。
> 完整規劃見專案負責人手上的 PLAN.md（未進版控）。

## 建置與執行

```sh
cargo build --release
./target/release/kist version
```

## 開發

本地跑一遍和 CI 一樣的四道關卡：

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo deny check          # 需先 cargo install --locked cargo-deny
```

## Workspace 結構

| crate | 職責 |
| --- | --- |
| `kist-format` | on-disk 格式：結構定義、CBOR 序列化、版本演進 |
| `kist-crypto` | 金鑰階層與 AEAD 封裝 |
| `kist-chunker` | 內容定義切塊（FastCDC） |
| `kist-backend` | 儲存後端（local / S3 / GCS / Azure）與本地 index 快取 |
| `kist-core` | backup / restore / check / prune 流程 |
| `kist-cli` | `kist` 執行檔（clap） |

設計決策記錄在 `docs/decisions/`。

## Clean build 時間（M0 骨架）

機器：AMD Ryzen 7 7700（8C/16T）、Linux 7.2.0、rustc 1.98.1、cargo 1.98.1。
`cargo clean` 之後、相依套件已在本機 cargo 快取中（不含下載時間）：

| 指令 | 時間 |
| --- | --- |
| `cargo build` | 3.9 s |
| `cargo build --release` | 3.5 s |

CI 上的時間會比較長（要下載相依套件），`Swatinem/rust-cache` 負責跨次數快取。
