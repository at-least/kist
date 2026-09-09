#!/bin/sh
# darwin cross 連結用的 linker shim：修 cargo-zigbuild 0.23.4 + zig 0.16 對
# `-Wl,-exported_symbols_list <file>`（rustc 給 cdylib 的分離形式）的處理。
#
# 根因：cargo-zigbuild 為了支援 whole-archive 的 `-Wl,/path/lib.a`，會把所有
# 「-Wl, 開頭 + 指向存在檔案」的參數改寫成 positional 輸入檔並排到選項之後。
# rustc 對 darwin cdylib 傳的 symbols list 檔剛好中招：list 檔被挪走，
# lld 拿下一個選項（-dead_strip）當 list 檔名 →
#   error: unable to read exported symbols list '-dead_strip': FileNotFound
# 逗號形式 `-Wl,-exported_symbols_list,<file>` 不符合那條改寫規則，可以直通。
#
# 這裡把分離形式合併成逗號形式（ld 語意等價），再轉手給 cargo-zigbuild。
# 上游修好後，本檔與 dist.sh 裡的兩行 CARGO_TARGET_*_LINKER 可一起刪除。
set -eu

n=$#
i=0
hold=""
while [ "$i" -lt "$n" ]; do
    arg=$1
    shift
    i=$((i + 1))
    if [ -n "$hold" ]; then
        case "$arg" in
        -Wl,*) set -- "$@" "-Wl,-exported_symbols_list,${arg#-Wl,}" ;;
        *) set -- "$@" "-Wl,-exported_symbols_list" "$arg" ;;
        esac
        hold=""
    elif [ "$arg" = "-Wl,-exported_symbols_list" ]; then
        hold=1
    else
        set -- "$@" "$arg"
    fi
done
if [ -n "$hold" ]; then
    set -- "$@" "-Wl,-exported_symbols_list"
fi

# rustc 的連結參數帶 `-arch arm64|x86_64`；zig cc 要完整的 -target。
target=""
prev=""
for a in "$@"; do
    if [ "$prev" = "-arch" ]; then
        case "$a" in
        arm64) target="aarch64-macos-none" ;;
        x86_64) target="x86_64-macos-none" ;;
        esac
    fi
    prev=$a
done
if [ -z "$target" ]; then
    echo "zigcc-apple.sh: no -arch in linker args; cannot derive zig -target" >&2
    exit 1
fi

exec cargo-zigbuild zig cc -- -target "$target" "$@"
