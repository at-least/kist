#!/bin/sh
# 量測讀取路徑的記憶體：check、check --read-data、restore（v4，M12 第 7 段）。
# 用法：run-read.sh <kist-bin> <workdir> <label> [--gate]
#   workdir 要已經有 run-baseline.sh 建好的 repo-<label> / cache-<label>。restore 的目標是
#   <workdir>/restore-<label>（每次清掉重建）。
# 產出：<workdir>/samples/{check,check-data,restore}-<label>.{samples,result}
set -eu

bin=$1; work=$2; label=$3; shift 3
gate=""
out="$label"
if [ "${1:-}" = "--gate" ]; then
    gate="--max-bytes 536870912"
    out="gate-$label"
    shift
fi

export KIST_PASSWORD_FILE="$work/pw"
common="--repo $work/repo-$label --cache-dir $work/cache-$label"
run() { # run <runlabel> <cmd...>
    rl=$1; shift
    echo "=== $(date -Is) $rl start"
    rc=0
    sh "$(dirname "$0")/run-measured.sh" "$rl" "$work/samples" $gate "$@" || rc=$?
    echo "=== $(date -Is) $rl rc=$rc"
}
run "check-$out" "$bin" check $common
run "check-data-$out" "$bin" check $common --read-data
rm -rf "$work/restore-$label"
run "restore-$out" "$bin" restore $common latest "$work/restore-$label"
