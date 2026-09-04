# kist

去重、加密、可多台機器共用 repo 的備份工具（Rust）。

> 目前狀態：**M3 完成** —— 本機與 S3（含 MinIO）repo 的 `init` / `backup` / `snapshots` /
> `restore` / `check` / `rebuild-index` / `forget` / `prune` 可用，多台機器可同時備份到同一個 repo，
> GC 不需要鎖，on-disk 格式已凍結（見 [docs/format.md](docs/format.md)）。
> 依 PLAN，到這裡可以開始自己使用；M4（SFTP、mount、設定檔、排程、Web UI）與 M5（硬化）還沒做。

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
kist rebuild-index                      # index 物件遺失或損壞時，從 pack 重建
kist forget --keep-daily 7 --keep-weekly 4 --keep-monthly 12   # 依保留政策刪 snapshot
kist forget latest                      # 或指定 snapshot（id、時間戳前綴、latest）
kist prune                              # 回收空間（兩階段，見下）
kist forget --keep-last 10 --prune      # 一次做完
```

結束碼：0 成功；1 失敗；3 完成但有項目被略過（backup）、還原失敗（restore）或刪不掉（prune）——
請看警告。

### 空間回收（GC）

`forget` 只刪 snapshot；`prune` 才回收資料，而且分兩階段：第一次執行把沒人引用的 pack / tree / index
**標記**起來，等超過 grace（預設 72 小時）而且每台活躍的機器在標記後都又備份過一次，
第二次執行才真的刪。中間任何一台機器重新用到那些資料，標記就撤銷。所以 `prune` 可以跟 backup
同時跑、可以排程每天跑，不需要鎖；剛 forget 完馬上 prune 不會釋放空間，那是設計。

- `--grace` 必須長於你最長的一次 backup（預設 72h；`backup --gc-grace` 要用同一個值）。
  跑得更久的 backup 不會悄悄壞掉，會在最後以錯誤結束，重跑即可。
- 超過 `--inactive-after`（預設 30 天）沒備份的機器不再擋住刪除；它回來備份時若用到已刪的資料，
  同樣會在最後失敗、重跑。
- 活資料比例低於 `--repack-below`（預設 50%）的 pack 會被重新打包。
- `--dry-run` 只報告。刪不掉的物件（S3 Object Lock、權限）會回報並保留標記，結束碼 3。
- 同一台機器同時只能跑一個 backup（client id 檔旁邊有鎖）。
- bucket 有 versioning 時，真的釋放空間還需要 lifecycle 規則清掉舊版本。

細節與安全性論證：[docs/format.md §11](docs/format.md)、[ADR 005](docs/decisions/005-m3-gc.md)。

本地 index 快取放在使用者快取目錄（Linux：`~/.cache/kist/`），可用 `--cache-dir` /
`KIST_CACHE_DIR` 指定、`--no-cache` 關閉。`check` 永遠不用快取。

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
`ListBucket`（不需要 `DeleteObject`）；`forget` / `prune` 另外需要 `DeleteObject`，
建議用另一組憑證在別台機器跑。

每台機器第一次 backup 時會產生一個 client id（`~/.local/share/kist/client-id`，
可用 `--client-id-file` 或 `KIST_CLIENT_ID_FILE` 指定）。

## 開發

所有驗證都在本機跑（GitHub Actions 的 workflow 只留手動觸發，避免吃配額）；四道關卡：

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

## M2 驗收數據（S3 / MinIO）

2026-09-05，commit `09f6f82`，同一台機器、同一份資料集，repo 放在本機 Docker 裡的 MinIO
（`minio/minio:RELEASE.2025-09-07`，HTTP、無 TLS），腳本 `tests/acceptance/run_s3.py`，
完整 log `tests/acceptance/m2-acceptance-s3-2026-09-05.log`。

| 項目 | 結果 | 對照本機後端 |
| --- | --- | --- |
| 第一次 backup | 157 s；77 個 pack（4.90 GiB） | 54 s |
| 第二次 backup（內容未變） | 35 s；**0 個新 pack、0 個新 chunk** | 0.8 s |
| restore | 213 s；`diff -r` **完全相同** | 41 s |
| `check` | 2 s | 0.3 s |
| `check --read-data` | 28 s | 17 s |
| `rebuild-index`（77 個 pack，只讀 trailer） | 0.4 s；再 `check` 通過 | — |
| 人為翻轉 pack 中一個 bit（透過 `mc` 上傳） | `check --read-data` 以非 0 結束並指名該 pack | 同 |
| 峰值 RSS（backup 期間） | **242 MiB** | 186 MiB |

第二次 backup 與 restore 對 S3 明顯慢：前者每次仍 put 全部 1 102 個 tree 並逐個讀 parent 的 tree，
後者對每個 chunk 各發一次 range GET（71 040 次）。
UNVERIFIED（未做 profile，只是從請求數推測）。兩者都列在 M3/M4 待辦（ADR 004「沒做」）。

## M3 驗收數據（GC）

2026-09-05，commit `783a570`（審查修正後），本機目錄後端，同一份資料集，腳本 `tests/acceptance/run_gc.py`，
完整 log `tests/acceptance/m3-acceptance-gc-2026-09-05.log`。流程：backup → 刪掉一成的目錄再 backup →
forget 舊 snapshot → `prune --grace 0s`（標記 + repack）→ backup → prune（刪）→ 再兩輪 backup + prune 收尾
→ `check --read-data` → restore 與 `diff -r`。

| 項目 | 結果 |
| --- | --- |
| prune 1（標記 112 個 tree + 8 個被 repack 的 pack，搬 18 MiB 活資料到 1 個新 pack） | 3.3 s |
| 標記後的 backup（內容未變） | 5 s；**0 個新 chunk**（不會把被標記 pack 裡的資料重傳） |
| prune 2（刪 120 個物件、520 MiB） | 1.1 s |
| prune 3–4（清掉被取代的 index blob，之後無事可做） | 1.1 s 各 |
| repo 大小 | 4.90 GiB → **4.41 GiB**（刪掉的一成資料全部回收） |
| `check --read-data` 之後 | 26 s，無錯誤、無警告 |
| restore 最新 snapshot | 57 s，`diff -r` **完全相同** |
| 峰值 RSS（整個流程） | 233 MiB |
| 競態 proptest（`tests/gc_race.rs`） | 24 案例進 `cargo test`；200 案例本機跑（見 ADR 005 §10） |

grace 設 0 只是為了驗收能在幾分鐘內走完；實際使用請保留預設 72 h。

## Clean build 時間

機器：AMD Ryzen 7 7700（8C/16T）、Linux 7.2.0、rustc 1.98.1、cargo 1.98.1。
`cargo clean` 之後、相依套件已在本機 cargo 快取中（不含下載時間）：

| 階段 | `cargo build` | `cargo build --release` |
| --- | --- | --- |
| M0 骨架（只有 clap） | 3.9 s | 3.5 s |
| M1（完整相依） | 8.7 s | 15.8 s |

（CI 從未實際執行；負責人決定所有驗證只在本機跑。）

## 授權

MIT OR Apache-2.0（你可以任選其一）。見 [LICENSE-MIT](LICENSE-MIT) 與
[LICENSE-APACHE](LICENSE-APACHE)。
