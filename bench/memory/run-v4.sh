#!/bin/sh
# M12 第 7 段的整套記憶體量測：兩個測試集各做 init + first + second backup（量測跑法，再門檻跑法）、
# prune ×2（量測）與 prune ×1（門檻）、check／check --read-data／restore（量測，再門檻）。
# 門檻跑法被 OOM 殺掉只記錄（.result 的 exit_code 與 oom_kill），不中斷。全部的 .result 印在最後。
# 用法：run-v4.sh <kist-bin> <workdir> <set-a> <set-b>
set -u
bin=$1; work=$2; a=$3; b=$4
here=$(dirname "$0")
for pair in "A $a" "B $b"; do
    set -- $pair
    label=$1; setdir=$2
    sh "$here/run-baseline.sh" "$bin" "$work" "$setdir" "$label"
    sh "$here/run-baseline.sh" "$bin" "$work" "$setdir" "$label" --gate
    sh "$here/run-prune.sh" "$bin" "$work" "$label" 2
    sh "$here/run-prune.sh" "$bin" "$work" "$label" 1 --gate
    sh "$here/run-read.sh" "$bin" "$work" "$label"
    sh "$here/run-read.sh" "$bin" "$work" "$label" --gate
done
echo "=== results"
for r in "$work"/samples/*.result; do
    echo "--- $r"; cat "$r"
done
