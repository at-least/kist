# ADR 004：M2 —— S3 後端、本地 index 快取、無鎖並發

- 狀態：已採用
- 日期：2026-09-05
- 里程碑：M2

M2 **沒有改任何 repo 內的 on-disk 格式**（v1 仍然凍結）；改的都是 client 端的行為與本機檔案。

## 決定

### 1. S3 用 `object_store`，TLS 用 ring 而不是 aws-lc-rs

`object_store` 的 `aws` feature 預設帶 `aws-lc-rs`，它是 C 碼，Windows 上要 CMake + NASM
才編得起來。CI 從來沒跑過，不該讓 M2 成為「Windows 因為建置工具而紅」的那個 commit。
改用 `aws-base` + `reqwest`（`rustls-no-provider`）+ `ring`：reqwest 不帶 crypto provider，
後端建構時安裝 ring 的 provider（`install_default`，第二次呼叫會回 Err，忽略）。
實測：MinIO（http）簽章正常、對公開 S3 bucket 的 HTTPS 正常。

### 2. `s3://bucket/prefix` 的 prefix 用 `PrefixStore` 套

`AmazonS3Builder::with_url` 只取 bucket，路徑部分**不會**變成 prefix（從原始碼確認），
所以自己解析 URL、用 `PrefixStore` 包一層。測試驗證：物件在 `<prefix>/config`，不在 bucket 根。

### 3. 「Put-only 權限」的定義

PLAN 寫「backup 只需要 Put 權限」。字面上做不到：開 repo 就要 `GET config`、讀 index blob、
找 parent snapshot。M2 採的定義是**抗勒索的意圖**：backup 帳號有 `PutObject`、`GetObject`、
`ListBucket`，**沒有 `DeleteObject`**——被入侵的備份機器加不了「刪除」這件事。
測試兩面都驗：受限帳號能完成整個 backup + check，`delete` 回 AccessDenied 且物件還在。

S3 擋不住「覆寫」：`config` 是唯一可覆寫的物件，被蓋掉 repo 就打不開（資料還在）。
對策在 bucket 設定：versioning 或 Object Lock；`init` 對 s3 repo 會印提醒。
真正免 List 的模式（本機記住最後一個 snapshot key）留給以後，是嚴格的改進不是必要。

### 4. 本地 index 快取：排序表 + `pread`，不用 mmap

PLAN 寫「mmap 的 sorted table」。`memmap2` 的 `Mmap::map` 是 `unsafe fn`，而所有 crate 都
`forbid(unsafe_code)`；mmap 的好處（不整份載入、靠 OS page cache）用「固定長度紀錄 +
二分搜尋 + 每步 `read_exact_at` 一筆」一樣拿得到，零 unsafe。**這是與 PLAN 字面的偏離**，
效果等價。

快取目錄以 `hash(cache_id ‖ repo 位置)` 命名：`cache_id` 從 master key 派生（明文 `repo_id`
可以被換掉，見 ADR 002 §10），加上位置是為了「同一個 repo 複製到第二個地方」不共用快取——
第二份可能少了東西，共用會讓 client 以為 chunk 存在而不上傳。

更新規則刻意保守：只有「manifest 裡的 blob 全都還在、新 blob 沒有 supersede 舊的」才增量合併，
其他情況一律重建。`check` 永遠不用快取——它的工作就是抓快取會遮住的問題。

已知限制：pack 被刪但 index blob 還在，快取（和 repo 的 index 一樣）看不出來；
M3 的 `gc/` 標記與「inactive client 回來要重新驗證 pack 存在」規則要處理這件事，
快取設計留了空間（manifest 記錄 pack 清單）。

### 5. `rebuild-index` 用 `supersedes`，不刪舊 blob

每個 pack 只 range read 檔尾 16 bytes 與 trailer。新 blob `supersedes` 開始時存在的所有 blob；
重建途中別的 client 寫出的 blob 不在名單裡、照常保留。舊 blob 不刪：刪除是 M3 GC 的事，
而且 Put-only 帳號本來就刪不了。

### 6. 並發正確性靠格式，不靠鎖

兩個 client 同時 backup：pack / tree 以內容命名、snapshot 以 client id 分 namespace 並用
conditional put、index 各寫各的 blob。最壞情況是同一個 chunk 被兩邊各存一份（多佔空間，
GC 可以收），不會互相覆蓋。測試：同時 backup 有重疊的資料 → `check --read-data` 無錯、
兩邊 byte-for-byte 還原、A 再備份 B 的資料 `chunks_new == 0`（證明跨 client 的 index 合併有效）。

### 7. 結束碼照 restic

0 成功；1 失敗；3「完成但有略過或部分還原失敗」（snapshot 已寫出）。排程器看到 3 該去看警告。

## 沒做（留給之後）

- 10 GiB 對 S3/MinIO 的效能量測（功能與正確性有測，吞吐量還沒量）。
- 免 List 的純 Put 模式；本機記住最後 snapshot key 以偵測回滾（M3/M4）。
- Windows 上 symlink 指向目錄時的種類（需要 Windows 環境）。
