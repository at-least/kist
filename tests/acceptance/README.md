# 驗收腳本（M1 本機 / M2 S3）

在 repo 之外的目錄跑（資料量約 25 GiB，不要放 tmpfs）：

```sh
mkdir -p ~/kist-acceptance && cp tests/acceptance/*.py ~/kist-acceptance/
cd ~/kist-acceptance
python3 gen.py src                                   # 10 萬檔、約 10 GiB
python3 -u run.py /path/to/kist/target/release/kist      # backup → restore → diff -r → check → 破壞偵測
```

S3 版：先 `eval "$(sh tests/minio-setup.sh)"` 起 MinIO，再 `python3 -u run_s3.py /path/to/kist`
（物件計數與破壞 pack 都經由容器內的 `mc`）。

`run.py` 每一步都有斷言，任何一步失敗就非 0 結束。`rss.py` 可以單獨包住一個命令量它的峰值 RSS。
`m1-acceptance-2026-09-04.log` 是 M1 驗收當天的完整輸出；`m1-acceptance-2026-09-05.log` 是審查修正後（commit `26637f0`）的重跑；`m2-acceptance-s3-2026-09-05.log` 是 M2 對 MinIO 的驗收。

M3（GC）：`python3 -u run_gc.py /path/to/kist` —— backup → 刪掉一成的目錄再 backup → forget 舊的 →
`prune --grace 0s`（標記 + repack）→ 再 backup → 再 prune（刪除）→ `check --read-data` → restore 與 `diff -r`。
`m3-acceptance-gc-2026-09-05.log` 是當天的輸出。
