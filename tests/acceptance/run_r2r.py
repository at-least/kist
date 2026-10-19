"""Remote-to-remote 驗收（M12 第 7 段之三）：SFTP 來源與 S3 來源各自備份進 MinIO 上的一個 S3 repo——第一次、沒變的
第二次，SFTP 再加一次 `--trust-mtime`（前兩次都整份讀過，這次應該全部沿用）——之後 restore 並對本機的同一份資料
`diff -r`、`check`。每個 backup 記時間、峰值 RSS（每 0.1 秒取樣 /proc 的 VmRSS，同 rss.py），並在容器內以
`mc admin trace --json` 數 S3 請求，依前綴分成來源與 repo（LIST 的 path 是 bucket，前綴在 query 的 prefix）。

用法：python3 -u run_r2r.py <kist binary>，在放著 gen.py 產生的 `src` 的目錄跑。事先：
  eval "$(sh tests/minio-setup.sh)"; eval "$(sh tests/sftp-setup.sh)"
並把同一份 `src` 放進兩個容器（README 有指令）：SFTP 的 `$KIST_TEST_SFTP_URL/m1`、MinIO 的
`s3://$KIST_TEST_S3_BUCKET/m1-source`。"""
import collections, json, os, shutil, subprocess, sys, tempfile, time, urllib.parse

BASE = os.path.dirname(os.path.abspath(__file__))
KIST = sys.argv[1]
SRC = os.path.join(BASE, "src")
OUT = os.path.join(BASE, "out-r2r")
CONTAINER = os.environ.get("MINIO_CONTAINER", "kist-minio")
BUCKET = os.environ["KIST_TEST_S3_BUCKET"]
# 容器裡的那份資料的名稱（預設是 README 的指令放的位置）；小資料集試跑時可以換
SFTP_URL = os.environ["KIST_TEST_SFTP_URL"].rstrip("/") + "/" + os.environ.get("KIST_R2R_SFTP_DIR", "m1")
S3_SOURCE = os.environ.get("KIST_R2R_S3_PREFIX", "m1-source")
S3_URL = f"s3://{BUCKET}/{S3_SOURCE}"
COMMON = dict(os.environ, KIST_PASSWORD="acceptance", AWS_ENDPOINT=os.environ["KIST_TEST_S3_ENDPOINT"],
              AWS_ALLOW_HTTP="true", AWS_DEFAULT_REGION="us-east-1",
              KIST_SFTP_PASSWORD=os.environ["KIST_TEST_SFTP_PASSWORD"],
              KIST_SFTP_KNOWN_HOSTS=os.environ["KIST_TEST_SFTP_KNOWN_HOSTS"])


def env_for(repo_prefix):
    return dict(COMMON, KIST_REPO=f"s3://{BUCKET}/{repo_prefix}",
                KIST_CLIENT_ID_FILE=os.path.join(BASE, f"{repo_prefix}.id"),
                KIST_CACHE_DIR=os.path.join(BASE, f"{repo_prefix}-cache"))


