#!/bin/sh
# 從這台 Linux 主機產出全部發佈二進位到 dist/。
#
# 產物（每個壓縮檔含 binary + LICENSE 兩份）：
#   dist/kist-v<VERSION>-<target>.tar.gz   linux-musl ×2 / darwin ×2
#   dist/kist-v<VERSION>-x86_64-pc-windows-gnu.zip
#   dist/SHA256SUMS
#
# darwin 需要 macOS SDK（zig ≥0.14 不再附 framework tbd，而 rustls-platform-verifier
# 在 macOS 要 Security/CoreFoundation framework）：放到
#   ~/.cache/macos-sdk/MacOSX11.3.sdk
# 或用 KIST_MACOS_SDK 指定。取法見 docs/decisions/012-release-binaries.md。
# darwin 連結另需 scripts/zigcc-apple.sh（見該檔頭註解）。
set -eu

cd "$(dirname "$0")"
VERSION=$(grep -m1 '^version = ' Cargo.toml | cut -d'"' -f2)
OUT=dist
rm -rf "$OUT"
mkdir -p "$OUT"

SDK="${KIST_MACOS_SDK:-$HOME/.cache/macos-sdk/MacOSX11.3.sdk}"
SHIM="$(pwd)/scripts/zigcc-apple.sh"

package() { # package <target> <fmt> <binname>
    _stage="$OUT/kist-v$VERSION-$1"
    rm -rf "$_stage"
    mkdir -p "$_stage"
    cp "target/$1/release/$3" "$_stage/$3"
    chmod +x "$_stage/$3"
    cp LICENSE-MIT LICENSE-APACHE "$_stage/"
    if [ "$2" = zip ]; then
        (cd "$OUT" && bsdtar -a -cf "kist-v$VERSION-$1.zip" "kist-v$VERSION-$1")
    else
        tar -C "$OUT" -czf "$OUT/kist-v$VERSION-$1.tar.gz" "kist-v$VERSION-$1"
    fi
    rm -rf "$_stage"
}

for t in \
    x86_64-unknown-linux-musl \
    aarch64-unknown-linux-musl \
    aarch64-apple-darwin \
    x86_64-apple-darwin \
    x86_64-pc-windows-gnu; do
    unset SDKROOT CARGO_TARGET_AARCH64_APPLE_DARWIN_LINKER \
        CARGO_TARGET_X86_64_APPLE_DARWIN_LINKER
    case "$t" in
    *windows-*)
        fmt=zip; binname=kist.exe ;;
    *apple-darwin*)
        fmt=tar; binname=kist
        if [ ! -d "$SDK" ]; then
            echo "darwin target $t 需要 macOS SDK：找不到 $SDK" >&2
            echo "下載 phracker/MacOSX-SDKs 的 MacOSX11.3.sdk.tar.xz 解開到該路徑，" >&2
            echo "或以 KIST_MACOS_SDK=<sdk 路徑> 執行本腳本。" >&2
            exit 1
        fi
        export SDKROOT="$SDK"
        case "$t" in
        aarch64-*) export CARGO_TARGET_AARCH64_APPLE_DARWIN_LINKER="$SHIM" ;;
        *) export CARGO_TARGET_X86_64_APPLE_DARWIN_LINKER="$SHIM" ;;
        esac ;;
    *)
        fmt=tar; binname=kist ;;
    esac

    echo "== build $t"
    cargo zigbuild --release --target "$t" -p kist-cli
    package "$t" "$fmt" "$binname"
done

( cd "$OUT" && sha256sum kist-v* > SHA256SUMS )
echo
ls -la "$OUT"
echo "SHA256SUMS:"
cat "$OUT/SHA256SUMS"
