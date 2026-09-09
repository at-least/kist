#!/usr/bin/env sh
# 起一個 MinIO 容器給整合測試用：建 bucket、建一個「只能 Put/Get/List、不能 Delete」的使用者。
# 本機：`eval "$(sh tests/minio-setup.sh)"`。CI 也用這支。
# 影像釘在本機驗證過的版本（mc admin policy create 的語法在舊版不同）。
set -eu
NAME=${MINIO_CONTAINER:-kist-minio}
PORT=${MINIO_PORT:-19000}
ROOT_USER=kistadmin
ROOT_PASS=kistsecret123
BUCKET=kist-test
PUTONLY_USER=kistbackup
PUTONLY_PASS=kistbackupsecret

docker rm -f "$NAME" >/dev/null 2>&1 || true
docker run -d --name "$NAME" -p "127.0.0.1:$PORT:9000" \
  -e MINIO_ROOT_USER=$ROOT_USER -e MINIO_ROOT_PASSWORD=$ROOT_PASS \
  minio/minio:RELEASE.2025-09-07T16-13-09Z server /data >/dev/null
i=0
until curl -sf "http://127.0.0.1:$PORT/minio/health/live" >/dev/null; do
  i=$((i+1)); [ $i -gt 60 ] && { echo "minio did not start" >&2; exit 1; }; sleep 1
done

# backup 需要的最小權限：PutObject + GetObject + ListBucket，沒有 DeleteObject。
# （S3 沒有「只能 Put」還能開 repo 的組合：開 repo 要讀 config 與 index，見 docs/format.md §7。）
POLICY='{"Version":"2012-10-17","Statement":[
  {"Effect":"Allow","Action":["s3:PutObject","s3:GetObject"],"Resource":["arn:aws:s3:::'"$BUCKET"'/*"]},
  {"Effect":"Allow","Action":["s3:ListBucket","s3:GetBucketLocation"],"Resource":["arn:aws:s3:::'"$BUCKET"'"]}
]}'
docker exec "$NAME" sh -c "
  mc alias set local http://127.0.0.1:9000 $ROOT_USER $ROOT_PASS >/dev/null &&
  mc mb -p local/$BUCKET >/dev/null &&
  printf '%s' '$POLICY' > /tmp/putonly.json &&
  mc admin policy create local kist-putonly /tmp/putonly.json >/dev/null &&
  mc admin user add local $PUTONLY_USER $PUTONLY_PASS >/dev/null &&
  mc admin policy attach local kist-putonly --user $PUTONLY_USER >/dev/null
"
cat <<ENV
export KIST_TEST_S3_ENDPOINT=http://127.0.0.1:$PORT
export KIST_TEST_S3_BUCKET=$BUCKET
export AWS_ACCESS_KEY_ID=$ROOT_USER
export AWS_SECRET_ACCESS_KEY=$ROOT_PASS
export KIST_TEST_S3_PUTONLY_KEY=$PUTONLY_USER
export KIST_TEST_S3_PUTONLY_SECRET=$PUTONLY_PASS
ENV
