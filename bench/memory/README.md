# 記憶體量測（M5：100 萬檔 backup 峰值 < 512 MiB；M12 第 7 段重量 v4）

量測 `kist backup`（與 v4 的 prune、check、restore）在大型測試集上的峰值記憶體。方法見
`docs/decisions/011-m5-memory.md`，v4 的結果見下與 `docs/decisions/026-v4-format.md`「實作第七段」；這裡放操作方式。

## 元件

- `generate.py <目標目錄> <a|b> [seed] [dirs per_dir]`：seed 產生 100 萬檔測試集。
  - layout `a`：dirs × per_dir（預設 1000 × 1000），常態形
  - layout `b`：單一平面目錄塞全部檔案，parent tree 鏈與 `read_dir` 列表的最壞形
  - 檔案 768–1279 bytes（全部單一 chunk → ~100 萬 chunk），內容為 sha256 鏈
    （不可壓縮、每檔唯一）；結尾附空檔/symlink/hardlink/unicode 名/深路徑/5 MiB 檔
- `run-measured.sh <label> <outdir> [--max-bytes N] <cmd...>`：把指令放進
  `systemd-run --user --scope`（可用 `-p MemoryMax=N -p MemorySwapMax=0` 做硬門檻），
  每 50 ms 取樣 cgroup 的 `memory.current` / `memory.stat`。
  報告兩個數字：
  - `sampled_max_ex_file` = current − file（含可回收 slab）
  - `sampled_max_ex_file_slab` = current − file − slab_reclaimable（**報告用的產品數字**：
    走訪 100 萬檔會在 cgroup 記下 ~400 MiB 的 dentry/inode cache，那不是 process 記憶體）
  - 真正的驗收是 `--max-bytes 536870912`（512 MiB）跑法不被殺：kernel 會先回收
    clean page cache，過了才是真的過
- `run-baseline.sh <kist-bin> <workdir> <setdir> <label> [--gate]`：init + first + second backup 各量一次。
- `run-prune.sh <kist-bin> <workdir> <label> <n> [--gate]`：在 run-baseline.sh 建好的
  repo 上量 `prune` n 次（v4 沒有 dry run：每輪寫一個世代；第一輪把全部 pack 收進 map，之後幾輪沒有到期的
  東西；--gate = 512 MiB 硬門檻，樣本檔名加 `gate-`）。
- `run-read.sh <kist-bin> <workdir> <label> [--gate]`：同一個 repo 上量 `check`、`check --read-data`、
  `restore latest`。
- `run-v4.sh <kist-bin> <workdir> <set-a> <set-b>`：M12 第 7 段的整套（兩個測試集各做 backup ×2 的量測與門檻
  跑法、prune ×2 量測與 ×1 門檻、三個讀取命令的量測與門檻），門檻跑法被 OOM 殺掉只記錄不中斷，最後印出全部
  `.result`。`run-measured.sh` 在 scope 被殺時也會寫 `.result`（`exit_code` 143、`oom_kill_last_seen` unknown：
  events 檔來不及留）。

## 流程

```sh
# 1. 產生測試集（各約 1 GiB 資料 / ext4 上 ~4 GiB block）
python3 bench/memory/generate.py /path/set-a a kist-memtest-1
python3 bench/memory/generate.py /path/set-b b kist-memtest-1
# 2. 量測（backup ×2：first 走新寫、second 走 parent 快速路徑）
sh bench/memory/run-baseline.sh target/release/kist /path/work /path/set-a A
# 3. 硬門檻
sh bench/memory/run-baseline.sh target/release/kist /path/work /path/set-a A --gate
# 4. prune（在同一個 repo 上）
sh bench/memory/run-prune.sh target/release/kist /path/work A 3 --gate
# 5. v4 整套
sh bench/memory/run-v4.sh target/release/kist /path/work /path/set-a /path/set-b
```

注意：機器要閒置；測試集放 ext4（tmpfs 的 shmem 頁不可回收，會灌大數字）。

## dhat 歸因

`cargo build --release --features kist-cli/dhat` 會用 dhat 分配器編出二進位，
跑完在 cwd 寫 `dhat-heap.json`（Total / t-gmax live / t-end 三個數字加
allocation site 樹）。歸因用，量測數字一律以 dhat **關閉**的 build 為準。