def run(env, args):
    """跑一個命令，回傳 (stdout, 秒數, 峰值 RSS MiB)；失敗就結束。"""
    t0 = time.time()
    # 輸出寫進暫存檔：警告多的時候 pipe 會滿，取樣迴圈又不讀它，命令就卡住
    out_f, err_f = tempfile.TemporaryFile("w+"), tempfile.TemporaryFile("w+")
    p = subprocess.Popen(args, env=env, stdout=out_f, stderr=err_f, text=True)
    peak = 0
    while p.poll() is None:
        try:
            with open(f"/proc/{p.pid}/status") as f:
                for line in f:
                    if line.startswith("VmRSS:"):
                        peak = max(peak, int(line.split()[1]) // 1024)
        except FileNotFoundError:
            pass
        time.sleep(0.1)
    dt = time.time() - t0
    out_f.seek(0); err_f.seek(0)
    out, err = out_f.read(), err_f.read()
    print(f"$ {' '.join(args)}\n  -> exit {p.returncode}, {dt:.1f}s, peak RSS {peak} MiB", flush=True)
    if err.strip():
        print("  stderr: " + err.strip()[:2000].replace("\n", "\n  "), flush=True)
    assert p.returncode == 0, f"{args} failed"
    return out, dt, peak


def mc(*args):
    p = subprocess.run(["docker", "exec", "-i", CONTAINER, "mc", *args], capture_output=True)
    assert p.returncode == 0, f"mc {args} failed: {p.stderr.decode()[:500]}"
    return p.stdout


def traced(env, args, groups):
    """在容器內開 `mc admin trace`、跑一個命令；回傳 run 的結果與 {前綴: {API: 次數}}。"""
    trace = os.path.join(BASE, "trace-r2r.jsonl")
    # 容器裡的 mc 要自己停：殺掉本機的 docker exec 不會停它（沒有 tty，訊號不轉過去），留下來的訂閱者會一直佔著
    # MinIO（一個約 12% 的 CPU），累積幾個之後新的 trace 就收不到東西（2026-10-19 的第一次跑，S3 那一半全是 0）。
    tracer = subprocess.Popen(["docker", "exec", "-i", CONTAINER, "sh", "-c",
                               "echo $$ > /tmp/kist-r2r-trace.pid; exec mc admin trace --json --call s3 local"],
                              stdout=open(trace, "wb"), stderr=subprocess.DEVNULL)
    time.sleep(3)  # trace 訂閱要一點時間；太早發的請求會漏
    result = run(env, args)
    time.sleep(2)
    subprocess.run(["docker", "exec", CONTAINER, "sh", "-c", "read p < /tmp/kist-r2r-trace.pid && kill $p"], check=True)
    tracer.wait()
    counts = {g: collections.Counter() for g in groups}
    with open(trace, "rb") as f:
        for line in f:
            if not line.strip():
                continue
            rec = json.loads(line)
            listed = urllib.parse.parse_qs(rec.get("query", "")).get("prefix", [""])[0]
            for g in groups:
                if f"/{BUCKET}/{g}/" in rec.get("path", "") or listed == g or listed.startswith(g + "/"):
                    counts[g][rec["api"].replace("s3.", "")] += 1
    os.remove(trace)
    for g, c in counts.items():
        print(f"  S3 requests to {g}: {sum(c.values())}: " + ", ".join(f"{k} {v}" for k, v in sorted(c.items())), flush=True)
    return result, counts


def total(c):
    return sum(c.values())


nfiles = sum(len(f) for _, _, f in os.walk(SRC))
nbytes = sum(os.path.getsize(os.path.join(r, f)) for r, _, fs in os.walk(SRC) for f in fs)
print(f"source: {nfiles} files, {nbytes/2**30:.2f} GiB; {SFTP_URL} and {S3_URL} hold the same tree", flush=True)

rows = []
for kind, url, repo in (("sftp", SFTP_URL, "r2r-from-sftp"), ("s3", S3_URL, "r2r-from-s3")):
    env = env_for(repo)
    mc("rm", "--recursive", "--force", f"local/{BUCKET}/{repo}/")
    shutil.rmtree(env["KIST_CACHE_DIR"], ignore_errors=True)
    run(env, [KIST, "init"])
    groups = [repo] + ([S3_SOURCE] if kind == "s3" else [])
    runs = [("first", []), ("unchanged", [])] + ([("--trust-mtime", ["--trust-mtime"])] if kind == "sftp" else [])
    for label, extra in runs:
        (out, dt, peak), counts = traced(env, [KIST, "--json", "backup", url, *extra], groups)
        s = json.loads(out)
        stats, rep = s["stats"], s["report"]
        assert stats["files"] == nfiles, stats
        if label != "first":
            assert rep["chunks_new"] == 0 and rep["packs_new"] == 0, rep
        reused = rep["files_reused"]
        if kind == "s3" and label == "unchanged":
            assert reused == nfiles, f"etag and size should prove every object: {rep}"
        if kind == "sftp" and label == "unchanged":
            assert reused == 0, f"no fast path for sftp without --trust-mtime: {rep}"
        if label == "--trust-mtime":
            assert s["mtime_trust"]["trusted"], s["mtime_trust"]
            assert reused == nfiles, f"two full reads came first; every file should be trusted: {rep}"
        rows.append((kind, label, dt, peak, reused, total(counts[repo]),
                     total(counts[S3_SOURCE]) if kind == "s3" else None))
    shutil.rmtree(OUT, ignore_errors=True)
    _, t_restore, peak_restore = run(env, [KIST, "restore", "latest", OUT])
    restored = os.path.join(OUT, *url.split("://", 1)[1].split("/"))
    p = subprocess.run(["diff", "-r", SRC, restored], capture_output=True, text=True)
    assert p.returncode == 0, f"restore differs from the source: {p.stdout[:2000]}"
    print(f"diff -r {SRC} {restored}: identical", flush=True)
    shutil.rmtree(OUT)
    _, t_check, _ = run(env, [KIST, "check"])
    rows.append((kind, "restore", t_restore, peak_restore, None, None, None))

print("\n=== SUMMARY (remote to remote, repo on MinIO) ===")
print(f"{'source':<6} {'backup':<14} {'time':>8} {'peak RSS':>9} {'reused':>8} {'repo reqs':>10} {'source reqs':>12}")
for kind, label, dt, peak, reused, repo_reqs, src_reqs in rows:
    show = lambda v: "-" if v is None else str(v)
    print(f"{kind:<6} {label:<14} {dt:>7.1f}s {peak:>6} MiB {show(reused):>8} {show(repo_reqs):>10} {show(src_reqs):>12}")
print("ALL R2R ACCEPTANCE CHECKS PASSED")
