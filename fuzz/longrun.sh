#!/bin/sh
# fuzz 長跑驅動（用法：fuzz/longrun.sh <每 target 秒數> [target ...]）。
#
# 依序對每個 target 呼叫 smoke.sh——環境坑、corpus 順序、seeds 防呆都在
# 那裡處理——這裡只補長跑需要的：逐 target 時間戳與結算（成敗、artifact
# 有無），全部跑完後給一行總結，方便脫離 session 跑完回來只看結尾：
#
#   nohup sh fuzz/longrun.sh 86400 > /tmp/kist-fuzz-24h.log 2>&1 < /dev/null &
#
# CPU 讓路內建：每個 target 以 nice 19 起（sh → cargo → rustc → libFuzzer
# 全樹繼承），忙時讓給日常操作、閒時全速。-max_total_time 是掛鐘時間，被
# 搶時每小時 exec 數會下降——這是取捨不是失效。KIST_FUZZ_NICE=<n> 可覆寫
# （0–19，0 = 關閉讓路；未設/留空 = 19）。讓路效果依核心排程設定，本機
# 2026-09-10 同核心爭搶實測 nice19 只拿到 0.6%（協議見 fuzz/README.md）。
#
# 結束碼 0 = 全部 target 乾淨（無 crash 且 seeds 未被寫入）。
set -u
cd "$(dirname "$0")/.." || exit 1

secs="${1:?usage: fuzz/longrun.sh <seconds-per-target> [target ...]}"
[ $# -gt 0 ] && shift
targets="${*:-pack cbor chunker parity}"

nice_level="${KIST_FUZZ_NICE:-19}"
case "$nice_level" in
    *[!0-9]*)
        echo "longrun: KIST_FUZZ_NICE 必須是 0–19（0 = 關閉讓路），現在是「$nice_level」" >&2
        exit 2
        ;;
esac
if [ "$nice_level" -gt 19 ]; then
    echo "longrun: KIST_FUZZ_NICE 必須 ≤ 19，現在是 $nice_level" >&2
    exit 2
fi
# 包在 smoke.sh 外層，nice 對整棵 process tree 生效。
run_smoke() {
    if [ "$nice_level" -gt 0 ]; then
        nice -n "$nice_level" sh fuzz/smoke.sh "$@"
    else
        sh fuzz/smoke.sh "$@"
    fi
}
if [ "$nice_level" -gt 0 ]; then
    echo "讓路：nice $nice_level（KIST_FUZZ_NICE=0 可關閉）"
else
    echo "讓路：關閉（KIST_FUZZ_NICE=<1-19> 可開啟）"
fi

fail=0
for t in $targets; do
    echo "=== $(date -Is) start $t (${secs}s) ==="
    if run_smoke "$t" "$secs"; then
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
