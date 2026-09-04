"""M3 驗收（GC）：backup → 刪掉一成的目錄再 backup → forget 舊的 → prune（標記 + repack）→
再 backup → prune（刪除）→ check --read-data → restore 與 diff -r。每步有斷言，記錄時間與峰值 RSS。
用法：python3 -u run_gc.py <kist binary>（跟 run.py 一樣在 repo 之外的目錄跑，需要 src/ 已產生）。"""
import os, resource, shutil, subprocess, sys, time

BASE = os.path.dirname(os.path.abspath(__file__))
KIST = sys.argv[1]
SRC = os.path.join(BASE, "src")
REPO = os.path.join(BASE, "repo-gc")
OUT = os.path.join(BASE, "out-gc")
ENV = dict(os.environ, KIST_REPO=REPO, KIST_PASSWORD="acceptance", KIST_CLIENT_ID_FILE=os.path.join(BASE, "client-id"),
           KIST_CACHE_DIR=os.path.join(BASE, "cache-gc"))

def run(args, check=True):
    t0 = time.time()
    p = subprocess.run(args, env=ENV, capture_output=True, text=True)
    dt = time.time() - t0
    peak = resource.getrusage(resource.RUSAGE_CHILDREN).ru_maxrss
    print(f"$ {' '.join(args)}\n  -> exit {p.returncode}, {dt:.1f}s, child peak RSS so far {peak/1024:.0f} MiB", flush=True)
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

for d in (REPO, OUT, os.path.join(BASE, "cache-gc")):
    shutil.rmtree(d, ignore_errors=True)
# 復原上一次跑刪掉的目錄：重新產生（gen.py 是決定性的）
subprocess.run([sys.executable, os.path.join(BASE, "gen.py"), SRC], check=True, capture_output=True)
nfiles = sum(len(f) for _, _, f in os.walk(SRC))
print(f"source: {nfiles} files, {du(SRC)/2**30:.2f} GiB", flush=True)

run([KIST, "init"])
_, t_b1 = run([KIST, "backup", SRC])
packs1, repo1 = count("packs"), du(REPO)
print(f"after backup 1: {packs1} packs, repo {repo1/2**30:.2f} GiB", flush=True)

# 刪掉 dir000..dir009（一成的資料）
for d in range(10):
    shutil.rmtree(os.path.join(SRC, f"dir{d:03d}"))
_, t_b2 = run([KIST, "backup", SRC])
snaps = run([KIST, "snapshots"])[0].stdout.strip().splitlines()
first_ts = snaps[1].split()[1]
run([KIST, "forget", first_ts])

_, t_p1 = run([KIST, "prune", "--grace", "0s"])
gc1, packs_p1, repo_p1 = count("gc"), count("packs"), du(REPO)
print(f"after prune 1: {gc1} markers, {packs_p1} packs, repo {repo_p1/2**30:.2f} GiB", flush=True)
assert gc1 > 0, "prune 1 should have marked something"
assert packs_p1 >= packs1, "prune 1 must not delete packs"

_, t_b3 = run([KIST, "backup", SRC])
_, t_p2 = run([KIST, "prune", "--grace", "0s"])
packs_p2, repo_p2 = count("packs"), du(REPO)
print(f"after prune 2: {count('gc')} markers, {packs_p2} packs, repo {repo_p2/2**30:.2f} GiB", flush=True)
# 整包死掉的 pack 在這輪刪；被 repack 的舊 pack 這輪才變孤兒被標記，下一輪刪
assert repo_p2 <= repo_p1

# 被 repack 的舊 pack 在 prune 2 才被標記；活躍 client 要在那之後再備份一次，prune 3 才會刪
run([KIST, "backup", SRC])
_, t_p3 = run([KIST, "prune", "--grace", "0s"])
run([KIST, "backup", SRC])
run([KIST, "prune", "--grace", "0s"])
repo_final = du(REPO)
print(f"final: {count('gc')} markers, {count('packs')} packs, {count('indexes')} indexes, repo {repo_final/2**30:.2f} GiB", flush=True)
assert repo_final < repo1 * 0.95, "GC should have reclaimed the deleted 10%"

_, t_check = run([KIST, "check", "--read-data"])
_, t_restore = run([KIST, "restore", "latest", OUT])
restored = os.path.join(OUT, SRC.lstrip("/"))
run(["diff", "-r", SRC, restored])
print("diff -r: identical", flush=True)

print("\n=== SUMMARY (GC) ===")
print(f"backup 1: {t_b1:.0f}s ({repo1/2**30:.2f} GiB)  backup 2 (-10%): {t_b2:.0f}s  prune 1 (mark+repack): {t_p1:.0f}s  "
      f"backup 3: {t_b3:.0f}s  prune 2 (delete): {t_p2:.0f}s  check --read-data: {t_check:.0f}s  restore: {t_restore:.0f}s")
print(f"repo: {repo1/2**30:.2f} GiB → {repo_p2/2**30:.2f} GiB after prune 2 → {repo_final/2**30:.2f} GiB after prune 3-4 (prune 3: {t_p3:.0f}s)")
print(f"peak RSS (max over all child processes): {resource.getrusage(resource.RUSAGE_CHILDREN).ru_maxrss/1024:.0f} MiB")
print("ALL GC ACCEPTANCE CHECKS PASSED")
