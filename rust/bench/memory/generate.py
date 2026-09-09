#!/usr/bin/env python3
"""Seed 產生 kist 記憶體量測用的測試集（100 萬檔）。

用法：generate.py <目標目錄> <layout: a|b> [seed] [dirs per_dir]
- layout a：dirs 個目錄 × 各 per_dir 檔（預設 1000×1000）
- layout b：單一平面目錄塞 per_dir 檔（預設 1,000,000）
- seed：內容種子（同 seed 重跑產生 byte-identical 的測試集）

檔案大小 768–1279 bytes（avg ≈ 1 KiB，全部單一 chunk → ~1M 個 chunk），
內容為 sha256 鏈（不可壓縮、每檔唯一 → 去重不會塌縮）。
結尾附邊界檔案：10 個空檔、1 個 symlink、1 對 hardlink、1 個 unicode 檔名、
1 個 40 層深路徑、1 個 5 MiB 檔（多 chunk → 間接清單）。
"""
import hashlib
import os
import sys


def file_content(name: bytes, size: int, seed: bytes) -> bytes:
    h = hashlib.sha256(seed + name).digest()
    out = bytearray()
    while len(out) < size:
        out += h
        h = hashlib.sha256(h).digest()
    return bytes(out[:size])


def main() -> None:
    root = sys.argv[1]
    layout = sys.argv[2]
    seed = (sys.argv[3] if len(sys.argv) > 3 else "kist-memtest-1").encode()
    dirs_spec = int(sys.argv[4]) if len(sys.argv) > 4 else 1000
    per_dir = int(sys.argv[5]) if len(sys.argv) > 5 else 1000
    n_files = dirs_spec * per_dir
    if layout == "a":
        dirs = dirs_spec
    elif layout == "b":
        dirs, per_dir = 1, n_files
    else:
        sys.exit(f"unknown layout {layout!r} (a|b)")
    if os.path.exists(root):
        sys.exit(f"{root} already exists; refuse to mix sets")
    os.makedirs(root)
    for d in range(dirs):
        dirpath = os.path.join(root, f"d{d:04d}") if dirs > 1 else root
        # layout b 時 dirpath 就是 root：exist_ok，root 已在上面建過
        os.makedirs(dirpath, exist_ok=True)
        for j in range(per_dir):
            i = d * per_dir + j
            name = f"f{i:07d}.dat".encode()
            size = 768 + (i * 7919) % 512
            p = os.path.join(dirpath.encode(), name)
            fd = os.open(p, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o644)
            os.write(fd, file_content(name, size, seed))
            os.close(fd)
            if i % 100_000 == 0:
                print(f"{i} files...", flush=True)
    # 邊界檔案（數量少，不影響 chunk 統計）
    x = os.path.join(root, "extra")
    os.makedirs(x)
    for k in range(10):
        open(os.path.join(x, f"empty{k}"), "wb").close()
    with open(os.path.join(x, "big5m.bin"), "wb") as f:
        f.write(file_content(b"big5m.bin", 5 * 1024 * 1024, seed))
    os.symlink("big5m.bin", os.path.join(x, "link-to-big"))
    a = os.path.join(x, "hard-a.dat")
    with open(a, "wb") as f:
        f.write(file_content(b"hard-a.dat", 4096, seed))
    os.link(a, os.path.join(x, "hard-b.dat"))
    with open(os.path.join(x, "unicode-日本語-🎉.txt"), "wb") as f:
        f.write(b"unicode name\n")
    deep = os.path.join(x, *(["deep"] * 40))
    os.makedirs(deep)
    with open(os.path.join(deep, "leaf.txt"), "wb") as f:
        f.write(b"deep leaf\n")
    print("done")


if __name__ == "__main__":
    main()
