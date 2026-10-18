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
# 依 client 分前綴的備份憑證（format-v4.md §1 建議的部署）：只能在自己 client 的前綴下建立物件，讀全部。
CLIENT_USER=kistclient
CLIENT_PASS=kistclientsecret
CLIENT_ID=c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1

docker rm -f "$NAME" >/dev/null 2>&1 || true
docker run -d --name "$NAME" -p "127.0.0.1:$PORT:9000" \
  -e MINIO_ROOT_USER=$ROOT_USER -e MINIO_ROOT_PASSWORD=$ROOT_PASS \
  minio/minio:RELEASE.2025-09-07T16-13-09Z server /data >/dev/null
i=0
until curl -sf "http://127.0.0.1:$PORT/minio/health/live" >/dev/null; do
  i=$((i+1)); [ $i -gt 60 ] && { echo "minio did not start" >&2; exit 1; }; sleep 1
done

# backup 需要的最小權限：PutObject + GetObject + ListBucket，沒有 DeleteObject。
# （S3 沒有「只能 Put」還能開 repo 的組合：開 repo 要讀 config、金鑰槽與 head，見 docs/format-v4.md §1。）
POLICY='{"Version":"2012-10-17","Statement":[
  {"Effect":"Allow","Action":["s3:PutObject","s3:GetObject"],"Resource":["arn:aws:s3:::'"$BUCKET"'/*"]},
  {"Effect":"Allow","Action":["s3:ListBucket","s3:GetBucketLocation"],"Resource":["arn:aws:s3:::'"$BUCKET"'"]}
]}'
# v4 的每台備份主機：只能在 <repo 前綴>/{packs,mpacks,parity,deltas,snapshots}/<client id>/ 底下建立物件
# （repo 前綴以 * 代替：測試每次用隨機前綴），讀全部、列 bucket；沒有 Delete，也寫不了 config、keys/、maint/
# 與別台 client 的前綴。
CLIENT_POLICY='{"Version":"2012-10-17","Statement":[
  {"Effect":"Allow","Action":["s3:PutObject"],"Resource":[
    "arn:aws:s3:::'"$BUCKET"'/*/packs/'"$CLIENT_ID"'/*",
    "arn:aws:s3:::'"$BUCKET"'/*/mpacks/'"$CLIENT_ID"'/*",
    "arn:aws:s3:::'"$BUCKET"'/*/parity/'"$CLIENT_ID"'/*",
    "arn:aws:s3:::'"$BUCKET"'/*/deltas/'"$CLIENT_ID"'/*",
    "arn:aws:s3:::'"$BUCKET"'/*/snapshots/'"$CLIENT_ID"'/*"]},
  {"Effect":"Allow","Action":["s3:GetObject"],"Resource":["arn:aws:s3:::'"$BUCKET"'/*"]},
  {"Effect":"Allow","Action":["s3:ListBucket","s3:GetBucketLocation"],"Resource":["arn:aws:s3:::'"$BUCKET"'"]}
]}'
# 強制條件寫入（format-v4.md §1 的 `s3:if-none-match`，AWS 的 Null 條件）：MinIO RELEASE.2025-09-07 不認得這個
# 條件 key（`invalid condition key`），政策建不起來就不建這個使用者、不 export KIST_TEST_S3_COND_*，測試略過；
# 換成支援的 S3 時測試就會跑。
COND_USER=kistcond
COND_PASS=kistcondsecret
COND_POLICY='{"Version":"2012-10-17","Statement":[
  {"Effect":"Allow","Action":["s3:PutObject"],"Resource":["arn:aws:s3:::'"$BUCKET"'/*"],
   "Condition":{"Null":{"s3:if-none-match":"false"}}},
  {"Effect":"Allow","Action":["s3:GetObject"],"Resource":["arn:aws:s3:::'"$BUCKET"'/*"]},
  {"Effect":"Allow","Action":["s3:ListBucket","s3:GetBucketLocation"],"Resource":["arn:aws:s3:::'"$BUCKET"'"]}
]}'
docker exec "$NAME" sh -c "
  mc alias set local http://127.0.0.1:9000 $ROOT_USER $ROOT_PASS >/dev/null &&
  mc mb -p local/$BUCKET >/dev/null &&
  printf '%s' '$POLICY' > /tmp/putonly.json &&
  mc admin policy create local kist-putonly /tmp/putonly.json >/dev/null &&
  mc admin user add local $PUTONLY_USER $PUTONLY_PASS >/dev/null &&
  mc admin policy attach local kist-putonly --user $PUTONLY_USER >/dev/null &&
  printf '%s' '$CLIENT_POLICY' > /tmp/client.json &&
  mc admin policy create local kist-client /tmp/client.json >/dev/null &&
  mc admin user add local $CLIENT_USER $CLIENT_PASS >/dev/null &&
  mc admin policy attach local kist-client --user $CLIENT_USER >/dev/null
"
COND=""
if docker exec "$NAME" sh -c "
  printf '%s' '$COND_POLICY' > /tmp/cond.json &&
  mc admin policy create local kist-cond /tmp/cond.json >/dev/null 2>&1 &&
  mc admin user add local $COND_USER $COND_PASS >/dev/null &&
  mc admin policy attach local kist-cond --user $COND_USER >/dev/null
"; then
  COND="export KIST_TEST_S3_COND_KEY=$COND_USER
export KIST_TEST_S3_COND_SECRET=$COND_PASS"
else
  echo "# note: this MinIO rejects the s3:if-none-match policy condition; the conditional-write test is skipped" >&2
fi
cat <<ENV
export KIST_TEST_S3_ENDPOINT=http://127.0.0.1:$PORT
export KIST_TEST_S3_BUCKET=$BUCKET
export AWS_ACCESS_KEY_ID=$ROOT_USER
export AWS_SECRET_ACCESS_KEY=$ROOT_PASS
export KIST_TEST_S3_PUTONLY_KEY=$PUTONLY_USER
export KIST_TEST_S3_PUTONLY_SECRET=$PUTONLY_PASS
export KIST_TEST_S3_CLIENT_KEY=$CLIENT_USER
export KIST_TEST_S3_CLIENT_SECRET=$CLIENT_PASS
export KIST_TEST_S3_CLIENT_ID=$CLIENT_ID
$COND
ENV
