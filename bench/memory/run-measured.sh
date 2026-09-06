#!/bin/sh
# 在 systemd-run --user scope 裡跑一個指令並取樣記憶體。
#
# 用法：run-measured.sh <label> <outdir> [--max-bytes N] <cmd> [args...]
#   --max-bytes N：加 MemoryMax=N MemorySwapMax=0（硬門檻跑法）；
#                  不給 = 不限量（量測跑法）。
#
# 產出：
#   <outdir>/<label>.samples   每行「memory.current file_page_cache」（bytes，~50ms 一次）
#   <outdir>/<label>.result    exit_code、sampled_max_ex_file、memory.peak、oom 摘要
#
# 取樣值 = memory.current − memory.stat 的 file 欄：把可回收的檔案 page cache
# 排除在報告數字外（advisor 2026-09-06：本機後端的 pack 寫入 page cache 會被
# cgroup 計費，但那不是產品的常駐記憶體）。真正的驗收是 --max-bytes 跑法
# 不被 OOM 殺掉——kernel 會先回收 clean page cache，所以過了就是真的過了。
set -eu

label=$1; shift
outdir=$1; shift
max_bytes=""
if [ "${1:-}" = "--max-bytes" ]; then
    max_bytes=$2
    shift 2
fi
mkdir -p "$outdir"

props=""
[ -n "$max_bytes" ] && props="-p MemoryMax=$max_bytes -p MemorySwapMax=0"

inner=$(mktemp /tmp/kist-measured-inner.XXXXXX.sh)
trap 'rm -f "$inner"' EXIT
cat > "$inner" <<'INNER'
#!/bin/sh
set -u
cg=$(sed -n 's/^0:://p' /proc/self/cgroup)
base="/sys/fs/cgroup$cg"
samples=$1; shift
sample() {
    cur=$(cat "$base/memory.current" 2>/dev/null) || return
    f=$(awk '$1=="file"{print $2}' "$base/memory.stat" 2>/dev/null) || return
    slab=$(awk '$1=="slab_reclaimable"{print $2}' "$base/memory.stat" 2>/dev/null) || return
    echo "$cur $f $slab" >> "$samples"
}
sample  # 指令太快結束時，背景取樣器可能一筆都來不及寫
(
    while :; do
        sample
        sleep 0.05
    done
) &
sampler=$!
"$@"
rc=$?
kill "$sampler" 2>/dev/null || true
wait "$sampler" 2>/dev/null || true
sample
# scope 隨指令結束就拆掉，peak / OOM 計數要在結束前自己留下
cat "$base/memory.peak" > "$samples.peak" 2>/dev/null || : > "$samples.peak"
cat "$base/memory.events" > "$samples.events" 2>/dev/null || : > "$samples.events"
exit $rc
INNER
chmod +x "$inner"

# shellcheck disable=SC2086
systemd-run --user --scope -q $props "$inner" "$outdir/$label.samples" "$@"
rc=$?
sampled=$(awk '{d=$1-$2; if(d>m)m=d} END{print m+0}' "$outdir/$label.samples")
sampled_ex_slab=$(awk '{d=$1-$2-$3; if(d>m)m=d} END{print m+0}' "$outdir/$label.samples")
peak=$(cat "$outdir/$label.samples.peak")
oom=$(awk '$1=="oom_kill"{print $2}' "$outdir/$label.samples.events")
{
    echo "label=$label"
    echo "max_bytes=${max_bytes:-none}"
    echo "exit_code=$rc"
    echo "sampled_max_ex_file=$sampled"
    echo "sampled_max_ex_file_slab=$sampled_ex_slab"
    echo "memory_peak_total=$peak"
    echo "memory_peak_last_seen=$peak"
    echo "oom_kill_last_seen=$oom"
} | tee "$outdir/$label.result"
exit "$rc"
