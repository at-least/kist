#!/bin/sh
# 遠端來源（sftp:// 或 s3://）的 backup 記憶體（M12 第 7 段之三）：init + first + second，進一個新的本機 repo。
# 用法：run-remote.sh <kist-bin> <workdir> <source url> <label> [--gate]
#   --gate：加上 MemoryMax=512M 的硬門檻跑法（同 run-baseline.sh）。預設為不限量的量測跑法。
# 來源的連線設定從環境來：SFTP 是 KIST_SFTP_PASSWORD（或 key）與 KIST_SFTP_KNOWN_HOSTS，S3 是 AWS_*。
# 產出：<workdir>/samples/<label>-{first,second}.{samples,result}（--gate：gate-<label>-…）
set -eu

bin=$1; shift
work=$1; shift
url=$1; shift
label=$1; shift
gate=""
out="$label"
if [ "${1:-}" = "--gate" ]; then
    gate="--max-bytes 536870912"  # 512 MiB
    out="gate-$label"
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
    rl=$1; shift
    echo "=== $(date -Is) $rl start"
    rc=0
    sh "$(dirname "$0")/run-measured.sh" "$rl" "$work/samples" $gate "$@" || rc=$?
    echo "=== $(date -Is) $rl rc=$rc"
}

"$bin" init $common
run "$out-first"  "$bin" backup $common "$url"
run "$out-second" "$bin" backup $common "$url"
