#!/bin/sh
# 量測 prune 的記憶體（多次，可選硬門檻）。v4 的 prune 沒有 dry run：每一輪讀全部 snapshot 與 pack 的 trailer、
# 寫一個世代（第一輪把全部 pack 收錄進 map；之後幾輪沒有到期的東西，只是重讀與重寫 head）。
# 用法：run-prune.sh <kist-bin> <workdir> <label> <n> [--gate]
#   workdir 要已經有 run-baseline.sh 建好的 repo-<label> / cache-<label> / client-<label>.id。
#   --gate：MemoryMax=512M + MemorySwapMax=0 的硬門檻跑法（驗收以此為準）；
#   產出檔名加 gate- 前缀與不加的區隔。
# 產出：<workdir>/samples/prune-<label>-<i>.{samples,result}
set -eu

bin=$1; work=$2; label=$3; n=$4; shift 4
gate=""
out="$label"
if [ "${1:-}" = "--gate" ]; then
    gate="--max-bytes 536870912"
    out="gate-$label"
    shift
fi

export KIST_PASSWORD_FILE="$work/pw"
export KIST_CLIENT_ID_FILE="$work/client-$label.id"
common="--repo $work/repo-$label --cache-dir $work/cache-$label"
i=1
while [ $i -le "$n" ]; do
    echo "=== $(date -Is) prune-$out-$i start"
    rc=0
    sh "$(dirname "$0")/run-measured.sh" "prune-$out-$i" "$work/samples" $gate \
        "$bin" prune $common || rc=$?
    echo "=== $(date -Is) prune-$out-$i rc=$rc"
    i=$((i + 1))
done
