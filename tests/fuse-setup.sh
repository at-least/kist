#!/bin/sh
# FUSE 掛載測試的前置：確認 /dev/fuse 與 fusermount3 在，並 export KIST_TEST_FUSE=1。
#
#   eval "$(sh tests/fuse-setup.sh)"
#   cargo test --workspace
set -e
[ -e /dev/fuse ] || { echo "/dev/fuse not available" >&2; exit 1; }
command -v fusermount3 >/dev/null || { echo "fusermount3 not found" >&2; exit 1; }
fusermount3 --version
echo "export KIST_TEST_FUSE=1"
