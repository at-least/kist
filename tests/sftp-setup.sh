#!/usr/bin/env sh
# 起一個 OpenSSH sftp-server 容器（atmoz/sftp）給 SFTP 整合測試用，並備好三樣測試素材：
#   1. 密碼認證的使用者（kisttest / kisttestpass）
#   2. known_hosts（ssh-keyscan 抓容器 host key；kist 嚴格驗證，沒有它連不上）
#   3. 一組帶 passphrase 的 ed25519 key（測 key 認證；公鑰用官方機制掛進 .ssh/keys/，
#      由 entrypoint 開機時併進 authorized_keys——docker cp 事後寫會有 owner 問題）
# 本機：`eval "$(sh tests/sftp-setup.sh)"`。CI 也用這支。
# kist 需要伺服器支援 hardlink@openssh.com / posix-rename@openssh.com，OpenSSH 都有。
set -eu
NAME=${SFTP_CONTAINER:-kist-sftp}
PORT=${SFTP_PORT:-19222}
USER_NAME=kisttest
PASS=kisttestpass
KEY_PASS=kistkeypass
WORK=$(mktemp -d)

ssh-keygen -t ed25519 -N "$KEY_PASS" -f "$WORK/id_ed25519" -C kist-test -q

docker rm -f "$NAME" >/dev/null 2>&1 || true
docker run -d --name "$NAME" -p "127.0.0.1:$PORT:22" \
  -v "$WORK/id_ed25519.pub:/home/$USER_NAME/.ssh/keys/0.pub:ro" \
  atmoz/sftp:alpine "$USER_NAME:$PASS:1001" >/dev/null

# 等 sshd 可連（keyscan 成功即代表握手層就緒）
i=0
until ssh-keyscan -p "$PORT" -T 2 "127.0.0.1" >"$WORK/known_hosts" 2>/dev/null \
  && grep -q "ssh-" "$WORK/known_hosts"; do
  i=$((i + 1)); [ $i -gt 30 ] && { echo "sftp did not start" >&2; exit 1; }; sleep 1
done

# 測試用 repo 根目錄（chroot 內 user 可寫；chroot 本身 root 擁有是 OpenSSH 的要求）
docker exec "$NAME" sh -c \
  "mkdir -p /home/$USER_NAME/repo && chown -R 1001:1001 /home/$USER_NAME/repo"

cat <<ENV
export KIST_TEST_SFTP_URL='sftp://$USER_NAME@127.0.0.1:$PORT/repo'
export KIST_TEST_SFTP_PASSWORD='$PASS'
export KIST_TEST_SFTP_KNOWN_HOSTS='$WORK/known_hosts'
export KIST_TEST_SFTP_KEY='$WORK/id_ed25519'
export KIST_TEST_SFTP_KEY_PASSPHRASE='$KEY_PASS'
ENV
