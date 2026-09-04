# kist

去重、加密、可多台機器共用 repo 的備份工具（Rust）。

> 目前狀態：**M1 完成** —— 本機 repo 的 `init` / `backup` / `snapshots` / `restore` / `check`
> 可用，on-disk 格式已凍結（見 [docs/format.md](docs/format.md)）。
> S3 後端、GC、排程等在後續里程碑。**尚未達到可日常使用的階段。**

## 建置

```sh
cargo build --release
./target/release/kist --help
```

## 使用

```sh
export KIST_REPO=/path/to/repo          # 或每個命令加 --repo
export KIST_PASSWORD=...                # 或 --password-file，或互動輸入

kist init                               # 建立 repo
kist backup ~/Documents ~/Photos        # 備份，產生一個 snapshot
kist snapshots                          # 列出 snapshot
kist restore latest /tmp/out            # 還原到 /tmp/out/<原本的絕對路徑>
kist check                              # 檢查一致性（不下載資料）
kist check --read-data                  # 下載並驗證每個 chunk
```

### S3 / MinIO

```sh
export KIST_REPO=s3://my-bucket/backups/laptop
export AWS_ACCESS_KEY_ID=... AWS_SECRET_ACCESS_KEY=... AWS_DEFAULT_REGION=us-east-1
# MinIO 或其他自架的 S3 相容服務：
export AWS_ENDPOINT=http://minio.local:9000 AWS_ALLOW_HTTP=true
kist init
```

`config` 是 repo 裡唯一可覆寫的物件，被蓋掉就打不開 repo：請對 bucket 開 versioning 或
Object Lock，並把 `config` 另存一份。backup 需要的權限是 `PutObject`、`GetObject`、
`ListBucket`（不需要 `DeleteObject`）。

每台機器第一次 backup 時會產生一個 client id（`~/.local/share/kist/client-id`，
可用 `--client-id-file` 或 `KIST_CLIENT_ID_FILE` 指定）。

## 開發

本地跑一遍和 CI 一樣的四道關卡：

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo deny check          # 需先 cargo install --locked cargo-deny
```

S3 整合測試預設略過；起一個 MinIO 容器並設環境變數就會跑（見 `tests/README.md`）。

## Workspace 結構

| crate | 職責 |
| --- | --- |
| `kist-format` | on-disk 格式：結構定義、CBOR 序列化、golden files（改動需負責人確認） |
| `kist-crypto` | 金鑰階層（Argon2id → KEK → master key → 派生子金鑰）與 XChaCha20-Poly1305 封裝 |
| `kist-chunker` | 內容定義切塊（FastCDC，512 KiB / 2 MiB / 8 MiB） |
| `kist-backend` | 儲存後端（`object_store`；M1 只有本機目錄）|
| `kist-core` | backup / restore / check / snapshots 流程 |
| `kist-cli` | `kist` 執行檔（clap） |

格式規格：[docs/format.md](docs/format.md)。設計決策：[docs/decisions/](docs/decisions/)。

## M1 驗收數據

2026-09-05，commit `26637f0`（審查修正後），同一台機器（Ryzen 7 7700、NVMe/btrfs，
本機目錄後端），release build。資料集：100 000 個檔案、9.75 GiB（一半亂數、一半可壓縮），
腳本與完整 log 在 `tests/acceptance/`。

| 項目 | 結果 |
| --- | --- |
| 第一次 backup | 54 s；寫 77 個 pack（repo 4.90 GiB，可壓縮的那一半被壓掉了） |
| 第二次 backup（內容未變） | 0.8 s；**0 個新 pack、0 個新 chunk**（含重新 put 1 102 個 tree） |
| restore | 41 s；`diff -r` 與原始資料**完全相同** |
| `check` | 0.3 s |
| `check --read-data` | 17 s |
| 人為翻轉 pack 中一個 bit | `check --read-data` 以非 0 結束並指名該 pack |
| 峰值 RSS（backup 期間，`ru_maxrss`） | **186 MiB**（審查修正前 253 MiB） |
| 512 MiB 單一大檔的 RSS 成長（reviewer 的 probe） | 22 MiB（修正前 736 MiB） |

## Clean build 時間

機器：AMD Ryzen 7 7700（8C/16T）、Linux 7.2.0、rustc 1.98.1、cargo 1.98.1。
`cargo clean` 之後、相依套件已在本機 cargo 快取中（不含下載時間）：

| 階段 | `cargo build` | `cargo build --release` |
| --- | --- | --- |
| M0 骨架（只有 clap） | 3.9 s | 3.5 s |
| M1（完整相依） | 8.7 s | 15.8 s |

CI 上的時間會比較長（要下載相依套件），`Swatinem/rust-cache` 負責跨次數快取。

## 授權

MIT OR Apache-2.0（你可以任選其一）。見 [LICENSE-MIT](LICENSE-MIT) 與
[LICENSE-APACHE](LICENSE-APACHE)。
