# kist

Deduplicating, encrypted backups to object storage, from one binary with no cgo.

> **狀態：儲存格式 v3（2026-09-09 定案，與本 repo 根目錄的 [docs/format.md](../docs/format.md) 逐 byte 一致）。**
> 本目錄是 Go 參考實作，與根目錄的 Rust 產品讀寫同一種 repo、互為驗證（[`PLAN.md`](PLAN.md)）。
> 本機 / S3 / SFTP 後端、無鎖 GC（`forget` / `prune`）、排程 `run`、`mount` 都已具備。

```console
$ export KIST_REPOSITORY=/backup/kist KIST_PASSWORD=...
$ kist-go init
$ kist-go backup ~/work
snapshot snapshots/fe2988.../20260904T141955943279016Z
  3 files, 3 directories, 1 symlinks
  3.7 MiB read, 2.9 MiB stored in 1 new packs
$ kist-go backup ~/work         # 沒改東西
  3.7 MiB read, 0 B stored in 0 new packs
$ kist-go snapshots
$ kist-go restore snapshots/fe2988.../20260904T141955943279016Z /tmp/out
$ kist-go check --read-data
```

## 它做什麼

- **去重**：FastCDC 內容定義切塊（512 KiB / 2 MiB / 8 MiB）。在檔案開頭插入位元組不會重排後面的邊界。
- **加密**：一律加密，沒有明文 repo 這個選項。XChaCha20-Poly1305，金鑰從 Argon2id 派生的階層而來。
- **多機共用一個 repo**：沒有鎖。snapshot 用條件寫入提交，其他所有物件都以內容 hash 命名且不可變。
- **可修復**：index 只是快取，永遠可以從 pack 重建。壞掉的 index blob 不會讓 repo 打不開。
- **會說實話**：跳過的檔案、還原不了的中繼資料、`check` 找到的問題，全部會講出來。`check` 有發現就以非零結束。

## 指令

| 指令 | 說明 |
| --- | --- |
| `kist-go init` | 建立 repo（密碼問兩次，救不回來） |
| `kist-go backup [--parity M] <path>...` | 備份並提交一個 snapshot；`--parity 2` 在每個 pack 旁存 12.5% 的 Reed-Solomon 冗餘 |
| `kist-go snapshots` | 列出 snapshot，舊的在前 |
| `kist-go restore <snapshot> <target>` | 還原到一個空目錄 |
| `kist-go check [--read-data] [--repair]` | 驗證 repo；`--repair` 用 parity 修回損壞的 pack |
| `kist-go forget --keep-daily 7 ...` | 依 retention 規則（或指名）移除 snapshot |
| `kist-go prune` | 標記沒人引用的 pack，grace（預設 72h）之後的下一次刪掉 |
| `kist-go rebuild-index` | 從 pack trailer 重建 index |
| `kist-go run --config kist.toml [--once]` | 依設定檔的排程跑備份與維護工作 |
| `kist-go mount <dir>` | 把所有 snapshot 掛成唯讀檔案系統（`<client>/<時間戳>/…`，Linux/macOS） |

每個指令都接受 `--json`：stdout 只印一個 JSON 物件（`snapshots` 印一個陣列），警告與進度仍在 stderr，失敗仍以非零結束。

`forget` 與 `prune` 要用持有 Delete 權限的憑證跑；備份用的憑證做不到（見 [docs/format.md](docs/format.md) §15 後端契約的權限分工）。`prune` 定期跑：第一次只標記，grace 過後的下一次才刪，中間有 client 引用到被標記的 pack 會自動復活它。

repo 位置：`--repo` 或 `$KIST_REPOSITORY`——本機路徑、`s3://bucket/prefix`、或 `sftp://user@host:port/path`（`/~/path` 表示相對於登入目錄）。SFTP 一定驗 host key（`~/.ssh/known_hosts` 或 `$KIST_SFTP_KNOWN_HOSTS`，先 `ssh-keyscan`）；認證依序試 SSH agent、`$KIST_SFTP_KEY`（`$KIST_SFTP_KEY_PASSPHRASE`）、`$KIST_SFTP_PASSWORD`。
密碼：`--password-file`、`$KIST_PASSWORD`，或終端機提示，依此順序。

## 設定檔與 `run`

