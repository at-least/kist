# 整合測試

## S3 / MinIO

S3 相關的測試（`crates/kist-backend/tests/s3.rs`、`crates/kist-core/tests/s3_end_to_end.rs`、
`crates/kist-cli/tests/cli.rs` 裡的 `s3_*`）預設略過，設了環境變數才會跑：

```sh
docker run -d --name kist-minio -p 127.0.0.1:19000:9000 \
  -e MINIO_ROOT_USER=kistadmin -e MINIO_ROOT_PASSWORD=kistsecret123 \
  minio/minio:latest server /data
docker exec kist-minio sh -c 'mc alias set local http://127.0.0.1:9000 kistadmin kistsecret123 && mc mb -p local/kist-test'

export KIST_TEST_S3_ENDPOINT=http://127.0.0.1:19000 KIST_TEST_S3_BUCKET=kist-test
export AWS_ACCESS_KEY_ID=kistadmin AWS_SECRET_ACCESS_KEY=kistsecret123
cargo test --workspace
```

每個測試用隨機 prefix，同一個 bucket 可以重複跑。CI 在 ubuntu 上用同樣的方式起 MinIO。

## 驗收

見 `tests/acceptance/README.md`。
