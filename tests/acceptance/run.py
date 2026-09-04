"""M1 驗收腳本：backup → restore → diff -r；第二次 backup；check 偵測破壞；記錄峰值記憶體。
每一步都有斷言，任何一步失敗就非 0 結束。"""
import os, resource, shutil, subprocess, sys, time

BASE = os.path.dirname(os.path.abspath(__file__))
KIST = sys.argv[1]
SRC = os.path.join(BASE, "src")
REPO = os.path.join(BASE, "repo")
OUT = os.path.join(BASE, "out")
ENV = dict(os.environ, KIST_REPO=REPO, KIST_PASSWORD="acceptance", KIST_CLIENT_ID_FILE=os.path.join(BASE, "client-id"))

def run(args, check=True):
    before = resource.getrusage(resource.RUSAGE_CHILDREN).ru_maxrss
    t0 = time.time()
    p = subprocess.run(args, env=ENV, capture_output=True, text=True)
    dt = time.time() - t0
    peak = resource.getrusage(resource.RUSAGE_CHILDREN).ru_maxrss
    print(f"$ {' '.join(args)}\n  -> exit {p.returncode}, {dt:.1f}s, child peak RSS so far {peak/1024:.0f} MiB")
    if p.stdout.strip():
        print("  " + p.stdout.strip().replace("\n", "\n  "))
    if p.stderr.strip():
        print("  stderr: " + p.stderr.strip()[:2000].replace("\n", "\n  "))
    if check:
        assert p.returncode == 0, f"{args} failed"
    return p, dt

def count(prefix):
    d = os.path.join(REPO, prefix)
    return sum(len(f) for _, _, f in os.walk(d)) if os.path.isdir(d) else 0

def du(path):
    return sum(os.path.getsize(os.path.join(r, f)) for r, _, fs in os.walk(path) for f in fs)

for d in (REPO, OUT):
    shutil.rmtree(d, ignore_errors=True)

nfiles = sum(len(f) for _, _, f in os.walk(SRC))
print(f"source: {nfiles} files, {du(SRC)/2**30:.2f} GiB")

run([KIST, "init"])
_, t_backup1 = run([KIST, "backup", SRC])
packs1, trees1 = count("packs"), count("trees")
print(f"after backup 1: {packs1} packs, {trees1} trees, repo {du(REPO)/2**30:.2f} GiB")
peak_backup = resource.getrusage(resource.RUSAGE_CHILDREN).ru_maxrss

_, t_backup2 = run([KIST, "backup", SRC])
packs2 = count("packs")
print(f"after backup 2: {packs2} packs")
assert packs2 == packs1, "second backup wrote new packs"

_, t_restore = run([KIST, "restore", "latest", OUT])
restored = os.path.join(OUT, SRC.lstrip("/"))
p, t_diff = run(["diff", "-r", SRC, restored])
print("diff -r: identical")

_, t_check = run([KIST, "check"])
_, t_check_data = run([KIST, "check", "--read-data"])

# 人為破壞一個 pack
packs_dir = os.path.join(REPO, "packs")
victim = sorted(os.listdir(packs_dir))[0]
path = os.path.join(packs_dir, victim)
data = bytearray(open(path, "rb").read())
data[len(data)//2] ^= 0x01
open(path, "wb").write(data)
p, _ = run([KIST, "check", "--read-data"], check=False)
assert p.returncode != 0, "check --read-data did not detect corruption"
assert victim in p.stderr, "check did not name the corrupted pack"
print(f"corruption detected in {victim}")

print("\n=== SUMMARY ===")
print(f"files: {nfiles}, source {du(SRC)/2**30:.2f} GiB, repo {du(REPO)/2**30:.2f} GiB, packs {packs1}, trees {trees1}")
print(f"backup 1: {t_backup1:.0f}s  backup 2: {t_backup2:.0f}s  restore: {t_restore:.0f}s  check: {t_check:.0f}s  check --read-data: {t_check_data:.0f}s")
print(f"peak RSS (max over all child processes, dominated by backup 1): {peak_backup/1024:.0f} MiB")
print("ALL ACCEPTANCE CHECKS PASSED")
