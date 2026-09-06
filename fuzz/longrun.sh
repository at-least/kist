#!/bin/sh
# fuzz 長跑驅動（用法：fuzz/longrun.sh <每 target 秒數> [target ...]）。
#
# 依序對每個 target 呼叫 smoke.sh——環境坑、corpus 順序、seeds 防呆都在
# 那裡處理——這裡只補長跑需要的：逐 target 時間戳與結算（成敗、artifact
# 有無），全部跑完後給一行總結，方便脫離 session 跑完回來只看結尾：
#
#   nohup sh fuzz/longrun.sh 86400 > /tmp/kist-fuzz-24h.log 2>&1 < /dev/null &
#
# 結束碼 0 = 全部 target 乾淨（無 crash 且 seeds 未被寫入）。
set -u
cd "$(dirname "$0")/.."

secs="${1:?usage: fuzz/longrun.sh <seconds-per-target> [target ...]}"
[ $# -gt 0 ] && shift
targets="${*:-pack cbor chunker parity}"

fail=0
for t in $targets; do
    echo "=== $(date -Is) start $t (${secs}s) ==="
    if sh fuzz/smoke.sh "$t" "$secs"; then
        echo "=== $(date -Is) OK $t ==="
    else
        echo "=== $(date -Is) FAIL $t（看上方 libFuzzer 輸出與 fuzz/artifacts/$t/）==="
        fail=1
    fi
    if [ -n "$(ls fuzz/artifacts/"$t" 2>/dev/null)" ]; then
        echo "=== $(date -Is) $t 有 artifact，要 triage："
        ls -l "fuzz/artifacts/$t"
        fail=1
    fi
done
echo "=== $(date -Is) longrun 結束 fail=$fail ==="
exit "$fail"