```toml
[repository]
location = "s3://bucket/kist"          # 或本機路徑、sftp://user@host/path
password_file = "/etc/kist/password"   # 或 $KIST_PASSWORD
parity = 2                             # 可選：每個 pack 的 Reed-Solomon parity shard 數（16 個 data shard）

[[backup]]
name = "home"
paths = ["/home", "/etc"]
schedule = "0 2 * * *"                 # 標準 cron 五欄，或 @daily / @hourly
pre_backup  = ["/usr/local/sbin/lvm-snap", "create"]   # 可選，argv；失敗就不備份
post_backup = ["/usr/local/sbin/lvm-snap", "release"]  # 可選；一定會跑

[retention]                            # 由 [prune] 的維護工作套用，因為 forget 是刪除
keep_daily = 7
keep_weekly = 4
keep_monthly = 6

[prune]
schedule = "0 4 * * 0"
grace = "72h"

[webhook]                              # 每個工作結束 POST 一個 JSON（跟 --json 同一個格式）
url = "https://hooks.example/kist"

[metrics]                              # Prometheus 文字格式，/metrics
listen = "127.0.0.1:9345"
```

未知的 key 是錯誤，不是被忽略的拼字錯。`[[backup]]` 放在被備份的機器上、用只能寫的憑證；`[prune]`（含 `[retention]`）放在維護主機上、用能刪的憑證。一份設定同時有兩者可以跑，但 `run` 會警告：那台機器持有能刪掉自己備份的憑證。工作一次跑一個，不重疊；備份跑過了下一個 tick，tick 延後、不並發。VSS / LVM 快照沒有內建：`pre_backup` / `post_backup` 就是整個機制，知道那台機器怎麼拍快照的腳本是你的。

### `--json` 與 webhook 的欄位

一個事件：`kind`（`init backup forget prune check restore rebuild_index`）、`job`（run 模式的工作名）、`started`、`finished`、`ok`、`error`、`warnings`，加上一個對應 kind 的子物件：

- `backup`：`snapshot host paths files dirs symlinks bytes bytes_stored chunks_new packs_added packs_revived`
- `forget`：`dry_run removed kept locked`
- `prune`：`dry_run packs_stored packs_live marked unmarked deleted locked held[{pack,reason}] bytes_reclaimed`
- `check`：`read_data snapshots trees chunks packs problems parity_packs repaired unrepairable`
- `restore`：`snapshot target files dirs symlinks hard_links bytes`
- `init`：`location client_id`；`rebuild_index`：`chunks`

`snapshots --json` 是陣列：`snapshot client_id time host paths files bytes`（讀不出來的列有 `error`）。

Metrics：`kist_runs_total{job,result}`、`kist_last_run_timestamp_seconds{job,result}`、`kist_last_run_duration_seconds{job}`、`kist_last_backup_{files,bytes,bytes_stored,packs_added}{job}`、`kist_last_prune_packs_{stored,live,deleted,held}`、`kist_prune_bytes_reclaimed_total`。

## `check` 的兩個層級

它們抓的是不同的東西，誰也不包含誰：

- **預設**（只讀中繼資料與 pack trailer）：抓得到不見的 pack、被截斷的 pack、指向不存在 chunk 的 tree——所有從 repo 的「形狀」看得出來的損壞。
- **`--read-data`**：另外把每個 chunk 讀出來解密驗證。**只有這個層級抓得到 chunk 資料裡被翻轉的位元**，因為別的路徑根本不會去解密那些資料。代價是讀完整個 repo。

## 文件

- [`docs/format.md`](docs/format.md) — 儲存格式 v3（唯一權威副本在 repo 根目錄的 [`docs/format.md`](../docs/format.md)，本副本逐 byte 一致，有測試釘住），含「設計決定 × 證據」對照表
- [`docs/decisions/`](docs/decisions/) — ADR，記錄為什麼這樣設計
- [`docs/release.md`](docs/release.md) — 版本、平台、release 流程
- [`PLAN.md`](PLAN.md) — 里程碑與工程規範

## 開發

```console
$ make verify        # build + vet + lint + test + test-race，這是「做完了」的判準
$ make test-s3       # MinIO in Docker；make test-sftp 同理用 OpenSSH
$ make fuzz          # 每個 Fuzz 目標跑 FUZZTIME（預設 30s）；make fuzz-long 是 24h
$ make release-snapshot   # 用 goreleaser 在本機建出所有平台的 artifact，不需要 tag
$ make fuzz          # 跑所有 FuzzXxx target
```

大規模驗收測試（10 萬檔 / 10 GiB，預設關閉）：

```console
$ KIST_ACCEPTANCE=1 KIST_ACCEPTANCE_DIR=/somewhere/with/30GiB \
    go test -v -timeout 180m ./internal/repo/ -run TestAcceptance
```

## 不做的事

不支援非加密 repo、不做 GUI、不自己實作加密原語。
