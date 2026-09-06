#!/bin/sh
# 量測一輪 backup 的記憶體：init + first + second。
# 用法：run-baseline.sh <kist-bin> <workdir> <setdir> <label> [--gate]
#   --gate：加上 MemoryMax=512M 的硬門檻跑法（advisor：kernel 會先回收
#   clean page cache，過了才算真的過）。預設為不限量的量測跑法。
# 產出：<workdir>/samples/<label>-{first,second}.{samples,result}
# <workdir> 的 repo-<label>、cache-<label> 每次重建（基線必須乾淨）。
set -eu

bin=$1; shift
work=$1; shift
setdir=$1; shift
label=$1; shift
gate=""
if [ "${1:-}" = "--gate" ]; then
    gate="--max-bytes 536870912"  # 512 MiB
    shift
fi

pw="$work/pw"
mkdir -p "$work" "$work/samples"
[ -f "$pw" ] || echo "bench" > "$pw"
rm -rf "$work/repo-$label" "$work/cache-$label" "$work/client-$label.id"

export KIST_PASSWORD_FILE="$pw"
export KIST_CLIENT_ID_FILE="$work/client-$label.id"
common="--repo $work/repo-$label --cache-dir $work/cache-$label"

run() { # run <runlabel> <cmd...>
    local rl=$1; shift
    echo "=== $(date -Is) $rl start"
    sh "$(dirname "$0")/run-measured.sh" "$rl" "$work/samples" $gate "$@"
    echo "=== $(date -Is) $rl rc=$?"
}

"$bin" init $common
run "$label-first"  "$bin" backup $common "$setdir"
run "$label-second" "$bin" backup $common "$setdir"
