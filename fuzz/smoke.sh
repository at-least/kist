#!/bin/sh
# fuzz 的煙霧／長跑共用入口（用法：fuzz/smoke.sh <target> <秒數>）。
#
# 封裝兩件事：
# 1. 環境坑（見 README「本機環境備忘」）：sccache wrapper 會撿 stable rustc
#    編 nightly 參數 → 關掉；rustup proxy 被 sandbox 的 argv[0] 弄壞 →
#    nightly bin 直接放 PATH 最前（只在本機有該路徑時，CI 裝好 nightly
#    就不需要）。
# 2. corpus 順序防呆：libFuzzer 把新單元寫進**第一個** corpus 目錄，
#    順序錯了會汙染進版控的 seeds——這裡固定 corpus 在前，收尾再驗一次
#    seeds 沒被動到。
set -eu
cd "$(dirname "$0")/.."

target="${1:?usage: fuzz/smoke.sh <target> <seconds>}"
secs="${2:-300}"

nightly_bin="$HOME/.rustup/toolchains/nightly-x86_64-unknown-linux-gnu/bin"
if [ -d "$nightly_bin" ]; then
    PATH="$HOME/.cargo/bin:$nightly_bin:$PATH"
    export PATH
fi
CARGO_BUILD_RUSTC_WRAPPER=
export CARGO_BUILD_RUSTC_WRAPPER

mkdir -p "fuzz/corpus/$target"
cargo fuzz run "$target" "fuzz/corpus/$target" "fuzz/seeds/$target" -- -max_total_time="$secs"

if [ -n "$(git status --porcelain -- fuzz/seeds)" ]; then
    echo "ERROR: fuzz/seeds 被這次跑動寫入了（corpus/seeds 順序不該顛倒）：" >&2
    git status --short -- fuzz/seeds >&2
    exit 1
fi
