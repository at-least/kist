# 驗收腳本（M1 本機 / M2 S3 / M3 GC；v4 版，M12 第 7 段重寫）

在 repo 之外的目錄跑（資料量約 25 GiB，不要放 tmpfs）：

```sh
mkdir -p ~/kist-acceptance && cp tests/acceptance/*.py ~/kist-acceptance/
cd ~/kist-acceptance
python3 gen.py src                                   # 10 萬檔、約 10 GiB
python3 -u run.py /path/to/kist      # backup → 第二次 backup（0 個新 pack）→ restore → diff -r → check → check --read-data → 破壞偵測
```

S3 版：先 `eval "$(sh tests/minio-setup.sh)"` 起 MinIO，再 `python3 -u run_s3.py /path/to/kist`
（物件計數與破壞 pack 都經由容器內的 `mc`；第二次 backup、`prune` 收錄進 map 之後的兩次沒變的 backup 都包在
`mc admin trace --json` 裡數 S3 請求，依 API 分類印出。LIST 的 trace 記錄裡 path 是 bucket 本身、repo 的前綴在
query 的 `prefix`，計數兩個都看；2026-10-19 之前只看 path，漏了 LIST）。

Remote-to-remote 版（M12 第 7 段之三）：兩個容器都起好（`eval "$(sh tests/minio-setup.sh)"`、
`eval "$(sh tests/sftp-setup.sh)"`），把同一份 `src` 放進去，再跑 `python3 -u run_r2r.py /path/to/kist`：

```sh
docker cp src kist-sftp:/home/kisttest/repo/m1 && docker exec kist-sftp chown -R kisttest /home/kisttest/repo/m1
docker cp src kist-minio:/tmp/m1 && docker exec kist-minio sh -c 'mc cp --recursive --quiet /tmp/m1/ local/kist-test/m1-source/ && rm -rf /tmp/m1'
```

SFTP 來源（`$KIST_TEST_SFTP_URL/m1`）與 S3 來源（`s3://$KIST_TEST_S3_BUCKET/m1-source`）各自備份進 MinIO 上的一個
S3 repo：第一次、沒變的第二次（S3 要全部以 etag 沿用、SFTP 要全部重讀），SFTP 再加一次 `--trust-mtime`（要全部
沿用），之後 restore、對本機的 `src` 做 `diff -r`、`check`。每個 backup 印時間、峰值 RSS、repo 與 S3 來源各自的請求
數。平的 100 萬檔（記憶體 bench 的 B 集）以同樣的方法放進容器，用 `bench/memory/run-remote.sh` 量記憶體。

`run.py` 每一步都有斷言，任何一步失敗就非 0 結束。`rss.py` 可以單獨包住一個命令量它的峰值 RSS。
`m1-acceptance-2026-09-04.log` 是 M1 驗收當天的完整輸出；`m1-acceptance-2026-09-05.log` 是審查修正後（commit `26637f0`）的重跑；`m2-acceptance-s3-2026-09-05.log` 是 M2 對 MinIO 的驗收（以上 v3）。
v4：`m1-acceptance-v4-2026-10-18.log`、`m2-acceptance-s3-v4-2026-10-18.log` 是 `2e6b20b`（後者的請求數漏了 LIST）；
`m1-acceptance-v4-2026-10-19.log`、`m2-acceptance-s3-v4-2026-10-19.log` 是 `b48e007`（沒變的 backup 加速之後；LIST 有數）。
`r2r-acceptance-2026-10-19.log` 是 remote-to-remote 版（`7cf9e2d`），`r2r-acceptance-392eb54-2026-10-19.log` 是第十三步之後的
重跑（S3 來源的 LIST 1 101：root 不再列兩次），`r2r-acceptance-10c1eac-musl-2026-10-20.log` 是以發佈設定（`cargo zigbuild
--target x86_64-unknown-linux-musl`，同 `dist.sh`）跑的（M12 第 7 段之四）。`run_r2r.py` 要在放著 `src` 的目錄旁跑（它以腳本所在
的目錄找 `src`：從 repo 裡的 `tests/acceptance` 跑會數到 0 個檔案）。`run_s3.py` 與 `run_r2r.py` 在容器裡記下
`mc admin trace` 的 PID、數完就在容器裡 kill（殺掉本機的 `docker exec` 停不了它，留下來的會讓之後的 trace 收不到東西）。
`run_s3.py` 改了之後重跑的 M2 是 `m2-acceptance-s3-v4-recount-2026-10-19.log`（`7cf9e2d`）：請求數 13／77／10，與
`b48e007` 那次的 13／76／10 只差 prune 之後那一次的 map 分片數（分片依 pack 名稱的第一個 byte，每次不同；這次 67 片）。

M3（GC）：`python3 -u run_gc.py /path/to/kist` —— `init --grace 2h`（v4 的 grace 最低 2 小時、沒有 `--grace 0s`；
要在 init 給：init 之後再調低是排空，72 小時內的標記都照 72 小時算）→ backup → 刪掉一成的目錄再 backup → forget 舊的
→ **真實時鐘**的 prune（第一輪把 pack 收進 map；等 pack 比 grace 老之後跑兩輪——v4 的時間只比 head，第一輪寫的 head
才把「現在的 T」推到現在、第二輪才標記與 repack；再等一個 grace 又兩輪，第二輪才 tombstone 並刪）→ `check --read-data`
→ restore 與 `diff -r`，整段四個多小時；`KIST_GC_GRACE` 可以調 grace（不能低於 2h），`KIST_GC_MARGIN_SECS` 是每次
等待多加的秒數。這份資料集裡刪掉的目錄的 pack 都還剩幾個跨目錄相同的小檔的 chunk，所以走的是 P7c 的 repack（舊 pack
一樣標成 condemned、之後刪除），不是 P7b 整個 pack 的標記；P7b 由 `crates/kist-core/tests/v4_prune.rs` 蓋。
`m3-acceptance-gc-2026-09-05.log` 是 v3 時代的輸出（`prune --grace 0s` 已不存在）；`m3-acceptance-gc-v4-2026-10-18.log`
是 v4 的第三次跑（binary `9acbafb`，19:29–23:35；前兩次失敗的原因在 `run_gc.py` 開頭的說明）。

