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

## 結果（v4，2026-10-18，16 核 / 62 GiB / ext4，release build，M12 第 7 段）

`run-v4.sh` 一次跑完（log 在 `~/kist-bench/run-v4-2.log`，不進版控）。數字是量測跑法的 `sampled_max_ex_file_slab`，
括號是門檻跑法的同一個數字；「殺」= `MemoryMax=512M` 下被 OOM 殺掉。兩個測試集都是 100 萬檔、100 萬個 chunk
（A、B 都是 16 個 pack、5 個 mpack）。

| 命令 | A 集（1000 dir × 1000 檔） | B 集（單一平面 1M 檔） | 512 MiB gate |
|---|---|---|---|
| backup，第一次（全部新寫） | 326 MiB，90 s | 362 MiB，68 s | A 過（285）、B 過（354） |
| backup，第二次（快速路徑，0 新 chunk） | 150 MiB，41 s | 278 MiB，18 s | A 過（141）、B 過（281） |
| prune，第一輪（收錄全部 pack）／第二輪 | 604／638 MiB，10 s | 587／638 MiB，8 s | **A、B 都被殺** |
| `check` | 228 MiB，6 s | 238 MiB，6 s | 過（262／236） |
| `check --read-data` | 473 MiB，14 s | 428 MiB，10 s | 過（432／443） |
| `restore latest` | 148 MiB，74 s | 538 MiB，84 s | A 過（156）、**B 被殺**（開始 2 秒內） |

- backup 的目標（M5：< 512 MiB）v4 仍然達到，A 集比 v3 的 373–431 MiB 低。
- **prune 超過門檻**：v3 的 `prune --dry-run` 在 A 集是 550 MiB（下表，當時記為「沒做」），v4 是 587–638 MiB。
  dhat 歸因見 ADR 026「實作第七段」；修法留給負責人。
- **restore 單一平面目錄的 100 萬個項目超過門檻**（A 集的 1000 個目錄不會）：dhat 在 `open_placement` 量到一個 336 MiB
  的配置——整個目錄的 100 萬個 `Entry` 一次解進記憶體。第一次量的 restore 是 362 MiB（A）／789 MiB（B）：dhat 另外
  抓到讀取端白記的「驗證過的 (item, pack)」集合（195 MiB HashSet 加 64 MiB Vec，只有 backup 會寫回快取），改成只有
  backup 的讀取端才記之後（M12 第 7 段第 10 步）是上表的 148／538 MiB。v3 只量過 A 集的 restore（71 MiB）。
- `check --read-data` 在兩個集上離門檻都不到 100 MiB（門檻跑法 432／443 MiB）；dhat 歸因見 ADR 026「實作第七段」。

## 結果摘要（v3，2026-09-06，16 核 / 61 GiB / ext4，release build）

| 測試集 | 修正前 | 修正後（N≥3 取最大） | 512 MiB gate |
|---|---|---|---|
| A（1000 dir × 1000 檔） | 1.17–1.9 GiB | 373–431 MiB | 3/3 通過 |
| B（單一平面 1M 檔） | 1.69 GiB | 442–524 MiB | 4/4 通過 |
| restore（A 集快照） | — | 71 MiB（非門檻記錄） | — |
| prune --dry-run（A repo） | — | 550 MiB（非門檻記錄，見 ADR「沒做」） | — |

修正前後的差異與根因清單見 ADR 011。
