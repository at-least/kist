"""GC 驗收（v4，M12 第 7 段）：backup → 刪掉一成的目錄再 backup → forget 舊的 → 三輪真實時鐘的 prune
（標記要等 pack 比 grace 老、刪除再等一個 grace，grace 最低 2 小時，所以整段要跑四個多小時）→
check --read-data → restore 與 diff -r。每步有斷言，記錄時間、物件數與峰值 RSS。
用法：python3 -u run_gc.py <kist binary>（跟 run.py 一樣在 repo 之外的目錄跑，需要 src/ 已產生）。
KIST_GC_GRACE 可以改 grace（預設 2h，不能更短）；KIST_GC_MARGIN_SECS 是每次等待多加的秒數（預設 120）。"""
import json, os, resource, shutil, subprocess, sys, time

BASE = os.path.dirname(os.path.abspath(__file__))
KIST = sys.argv[1]
SRC = os.path.join(BASE, "src")
REPO = os.path.join(BASE, "repo-gc")
OUT = os.path.join(BASE, "out-gc")
GRACE = os.environ.get("KIST_GC_GRACE", "2h")
GRACE_SECS = {"h": 3600, "m": 60, "s": 1}[GRACE[-1]] * int(GRACE[:-1])
MARGIN = int(os.environ.get("KIST_GC_MARGIN_SECS", "120"))
ENV = dict(os.environ, KIST_REPO=REPO, KIST_PASSWORD="acceptance", KIST_CLIENT_ID_FILE=os.path.join(BASE, "client-id"),
           KIST_CACHE_DIR=os.path.join(BASE, "cache-gc"))


def run(args, check=True):
    t0 = time.time()
    p = subprocess.run(args, env=ENV, capture_output=True, text=True)
    dt = time.time() - t0
    peak = resource.getrusage(resource.RUSAGE_CHILDREN).ru_maxrss
    print(f"$ {time.strftime('%H:%M:%S')} {' '.join(args)}\n  -> exit {p.returncode}, {dt:.1f}s, child peak RSS so far {peak/1024:.0f} MiB", flush=True)
    if p.stdout.strip():
        print("  " + p.stdout.strip().replace("\n", "\n  "), flush=True)
    if p.stderr.strip():
        print("  stderr: " + p.stderr.strip()[:2000].replace("\n", "\n  "), flush=True)
    if check:
        assert p.returncode == 0, f"{args} failed"
    return p, dt


def prune():
    p, dt = run([KIST, "--json", "prune"])
    report = json.loads(p.stdout)
    print(f"  prune report: generation {report['generation']} (from {report['base']}), newcomers {report['newcomers']}, "
          f"condemned {report['condemned']}, revived {report['revived']}, tombstoned {report['tombstoned']}, "
          f"repacked {report['repacked']}, deleted packs {report['deleted_packs']} ({report['deleted_bytes']/2**20:.0f} MiB), "
          f"warnings {len(report['warnings'])}", flush=True)
    return report, dt


def count(prefix):
    d = os.path.join(REPO, prefix)
    return sum(len(f) for _, _, f in os.walk(d)) if os.path.isdir(d) else 0


def du(path):
    return sum(os.path.getsize(os.path.join(r, f)) for r, _, fs in os.walk(path) for f in fs)


def wait_until(deadline, why):
    now = time.time()
    if deadline > now:
        print(f"  sleeping {deadline - now:.0f}s ({why}; until {time.strftime('%H:%M:%S', time.localtime(deadline))})", flush=True)
        time.sleep(deadline - now)


for d in (REPO, OUT, os.path.join(BASE, "cache-gc")):
    shutil.rmtree(d, ignore_errors=True)
# 復原上一次跑刪掉的目錄：重新產生（gen.py 是決定性的）
subprocess.run([sys.executable, os.path.join(BASE, "gen.py"), SRC], check=True, capture_output=True)
nfiles = sum(len(f) for _, _, f in os.walk(SRC))
print(f"source: {nfiles} files, {du(SRC)/2**30:.2f} GiB; grace {GRACE}", flush=True)