## 結果（v4，2026-10-19，16 核 / 62 GiB / ext4，release build，M12 第 7 段之二）

`run-v4.sh` 以 `b48e007` 的 binary 一次跑完（log 在 `~/kist-bench/run-v4-2026-10-19.log`，不進版控）。數字是量測跑法的
`sampled_max_ex_file_slab`，括號是 `MemoryMax=512M` 門檻跑法的同一個數字；這一輪 26 次全部 exit 0、沒有 OOM。兩個測試集
都是 100 萬檔、100 萬個 chunk。

| 命令 | A 集（1000 dir × 1000 檔） | B 集（單一平面 1M 檔） | 512 MiB gate |
|---|---|---|---|
| backup，第一次（全部新寫） | 264 MiB，54 s | 314 MiB，63 s | 過（292／394） |
| backup，第二次（快速路徑，0 新 chunk） | 143 MiB，12 s | 285 MiB，20 s | 過（149／285） |
| prune，第一輪（收錄全部 pack）／第二輪 | 233／161 MiB，7／6 s | 236／171 MiB，6／5 s | 過（第三輪：159／168） |
| `check` | 229 MiB，6 s | 240 MiB，5 s | 過（230／271） |
| `check --read-data` | 441 MiB，10 s | 436 MiB，10 s | 過（440／432） |
| `restore latest` | 151 MiB，79 s | 151 MiB，98 s | 過（156／161） |

- M5 的目標（backup < 512 MiB）v4 仍然達到；負責人 2026-10-19 決定門檻也訂給 prune 與 restore，兩個現在都在門檻內。
- **prune** 原本（第 7 段，`9acbafb`）A 集 676 MiB、B 集 587–638 MiB，門檻下都被殺；dhat 的歸因是以 32 bytes 的 ID 當鍵的
  兩張 HashMap 與每個階段各 clone 一份的記錄集合。M12 第 7 段第十二步（`668c02c`）改成記錄只留一份、以位置索引取代
  HashMap（ADR 026「實作第七段之二」）。第一輪比之後幾輪高，是因為它讀全部 pack 的 trailer、寫全部的分片。
- **restore** 單一平面目錄的 100 萬個項目原本 529 MiB、門檻下被殺（`open_placement` 一次把整個目錄的 `Entry` 解進
  記憶體，dhat 量到 336 MiB 的單一配置）；第十三步（`479871b`）改成一段一段讀目錄。再之前第九步（`55f8e56`）拿掉了
  讀取端白記的驗證集合（362 → 148 MiB，A 集）。
- 離門檻最近的是 `check --read-data`（量測 441／436 MiB）：正在驗的 pack 在記憶體裡兩份、走過的 chunk ID 的 HashSet、
  map 分片解碼與記錄的 clone（dhat 歸因見 ADR 026「實作第七段」）；沒改。第十三步那一輪量到的 B 集門檻跑法是 464 MiB。
- 第二次 backup A 集 41 → 12 s：第十六步（快速路徑的 B2 不再每個檔案換一次執行緒）與第十七步（每個條目只 lstat 一次）
  拿掉的就是每個檔案的成本，在 M1 的資料集上各自量過（ADR 026「實作第七段之二」）；這裡沒有逐步量。第一次 backup 也從
  90 變 54 s，那不是這兩步能解釋的——兩輪之間機器的狀態（page cache）不同，時間只當參考，記憶體才是這份 bench 的數字。

第 7 段（2026-10-18，`2e6b20b`／`55f8e56`）的舊表在 git 的歷史裡（`git show 0f679e8:bench/memory/README.md`）。

## 結果摘要（v3，2026-09-06，16 核 / 61 GiB / ext4，release build）

| 測試集 | 修正前 | 修正後（N≥3 取最大） | 512 MiB gate |
|---|---|---|---|
| A（1000 dir × 1000 檔） | 1.17–1.9 GiB | 373–431 MiB | 3/3 通過 |
| B（單一平面 1M 檔） | 1.69 GiB | 442–524 MiB | 4/4 通過 |
| restore（A 集快照） | — | 71 MiB（非門檻記錄） | — |
| prune --dry-run（A repo） | — | 550 MiB（非門檻記錄，見 ADR「沒做」） | — |

修正前後的差異與根因清單見 ADR 011。
