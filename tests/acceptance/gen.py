"""產生驗收用測試集：10 萬個檔案、約 10 GiB。
- 99,000 個小檔（1–20 KiB，一半文字一半亂數）≈ 1 GiB
- 1,000 個大檔（約 9.4 MiB，一半可壓縮一半亂數）≈ 9.2 GiB
"""
import os, random, sys
root = sys.argv[1]
rng = random.Random(1234)
text = (b"The quick brown fox jumps over the lazy dog. " * 40)
small = 0
for d in range(100):
    for s in range(10):
        dpath = os.path.join(root, f"dir{d:03d}", f"sub{s:02d}")
        os.makedirs(dpath, exist_ok=True)
        for f in range(99):
            size = rng.randint(1024, 20 * 1024)
            if (f % 2) == 0:
                data = os.urandom(size)
            else:
                data = (text * (size // len(text) + 1))[:size]
            with open(os.path.join(dpath, f"file{f:03d}.dat"), "wb") as fh:
                fh.write(data)
            small += 1
        # 每個 sub 目錄一個大檔
        big = 9_400_000
        if (s % 2) == 0:
            data = os.urandom(big)
        else:
            chunk = os.urandom(4096)
            data = (chunk + text) * (big // (4096 + len(text)) + 1)
            data = data[:big]
        with open(os.path.join(dpath, "big.bin"), "wb") as fh:
            fh.write(data)
print("small files:", small, "big files:", 1000)
