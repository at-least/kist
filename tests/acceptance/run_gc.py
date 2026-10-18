"""GC 驗收（v4，M12 第 7 段）：backup → 刪掉一成的目錄再 backup → forget 舊的 → 真實時鐘的 prune
（標記要等 pack 比 grace 老、刪除再等一個 grace，grace 最低 2 小時，所以整段要跑四個多小時）→
check --read-data → restore 與 diff -r。每步有斷言，記錄時間、物件數與峰值 RSS。
v4 的時間只比 head（format-v4.md §11.1）：一輪 prune 的「現在的 T」是它 P1 讀到的最新 head 的時間，不是後端的
當下，所以等過一個 grace 之後要跑兩輪——第一輪寫的 head 才把 T 推到現在、第二輪才標記（刪除也一樣；
`crates/kist-core/tests/v4_prune.rs` 的情境是同樣的節奏）。第一次跑（2026-10-18 15:02）只跑一輪就斷言，錯的是腳本。
第二次跑（17:09）在第二輪斷言 condemned > 0 又錯：這份資料集奇數號的小檔是同一段文字截成隨機長度，同長度的檔跨目錄
相同、去重成同一個 chunk，所以刪掉的 dir000–dir009 的每個 pack 都還剩幾個被引用的 chunk（修後 7 個新 pack 各 68–90 KiB），
走的是 P7c 的 repack（舊 pack 一樣標成 condemned，但數在 repacked），不是 P7b 整個 pack 的標記；兩條都是「標記」，
斷言改成 condemned + repacked > 0。同一次跑還發現 init 之後 `settings --grace 2h` 是調低，開始 72 小時的 drain，期間的
標記都以 72 小時當 age，第三階段永遠刪不到——grace 改在 `init --grace` 給（第十步加的）。
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

run([KIST, "init", "--grace", GRACE])
assert "drain" not in run([KIST, "settings"])[0].stdout, "grace given at init: no drain"
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
# 2a：這一輪的「現在的 T」還是 prune 1 寫的 head 的時間（比 pack 晚不到一秒），只是把 T 推進；2b 才標記
r2a, t_p2a = prune()
assert r2a["condemned"] == 0 and r2a["deleted_packs"] == 0, "prune 2a only advances T (its base head is prune 1's)"
r2, t_p2 = prune()
marked_at = time.time()
packs_p2, repo_p2 = count("packs"), du(REPO)
print(f"after prune 2b: {packs_p2} packs, repo {repo_p2/2**30:.2f} GiB", flush=True)
# 這份資料集的 pack 沒有一個是整個沒人引用的（跨目錄相同的小文字檔），所以走 repack；兩條路都把舊 pack 標成 condemned
assert r2["condemned"] + r2["repacked"] > 0, "prune 2b should have marked or repacked the forgotten snapshot's packs"
assert r2["deleted_packs"] == 0, "prune 2b must not delete: the marks are fresh"
assert packs_p2 >= packs1, "prune 2b must not delete packs"

wait_until(marked_at + GRACE_SECS + MARGIN, "marks must be older than the grace on both clocks before P7d deletes")
# 3a 推進 T（base 是 2b 的 head，標記在那一代），3b 才 tombstone 並刪除
r3a, t_p3a = prune()
assert r3a["deleted_packs"] == 0, "prune 3a only advances T"
r3, t_p3 = prune()
packs_p3, repo_p3 = count("packs"), du(REPO)
print(f"after prune 3b: {packs_p3} packs, repo {repo_p3/2**30:.2f} GiB", flush=True)
assert r3["tombstoned"] > 0 and r3["deleted_packs"] > 0, "prune 3b should delete what prune 2b marked"
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
      f"prune 1 (absorb): {t_p1:.0f}s  prune 2a (advance T): {t_p2a:.0f}s  "
      f"prune 2b (mark {r2['condemned']}, repack {r2['repacked']}): {t_p2:.0f}s  prune 3a (advance T): {t_p3a:.0f}s  "
      f"prune 3b (delete {r3['deleted_packs']} packs, {r3['deleted_bytes']/2**20:.0f} MiB): {t_p3:.0f}s  prune 4: {t_p4:.0f}s  "
      f"check --read-data: {t_check:.0f}s  restore: {t_restore:.0f}s")
print(f"repo: {repo1/2**30:.2f} GiB → {repo_p2/2**30:.2f} GiB after prune 2b → {repo_p3/2**30:.2f} GiB after prune 3b")
print(f"peak RSS (max over all child processes): {resource.getrusage(resource.RUSAGE_CHILDREN).ru_maxrss/1024:.0f} MiB")
print("ALL GC ACCEPTANCE CHECKS PASSED")
