"""M2 驗收腳本（S3/MinIO 版，v4）：對 s3:// repo 跑 backup → 第二次 backup → restore → diff -r →
check --read-data → prune（v4：第一輪把 pack 收錄進 map，不刪東西）→ check → 第三次 backup → 人為破壞 pack 後
check 要抓到；記錄每步時間與峰值記憶體。第二次 backup（pack 只在 delta 裡）、prune 之後的第三次 backup（新的一代，
本地快取要下載它的 map 分片）與第四次（快取已有這一代）都在容器內以 `mc admin trace --json` 數 S3 請求（依 API 分類）。
物件的數量與大小、破壞 pack 都透過容器內的 `mc`（本機沒有 mc 也能跑）。
用法：python3 run_s3.py <kist binary>；需要先 eval "$(sh tests/minio-setup.sh)"。"""
import os, resource, shutil, subprocess, sys, time

BASE = os.path.dirname(os.path.abspath(__file__))
KIST = sys.argv[1]
SRC = os.path.join(BASE, "src")
OUT = os.path.join(BASE, "out")
CONTAINER = os.environ.get("MINIO_CONTAINER", "kist-minio")
BUCKET = os.environ["KIST_TEST_S3_BUCKET"]
PREFIX = "acceptance"
REPO = f"s3://{BUCKET}/{PREFIX}"
ENV = dict(os.environ, KIST_REPO=REPO, KIST_PASSWORD="acceptance",
           KIST_CLIENT_ID_FILE=os.path.join(BASE, "client-id"), KIST_CACHE_DIR=os.path.join(BASE, "cache"),
           AWS_ENDPOINT=os.environ["KIST_TEST_S3_ENDPOINT"], AWS_ALLOW_HTTP="true", AWS_DEFAULT_REGION="us-east-1")

def run(args, check=True, env=ENV):
    t0 = time.time()
    p = subprocess.run(args, env=env, capture_output=True, text=True)
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

def mc(*args, input=None):
    p = subprocess.run(["docker", "exec", "-i", CONTAINER, "mc", *args], input=input, capture_output=True)
    assert p.returncode == 0, f"mc {args} failed: {p.stderr.decode()[:500]}"
    return p.stdout

def count(prefix):
    out = mc("ls", "--recursive", f"local/{BUCKET}/{PREFIX}/{prefix}/").decode()
    return len([l for l in out.splitlines() if l.strip()])

def traced(args):
    """在容器內開 `mc admin trace`，跑一個命令，回傳 (run 的結果, 依 API 分類的請求數)。"""
    import collections, json, urllib.parse
    mc("admin", "trace", "--help")  # 這版 mc 要有 trace
    subprocess.run(["docker", "exec", CONTAINER, "sh", "-c", "rm -f /tmp/trace.jsonl"], check=True)
    tracer = subprocess.Popen(["docker", "exec", "-i", CONTAINER, "mc", "admin", "trace", "--json", "--call", "s3", "local"],
                              stdout=open(os.path.join(BASE, "trace.jsonl"), "wb"), stderr=subprocess.DEVNULL)
    time.sleep(3)  # trace 訂閱要一點時間；太早發的請求會漏
    result = run(args)
    time.sleep(2)
    tracer.terminate(); tracer.wait()
    counts = collections.Counter()
    with open(os.path.join(BASE, "trace.jsonl"), "rb") as f:
        for line in f:
            if not line.strip():
                continue
            rec = json.loads(line)
            # LIST 的 path 是 bucket 本身，repo 的前綴在 query 的 prefix 參數裡（2026-10-19 之前漏數了 LIST）
            listed = urllib.parse.parse_qs(rec.get("query", "")).get("prefix", [""])[0]
            if PREFIX in rec.get("path", "") or listed.startswith(PREFIX + "/"):
                counts[rec["api"]] += 1
    total = sum(counts.values())
    print(f"  S3 requests to {PREFIX}: {total}: " + ", ".join(f"{k} {v}" for k, v in sorted(counts.items())), flush=True)
    return result, total, counts

def repo_bytes():
    out = mc("du", "--json", f"local/{BUCKET}/{PREFIX}/").decode()
    import json
    return json.loads(out.splitlines()[-1])["size"]

def du(path):
    return sum(os.path.getsize(os.path.join(r, f)) for r, _, fs in os.walk(path) for f in fs)

