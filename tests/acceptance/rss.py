"""跑一個命令，每 0.2 秒取樣它的 RSS，印出峰值。"""
import subprocess, sys, time, os
p = subprocess.Popen(sys.argv[1:], env=dict(os.environ))
peak = 0
while p.poll() is None:
    try:
        with open(f"/proc/{p.pid}/status") as f:
            for line in f:
                if line.startswith("VmRSS:"):
                    rss = int(line.split()[1]) // 1024
                    peak = max(peak, rss)
    except FileNotFoundError:
        pass
    time.sleep(0.2)
print(f"exit={p.returncode} peak_rss={peak} MiB", flush=True)
sys.exit(p.returncode)
