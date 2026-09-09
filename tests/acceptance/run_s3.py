"""M2 驗收腳本（S3/MinIO 版）：對 s3:// repo 跑 backup → 第二次 backup → restore → diff -r →
check --read-data → 人為破壞 pack 後 check 要抓到；記錄每步時間與峰值記憶體。
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
packs1, trees1 = count("packs"), count("trees")
print(f"after backup 1: {packs1} packs, {trees1} trees, repo {repo_bytes()/2**30:.2f} GiB", flush=True)
peak_backup = resource.getrusage(resource.RUSAGE_CHILDREN).ru_maxrss

_, t_backup2 = run([KIST, "backup", SRC])
packs2 = count("packs")
print(f"after backup 2: {packs2} packs", flush=True)
assert packs2 == packs1, "second backup wrote new packs"

_, t_restore = run([KIST, "restore", "latest", OUT])
restored = os.path.join(OUT, SRC.lstrip("/"))
p, t_diff = run(["diff", "-r", SRC, restored])
print("diff -r: identical", flush=True)

_, t_check = run([KIST, "check"])
_, t_check_data = run([KIST, "check", "--read-data"])

_, t_rebuild = run([KIST, "rebuild-index"])
_, t_check2 = run([KIST, "check"])

# 人為破壞一個 pack：下載、翻一個 bit、傳回去
victim = sorted(mc("ls", f"local/{BUCKET}/{PREFIX}/packs/").decode().split())[-1]
key = f"local/{BUCKET}/{PREFIX}/packs/{victim}"
data = bytearray(mc("cat", key))
data[len(data)//2] ^= 0x01
mc("pipe", key, input=bytes(data))
p, _ = run([KIST, "check", "--read-data"], check=False)
assert p.returncode != 0, "check --read-data did not detect corruption"
assert victim in p.stderr, "check did not name the corrupted pack"
print(f"corruption detected in {victim}", flush=True)

print("\n=== SUMMARY (S3/MinIO) ===")
print(f"files: {nfiles}, source {du(SRC)/2**30:.2f} GiB, repo {repo_bytes()/2**30:.2f} GiB, packs {packs1}, trees {trees1}")
print(f"backup 1: {t_backup1:.0f}s  backup 2: {t_backup2:.0f}s  restore: {t_restore:.0f}s  check: {t_check:.0f}s  "
      f"check --read-data: {t_check_data:.0f}s  rebuild-index: {t_rebuild:.0f}s  check after rebuild: {t_check2:.0f}s")
print(f"peak RSS (max over all child processes, dominated by backup 1): {peak_backup/1024:.0f} MiB")
print("ALL ACCEPTANCE CHECKS PASSED")
