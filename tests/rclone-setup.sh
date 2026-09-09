#!/bin/sh
# rclone 橋接測試的前置：確認 rclone 在 PATH 上，並輸出要 export 的環境變數。
#
#   eval "$(sh tests/rclone-setup.sh)"
#   cargo test --workspace
#
# rclone 模式不需要起伺服器：kist 會自己 spawn `rclone serve sftp --stdio <source>`，
# SFTP 走子程序的 stdin/stdout。要手動驗「人肉跑一個 rclone SFTP 伺服器」的場景
# （sftp:// 嚴格模式會明確拒絕它），可以另外開：
#
#   rclone serve sftp --addr 127.0.0.1:2022 --user kist --pass kistpass /srv/backups
set -e
command -v rclone >/dev/null || {
    echo "rclone not found in PATH (or set KIST_RCLONE_BIN)" >&2
    exit 1
}
rclone version | head -1
echo "export KIST_TEST_RCLONE=1"