run([KIST, "init"])
run([KIST, "settings", "--grace", GRACE])
_, t_b1 = run([KIST, "backup", SRC])
backup1_done = time.time()
packs1, mpacks1, repo1 = count("packs"), count("mpacks"), du(REPO)
print(f"after backup 1: {packs1} packs, {mpacks1} mpacks, repo {repo1/2**30:.2f} GiB", flush=True)

# 刪掉 dir000..dir009（一成的資料）
for d in range(10):
    shutil.rmtree(os.path.join(SRC, f"dir{d:03d}"))
_, t_b2 = run([KIST, "backup", SRC])
snaps = json.loads(run([KIST, "--json", "snapshots"])[0].stdout)
snaps.sort(key=lambda s: s["key"])
assert len(snaps) == 2, snaps
run([KIST, "forget", snaps[0]["key"]])

r1, t_p1 = prune()
assert r1["newcomers"] >= packs1 + mpacks1, "prune 1 should absorb every pack"
assert r1["condemned"] == 0 and r1["deleted_packs"] == 0, "nothing is older than the grace yet"

wait_until(backup1_done + GRACE_SECS + MARGIN, "packs must be older than the grace before P7b marks them")
r2, t_p2 = prune()
marked_at = time.time()
packs_p2, repo_p2 = count("packs"), du(REPO)
print(f"after prune 2: {packs_p2} packs, repo {repo_p2/2**30:.2f} GiB", flush=True)
assert r2["condemned"] > 0, "prune 2 should have marked the packs only the forgotten snapshot used"
assert r2["deleted_packs"] == 0, "prune 2 must not delete: the marks are fresh"
assert packs_p2 >= packs1, "prune 2 must not delete packs"

wait_until(marked_at + GRACE_SECS + MARGIN, "marks must be older than the grace on both clocks before P7d deletes")
r3, t_p3 = prune()
packs_p3, repo_p3 = count("packs"), du(REPO)
print(f"after prune 3: {packs_p3} packs, repo {repo_p3/2**30:.2f} GiB", flush=True)
assert r3["tombstoned"] > 0 and r3["deleted_packs"] > 0, "prune 3 should delete what prune 2 marked"
assert repo_p3 < repo1 * 0.95, "GC should have reclaimed the deleted 10%"

r4, t_p4 = prune()
assert r4["deleted_packs"] == 0, "nothing left to delete"

_, t_check = run([KIST, "check", "--read-data"])
_, t_restore = run([KIST, "restore", "latest", OUT])
restored = os.path.join(OUT, SRC.lstrip("/"))
run(["diff", "-r", SRC, restored])
print("diff -r: identical", flush=True)

print("\n=== SUMMARY (GC, v4) ===")
print(f"backup 1: {t_b1:.0f}s ({repo1/2**30:.2f} GiB, {packs1} packs, {mpacks1} mpacks)  backup 2 (-10%): {t_b2:.0f}s  "
      f"prune 1 (absorb): {t_p1:.0f}s  prune 2 (mark {r2['condemned']}, repack {r2['repacked']}): {t_p2:.0f}s  "
      f"prune 3 (delete {r3['deleted_packs']} packs, {r3['deleted_bytes']/2**20:.0f} MiB): {t_p3:.0f}s  prune 4: {t_p4:.0f}s  "
      f"check --read-data: {t_check:.0f}s  restore: {t_restore:.0f}s")
print(f"repo: {repo1/2**30:.2f} GiB → {repo_p2/2**30:.2f} GiB after prune 2 → {repo_p3/2**30:.2f} GiB after prune 3")
print(f"peak RSS (max over all child processes): {resource.getrusage(resource.RUSAGE_CHILDREN).ru_maxrss/1024:.0f} MiB")
print("ALL GC ACCEPTANCE CHECKS PASSED")