shutil.rmtree(OUT, ignore_errors=True)
shutil.rmtree(os.path.join(BASE, "cache"), ignore_errors=True)
mc("rm", "--recursive", "--force", f"local/{BUCKET}/{PREFIX}/")
nfiles = sum(len(f) for _, _, f in os.walk(SRC))
print(f"source: {nfiles} files, {du(SRC)/2**30:.2f} GiB -> {REPO}", flush=True)

run([KIST, "init"])
_, t_backup1 = run([KIST, "backup", SRC])
packs1, mpacks1 = count("packs"), count("mpacks")
print(f"after backup 1: {packs1} packs, {mpacks1} mpacks, repo {repo_bytes()/2**30:.2f} GiB", flush=True)
peak_backup = resource.getrusage(resource.RUSAGE_CHILDREN).ru_maxrss

(_, t_backup2), req_backup2, _ = traced([KIST, "backup", SRC])
packs2 = count("packs")
print(f"after backup 2: {packs2} packs", flush=True)
assert packs2 == packs1, "second backup wrote new packs"

_, t_restore = run([KIST, "restore", "latest", OUT])
restored = os.path.join(OUT, SRC.lstrip("/"))
p, t_diff = run(["diff", "-r", SRC, restored])
print("diff -r: identical", flush=True)

_, t_check = run([KIST, "check"])
_, t_check_data = run([KIST, "check", "--read-data"])

# 維護角色收錄進 map（v4 的 rebuild 只在 map 壞掉時重建；收錄是 prune 的第一輪）
import json as _json
p, t_prune = run([KIST, "--json", "prune"])
prune_report = _json.loads(p.stdout)
print(f"  prune report: generation {prune_report['generation']}, newcomers {prune_report['newcomers']}, "
      f"deleted packs {prune_report['deleted_packs']}, warnings {len(prune_report['warnings'])}", flush=True)
assert prune_report["newcomers"] == packs1 + mpacks1, "prune did not index every pack"
assert prune_report["deleted_packs"] == 0
_, t_check2 = run([KIST, "check"])
# 收錄進 map 之後，沒變的 backup 不必再讀 delta 裡的 pack trailer
(_, t_backup3), req_backup3, _ = traced([KIST, "backup", SRC])
assert count("packs") == packs1, "third backup wrote new packs"
# 同一代再備一次：分片已在本地快取
(_, t_backup4), req_backup4, _ = traced([KIST, "backup", SRC])
assert count("packs") == packs1, "fourth backup wrote new packs"

# 人為破壞一個 pack：下載、翻一個 bit、傳回去
client_dir = mc("ls", f"local/{BUCKET}/{PREFIX}/packs/").decode().split()[-1].rstrip("/")
victim = sorted(l.split()[-1] for l in mc("ls", f"local/{BUCKET}/{PREFIX}/packs/{client_dir}/").decode().splitlines() if l.strip())[-1]
key = f"local/{BUCKET}/{PREFIX}/packs/{client_dir}/{victim}"
data = bytearray(mc("cat", key))
data[len(data)//2] ^= 0x01
mc("pipe", key, input=bytes(data))
p, _ = run([KIST, "check", "--read-data"], check=False)
assert p.returncode != 0, "check --read-data did not detect corruption"
assert victim in p.stderr, "check did not name the corrupted pack"
print(f"corruption detected in {victim}", flush=True)

print("\n=== SUMMARY (S3/MinIO) ===")
print(f"files: {nfiles}, source {du(SRC)/2**30:.2f} GiB, repo {repo_bytes()/2**30:.2f} GiB, packs {packs1}, mpacks {mpacks1}")
print(f"backup 1: {t_backup1:.0f}s  backup 2: {t_backup2:.0f}s  restore: {t_restore:.0f}s  check: {t_check:.0f}s  "
      f"check --read-data: {t_check_data:.0f}s  prune (index {prune_report['newcomers']} packs): {t_prune:.0f}s  "
      f"check after prune: {t_check2:.0f}s  backup 3 (after prune): {t_backup3:.0f}s  backup 4: {t_backup4:.0f}s")
print(f"S3 requests of an unchanged backup: {req_backup2} before the map holds the packs, {req_backup3} right after prune "
      f"(new generation: the cache downloads its map shards), {req_backup4} once the cache has that generation")
print(f"peak RSS (max over all child processes, dominated by backup 1): {peak_backup/1024:.0f} MiB")
print("ALL ACCEPTANCE CHECKS PASSED")
