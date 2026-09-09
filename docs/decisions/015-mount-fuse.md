# ADR 015：`kist mount`——把 snapshots 掛成唯讀檔案系統（FUSE）

日期：2026-09-07　狀態：已採用

## 背景

M4 尾包最後一項：`mount`（Go 參考實作有可對照的實作，`internal/mount`，746 行）。
使用情境：備份完了想直接翻舊檔案、比對、拷貝幾個檔案出來——不整個 restore。

環境：Linux（fusermount3）與 macOS。Rust 側選 `fuser` 0.18（純 Rust、無 libfuse
依賴；0.18 的 `Filesystem` trait 方法是 `&self`，天生支援多執行緒併發 dispatch）。

## 決策

### 1. 佈局與快取（對齊 Go 參考實作）

掛載點下的樹：`<client id hex>/<timestamp>/<備份的樹>`。

- 頂兩層 **volatile**（TTL 1 秒，每次都重列）——新 backup 落地後 `ls` 就看得到；
- snapshot 內容 **immutable**（TTL 24 小時）——內容定址、永不改變；
- 備份來源路徑是**絕對路徑**（`/tmp/x/src`），根 tree 條目展開成虛擬的中介層級
  （`tmp` → `x` → `src`，見 `vpath.rs`）。Go 的 `expandRoots` 在「真實條目與合成
  條目同名」時兩筆都留（違反它自己的註解）；Rust 版讓真實條目直接替換合成條目。
- 只讀：`MountOption::RO` + `DefaultPermissions`（kernel 依 mode 檔權限）；
  寫入類操作（open O_WRONLY、mkdir、setattr…）一律回 EROFS。

### 2. 隨機讀：用 index 的 `raw_len`，不往前解碼（與 Go 的差異）

樹的 chunk 清單只有 id、沒有每顆 chunk 的明文長度；但 **index 的 `PackEntry`
記了 `raw_len`**（明文長度）。mount 在第一次讀檔時從 index 建邊界表
（`ChunkOffsets`），之後任何 offset 都是「二分搜尋 + 抓一顆 chunk + 切片」。
Go 版沒用這個資訊，讀檔尾要先把前面全部解碼一遍。間接內容（大檔的 chunk
清單本身是個資料塊）在第一次讀時解析成資料 chunk 清單再建表。

### 3. 執行模型（fuser callback × tokio）

fuser 的 callback 是同步 `&self`，n 條 event loop（Linux 4 條）併發收發。非同步
工作放進 `mount()` 另建的 multi-thread runtime（worker = event loop × 2），callback
用 `Handle::block_on` 等。兩條鐵律（advisor 確認）：

- **worker 必須多於 event loop**：park 在 `block_on` 的 fuser 執行緒不算勞動力；
- **鎖不跨 `.await`**：`FsCore` 內所有 `std::sync::Mutex` 都是「拿鎖 → clone →
  放鎖 → await」，`tokio::sync::OnceCell` 才承載跨 await 的 once 語意。

生命週期：`Mounted` 持有內部 runtime（`_rt`）——不能在 `mount()` 結束時 drop
（async context 裡 drop runtime 會 panic），也不能在 `unmount()` 的 Drop 路徑
drop——`Drop for Mounted` 把 runtime 丟到獨立執行緒收。`FsCore::new`（載 index）
在**呼叫端**的 runtime 上 await；`mount()` 本身因此是 async。

### 4. 新 snapshot 的可見性

`seen` 集合記看過的 snapshot key；瀏覽時發現新 key → 主動重載 index（限流
1 秒一次；key 一律記為看過，重載失敗不重試——免得壞後端把每次瀏覽變成
reload 風暴）。讀取途中 chunk 因 prune 的 repack 搬家 → `read_chunk_reloading`
的既有限流重載兜底。

### 5. 語意取捨（對齊 Go）

- **唯讀**，沒有任何寫入路徑；
- 硬連結各自成檔（`nlink=1`）：mount 服務 bytes 與 modes，不做 inode 身分；
- xattr 透過 `getxattr`/`listxattr` 露出（樹裡有記的才會出現）；
- chunk 快取：LRU 8 顆解密後 chunk（8 MiB chunk 時 64 MiB 上限）；
- inode 號永不重用 → generation 恆 0、forget 是 no-op；記憶體上限 =
  本 session 瀏覽過的條目數。

## 程式結構

- `crates/kist-mount`（新 crate）：`vpath.rs`（虛擬層級，純）、`offsets.rs`
  （chunk 邊界表，純）、`corefs.rs`（`FsCore`：inode 表 + 瀏覽 + 讀取，
  **不知道 FUSE 存在**，直接對真 repo 測）、`fuse.rs`（fuser 轉接層，`#[cfg(unix)]`）。
- `kist-core`：`read_tree_chain` 轉 pub；`ReloadableIndex` 加 `raw_len_reloading`
  （mount 建邊界表用）與 `store`（主動重載）。
- `kist-cli`：`mount <mountpoint>` 子命令，`#[cfg(unix)]`（Windows 沒有這個命令，
  與 Go 的 build tag 同步）；SIGINT/SIGTERM → fusermount 卸載 → exit 0。

## 測試

- 單元（純邏輯）：`vpath` 展開/排序/二分搜尋/非 UTF-8 名稱、`offsets` 邊界與
  跨 chunk 讀的數學；
- `tests/fs_read.rs`：**不掛載**，直接驅動 `FsCore` 對真 repo（local 後端）：
  瀏覽、全檔與跨 chunk 偏移讀的位元組一致、indirect content（8 MiB → >256
  chunks）、symlink、xattr、各種 NotFound；
- `crates/kist-cli/tests/cli.rs::mount_round_trip`（`KIST_TEST_FUSE=1`）：
  起 `kist mount` **子程序**、經 kernel 讀（含 offset 讀）、SIGTERM → exit 0 →
  掛載點淨空。
- CLI 手動驗證：mount → `ls`/`cat`/`md5sum`（3 MiB 隨機檔全檔一致）→
  `dd skip=2M` 偏移讀一致 → 4 路併發 md5 全同 → 寫入回
  "Read-only file system" → Ctrl-C 乾淨卸載。

## 已知限制

- **不驗 kernel page cache 之外的即時性**：snapshot 內容 TTL 24 小時；
  新 snapshot 靠頂兩層的 volatile TTL 露出（新掛載或重新 `ls` 就看得到）。
- 掛載時有檔案開著 → 卸載 EBUSY（FUSE 標準行為）。
- 外部 `fusermount3 -u` 卸載後 kist 行程不會自己結束，仍以 Ctrl-C/SIGTERM
  收尾（fuser 的 `BackgroundSession` 沒有「session 已結束」的等待 API）。
- Windows 無此命令（fuser 不支援；Go 參考實作同樣只在 linux/darwin）。
- `check`/`prune` 與 mount 同時跑安全：mount 只讀，讀到被 repack 搬走的
  chunk 會觸發 index 重載（限流）。

## 驗證摘要

- `cargo test --workspace`（KIST_TEST_FUSE=1、KIST_TEST_RCLONE=1、docker SFTP
  環境）：264 passed / 0 failed；clippy `-D warnings`、`cargo fmt --check` 乾淨。
- 手動 E2E 見上（CLI 手動驗證段）。
