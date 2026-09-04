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
帳號）。每個測試用隨機 prefix，同一個 bucket 可以重複跑。（CI workflow 只手動觸發；驗證都在本機跑。）

## 驗收

見 `tests/acceptance/README.md`。

## 競態測試（M3）

`crates/kist-core/tests/gc_race.rs` 用 proptest 隨機交錯兩台 client 的 backup（拆成 prepare / commit）、forget、prune 與時鐘推進，每一步驗 repo 一致。預設 24 個案例；`PROPTEST_CASES=200 cargo test -p kist-core --test gc_race` 跑更多。
