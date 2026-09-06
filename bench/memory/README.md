# 記憶體量測（M5：100 萬檔 backup 峰值 < 512 MiB）

量測 `kist backup` 在大型測試集上的峰值記憶體。方法與結果見
`docs/decisions/011-m5-memory.md`；這裡只放操作方式。

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

## 流程

```sh
# 1. 產生測試集（各約 1 GiB 資料 / ext4 上 ~4 GiB block）
python3 bench/memory/generate.py /path/set-a a kist-memtest-1
python3 bench/memory/generate.py /path/set-b b kist-memtest-1
# 2. 量測（backup ×2：first 走新寫、second 走 parent 快速路徑）
sh bench/memory/run-baseline.sh target/release/kist /path/work /path/set-a A
# 3. 硬門檻
sh bench/memory/run-baseline.sh target/release/kist /path/work /path/set-a A --gate
```

注意：機器要閒置；測試集放 ext4（tmpfs 的 shmem 頁不可回收，會灌大數字）。

## dhat 歸因

`cargo build --release --features kist-cli/dhat` 會用 dhat 分配器編出二進位，
跑完在 cwd 寫 `dhat-heap.json`（Total / t-gmax live / t-end 三個數字加
allocation site 樹）。歸因用，量測數字一律以 dhat **關閉**的 build 為準。

## 結果摘要（2026-09-06，16 核 / 61 GiB / ext4，release build）

| 測試集 | 修正前 | 修正後（N≥3 取最大） | 512 MiB gate |
|---|---|---|---|
| A（1000 dir × 1000 檔） | 1.17–1.9 GiB | 373–431 MiB | 3/3 通過 |
| B（單一平面 1M 檔） | 1.69 GiB | 442–524 MiB | 4/4 通過 |
| restore（A 集快照） | — | 71 MiB（非門檻記錄） | — |
| prune --dry-run（A repo） | — | 550 MiB（非門檻記錄，見 ADR「沒做」） | — |

修正前後的差異與根因清單見 ADR 011。
