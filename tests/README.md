# 整合測試

## S3 / MinIO

S3 相關的測試（`crates/kist-backend/tests/s3.rs`、`contract_v4.rs` 裡的 `s3_*`、
`crates/kist-core/tests/v4_backup_backends.rs` 與 `crates/kist-cli/tests/cli.rs` 裡的 `s3_*`）預設略過，設了環境變數才會跑：

```sh
eval "$(sh tests/minio-setup.sh)"   # 起容器、建 bucket、建 Put-only 使用者，並設好環境變數
cargo test --workspace
docker rm -f kist-minio             # 用完關掉
```

`tests/minio-setup.sh` 會印出要 export 的環境變數（root 帳號、Put/Get/List-only 的 `kistbackup`
帳號，以及依 client 分前綴的 `kistclient` 帳號：`KIST_TEST_S3_CLIENT_KEY` / `_SECRET` / `_ID`，政策只允許在
`*/{packs,mpacks,parity,deltas,snapshots}/<client id>/` 底下 PutObject、讀全部、列 bucket——format-v4.md §1
建議的部署；`v4_backup_backends.rs` 的 `s3_client_prefix_credentials_confine_a_client` 驗它備得了份、寫別處與刪除
都被擋、prune 在 P0 就失敗）。腳本另外試建一個只放行帶 `If-None-Match` 的 PutObject 的 `kistcond` 帳號（§1 的
`s3:if-none-match` 條件，AWS 的 `Null` 寫法）：MinIO RELEASE.2025-09-07 不認得這個條件 key（`invalid condition key`），
建不起來就印一行說明、不 export `KIST_TEST_S3_COND_KEY` / `_SECRET`，`s3_conditional_write_policy_blocks_plain_puts`
略過；換成支援的 S3 時它會跑。每個測試用隨機 prefix，同一個 bucket 可以重複跑。（CI workflow
只手動觸發；平時以本機 gate 為準。）

## SFTP

SFTP 相關的測試（`crates/kist-backend/tests/sftp.rs`、`contract_v4.rs` 裡的
`sftp_*`、`crates/kist-core/tests/v4_backup_backends.rs` 裡的 `sftp_*`）預設略過，設了環境變數才會跑：

```sh
eval "$(sh tests/sftp-setup.sh)"    # 起 atmoz/sftp 容器、keyscan 產 known_hosts、備 key 認證素材
cargo test --workspace
docker rm -f kist-sftp              # 用完關掉
```

`tests/sftp-setup.sh` 會印出要 export 的環境變數：`KIST_TEST_SFTP_URL`（port 固定
19222，可用 `SFTP_PORT` 蓋過；`kisttest` 帳號）、密碼、known_hosts 路徑，以及帶
passphrase 的 ed25519 key（測 `KIST_TEST_SFTP_KEY` / `KIST_TEST_SFTP_KEY_PASSPHRASE`
認證路徑——測試專用的名稱，不是 CLI 的 `KIST_SFTP_*`）。容器的 OpenSSH sftp-server
支援 kist 需要的 `hardlink@openssh.com` / `posix-rename@openssh.com` 擴充。

## rclone 橋接

rclone 相關的測試（`crates/kist-backend/tests/rclone.rs`、`rclone_bin.rs`、`contract_v4.rs` 裡的
`rclone_*`、`crates/kist-core/tests/v4_backup_backends.rs` 裡的 `rclone_*`）預設略過，
需要 PATH 上有 rclone 並設環境變數：

```sh
eval "$(sh tests/rclone-setup.sh)"  # 只是確認 rclone 在 PATH 上並 export KIST_TEST_RCLONE=1
cargo test --workspace
```

stdio 模式不用起伺服器：`rclone://` 會讓 kist 自己 spawn `rclone serve sftp --stdio`。
這些測試同時驗證 rclone 的擴充「宣稱支援但實際行為」——`put_if_absent` 走暫存檔的 O_EXCL
fallback、stat 與 posix-rename（rclone 不實做 hardlink 與 O_EXCL 建檔，寬鬆語意的理由見
`docs/decisions/014-rclone-bridge.md`；`Backend` 沒有覆寫的 API）。

## FUSE 掛載

真實掛載測試（`crates/kist-cli/tests/cli.rs` 的 `mount_round_trip`：起 `kist mount`
子程序、經 kernel 讀、SIGTERM 卸載）預設略過，需要 /dev/fuse + fusermount3 並設
環境變數：

```sh
eval "$(sh tests/fuse-setup.sh)"    # 確認 /dev/fuse 與 fusermount3，export KIST_TEST_FUSE=1
cargo test --workspace
```

`crates/kist-mount` 的其他測試（純邏輯與 v4 的 `FsCore` 對真 repo 的讀取，`tests/v4_fs_read.rs`）不需要
FUSE，`cargo test --workspace` 就會跑。

## 驗收

見 `tests/acceptance/README.md`。

## GC 模型（M12）

`crates/kist-gcmodel` 是 v4 GC 協定（format-v4.md §11）的可執行模型（ADR 026 實作第一段）：確定情境
（`tests/scenarios.rs`）、三種設定的隨機交錯（`tests/random.rs`，各 512 case 進 `cargo test`；5000 case 的深度版與
探索工具標成 ignored）、兩輪外部審查的測試（`tests/review.rs`、`tests/review2.rs`）與突變搜尋固定下來的交錯
（`tests/found.rs`）。v3 的競態測試 `gc_race.rs` 已隨 v3 移除。
