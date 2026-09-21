# 整合測試

## S3 / MinIO

S3 相關的測試（`crates/kist-backend/tests/s3.rs`、`crates/kist-core/tests/s3_end_to_end.rs`、
`crates/kist-cli/tests/cli.rs` 裡的 `s3_*`）預設略過，設了環境變數才會跑：

```sh
eval "$(sh tests/minio-setup.sh)"   # 起容器、建 bucket、建 Put-only 使用者，並設好環境變數
cargo test --workspace
docker rm -f kist-minio             # 用完關掉
```

`tests/minio-setup.sh` 會印出要 export 的環境變數（root 帳號、Put/Get/List-only 的 `kistbackup`
帳號）。每個測試用隨機 prefix，同一個 bucket 可以重複跑。（兩個 CI workflow
都只手動觸發；Go 端 gate 平時以本機 `cd go && make verify` 為準。）

## SFTP

SFTP 相關的測試（`crates/kist-backend/tests/sftp.rs`、`contract.rs` 裡的
`sftp_backend_meets_the_contract`）預設略過，設了環境變數才會跑：

```sh
eval "$(sh tests/sftp-setup.sh)"    # 起 atmoz/sftp 容器、keyscan 產 known_hosts、備 key 認證素材
cargo test --workspace
docker rm -f kist-sftp              # 用完關掉
```

`tests/sftp-setup.sh` 會印出要 export 的環境變數：`KIST_TEST_SFTP_URL`（含隨機 port 的
`kisttest` 帳號）、密碼、known_hosts 路徑，以及帶 passphrase 的 ed25519 key（測
`KIST_SFTP_KEY` / `KIST_SFTP_KEY_PASSPHRASE` 認證路徑）。容器的 OpenSSH sftp-server
支援 kist 需要的 `hardlink@openssh.com` / `posix-rename@openssh.com` 擴充。

## rclone 橋接

rclone 相關的測試（`crates/kist-backend/tests/rclone.rs`、`rclone_bin.rs`）預設略過，
需要 PATH 上有 rclone 並設環境變數：

```sh
eval "$(sh tests/rclone-setup.sh)"  # 只是確認 rclone 在 PATH 上並 export KIST_TEST_RCLONE=1
cargo test --workspace
```

stdio 模式不用起伺服器：`rclone://` 會讓 kist 自己 spawn `rclone serve sftp --stdio`。
這些測試同時驗證 rclone 的擴充「宣稱支援但實際行為」——`put` 走 O_EXCL fallback +
posix-rename、`put_if_absent` 走 stat + posix-rename（rclone 不實做 hardlink 與
O_EXCL 建檔，寬鬆語意的理由見 `docs/decisions/014-rclone-bridge.md`）。

## FUSE 掛載

真實掛載測試（`crates/kist-cli/tests/cli.rs` 的 `mount_round_trip`：起 `kist mount`
子程序、經 kernel 讀、SIGTERM 卸載）預設略過，需要 /dev/fuse + fusermount3 並設
環境變數：

```sh
eval "$(sh tests/fuse-setup.sh)"    # 確認 /dev/fuse 與 fusermount3，export KIST_TEST_FUSE=1
cargo test --workspace
```

`crates/kist-mount` 的其他測試（純邏輯與 `FsCore` 對真 repo 的讀取）不需要 FUSE，
`cargo test --workspace` 就會跑。

## 驗收

見 `tests/acceptance/README.md`。

## 競態測試（M3）

`crates/kist-core/tests/gc_race.rs` 用 proptest 隨機交錯兩台 client 的 backup（拆成 prepare / commit）、forget、prune 與時鐘推進，每一步驗 repo 一致。預設 24 個案例；`PROPTEST_CASES=200 cargo test -p kist-core --test gc_race` 跑更多。
