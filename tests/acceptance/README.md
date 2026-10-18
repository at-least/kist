# 驗收腳本（M1 本機 / M2 S3 / M3 GC；v4 版，M12 第 7 段重寫）

在 repo 之外的目錄跑（資料量約 25 GiB，不要放 tmpfs）：

```sh
mkdir -p ~/kist-acceptance && cp tests/acceptance/*.py ~/kist-acceptance/
cd ~/kist-acceptance
python3 gen.py src                                   # 10 萬檔、約 10 GiB
python3 -u run.py /path/to/kist      # backup → 第二次 backup（0 個新 pack）→ restore → diff -r → check → check --read-data → 破壞偵測
```

S3 版：先 `eval "$(sh tests/minio-setup.sh)"` 起 MinIO，再 `python3 -u run_s3.py /path/to/kist`
（物件計數與破壞 pack 都經由容器內的 `mc`；第二次 backup 與 `rebuild` 之後再一次沒變的 backup 都包在
`mc admin trace --json` 裡數 S3 請求，依 API 分類印出）。

`run.py` 每一步都有斷言，任何一步失敗就非 0 結束。`rss.py` 可以單獨包住一個命令量它的峰值 RSS。
`m1-acceptance-2026-09-04.log` 是 M1 驗收當天的完整輸出；`m1-acceptance-2026-09-05.log` 是審查修正後（commit `26637f0`）的重跑；`m2-acceptance-s3-2026-09-05.log` 是 M2 對 MinIO 的驗收。

M3（GC）：`python3 -u run_gc.py /path/to/kist` —— backup → 刪掉一成的目錄再 backup → forget 舊的 →
`settings --grace 2h` 之後三輪**真實時鐘**的 prune（v4 的 grace 最低 2 小時、沒有 `--grace 0s`：第一輪把 pack 收進
map，第二輪等 pack 比 grace 老才標記與 repack，第三輪再等一個 grace 才刪）→ `check --read-data` → restore 與
`diff -r`，整段四個多小時；`KIST_GC_GRACE` 可以調 grace（不能低於 2h），`KIST_GC_MARGIN_SECS` 是每次等待多加的秒數。
`m3-acceptance-gc-2026-09-05.log` 是 v3 時代的輸出（`prune --grace 0s` 已不存在）。

