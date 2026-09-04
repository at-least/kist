# ADR 003：M1 審查後的修正

- 狀態：已採用
- 日期：2026-09-05
- 里程碑：M1（審查階段）

## 背景

M1 完成後做了兩層審查：advisor（看得到整段開發脈絡）與一個全新的 reviewer agent
（只拿到 `docs/format.md` 與程式碼、不知道任何設計理由——用它取代 PLAN.md 要求的獨立
review session，負責人同意）。reviewer 回報 13 項、每項附實跑的 probe。這份 ADR 記錄
修正時做的幾個「有取捨」的決定；純 bug 修正看 commit 訊息就夠。

## 決定

### 1. tree 每次 backup 都重新 put，不先列表

原本 backup 開始時列出 `trees/`，名稱已存在就不上傳。reviewer 指出兩個問題：
壞掉的 tree 永遠不會被修回來（名字還在），以及 M3 的 GC 若在 backup 進行中刪掉 tree，
列表已經過期、snapshot 會指向不存在的東西。

tree 是 content-addressed、同名一定同 bytes，put 是冪等的，所以改成一律 put。
代價：每個目錄一次 put。本機後端幾乎免費；S3 上 10 萬個目錄的 repo 每次 backup 多
10 萬次 PUT（約 0.5 USD）。M2 有本地快取後，用 `cache_id` 記住「這台機器已寫過的 tree」
就能省掉，同時保有自我修復（快取是本機的，壞掉的遠端 tree 不會被快取遮住——
`check` 發現壞 tree 時清快取即可）。

### 2. 讀不到的東西：跳過、記錄、snapshot 照寫、結束碼非 0

原本一個 permission denied 讓整個 backup 失敗、什麼都沒寫出來。改成 restic 的做法：
警告、`SnapshotStats.errors` 計數、snapshot 仍然寫出（不含那些項目），CLI 以非 0 結束
讓排程器知道要看警告。理由：一個讀不到的檔案不該讓其他 99.9% 的資料沒有備份。
目前「有略過」與「硬失敗」都是結束碼 1；restic 用 3 區分前者，之後再分。

### 3. restore 單檔失敗不中止，殘檔移除

同樣的道理反過來：一個壞掉的 chunk 不該讓其他檔案也拿不回來。restore 回傳
`RestoreSummary`，錯誤逐筆列出，CLI 以非 0 結束。寫到一半失敗的檔案會被刪掉，
不留下「看起來完整」的殘檔。

### 4. snapshot 的時間 = backup 開始時間

快速路徑靠「檔案的 mtime/ctime 早於上一次 backup 開始」來排除 racily-clean 的檔案
（在被讀取的同一個時間刻度內又被改、metadata 看起來沒變）。第一版把 snapshot 時間
記成寫出 snapshot 的時間（所有檔案讀完之後），窗口根本沒關上。現在 `time` 與 key 的
時間戳都來自 backup 開始的那個瞬間；key 撞到就往後推 1 ns 重試。

### 5. 大檔的記憶體：每輪最多封一個 pack

原本一個檔案的切塊全在一個 blocking closure 裡跑完，封好的 pack 全部留在記憶體等
closure 結束才上傳：10 GiB 的 VM image 要 10 GiB RAM。改成 closure 一封好 pack 就回到
async 端上傳，切塊狀態在 closure 之間移進移出。實測 512 MiB 單檔的 RSS 成長從
736 MiB 降到 22 MiB。

### 6. 明文 config 的防護

見 ADR 002 §10：`repo_id` + chunker 綁進 master key 的 AAD、所有明文參數讀進來先驗範圍、
KDF 參數有上限、快取 ID 從 master key 派生。這是 M1 唯一的不相容改動（之前的 repo
打不開；當時沒有真實 repo）。

## 沒修、留到 M2 或記為設計限制

- `rebuild-index`（壞掉的 index blob 目前讓 backup/restore 都不能用）：M2。
- Windows 的 symlink 一律 `symlink_file`（指向目錄的 symlink 種類錯）：M2，需要 Windows 環境驗證。
- 回滾／刪除攻擊不可偵測、key 明文洩漏時間、wall clock 排序、client id 被複製：
  寫進 `docs/format.md` §12，對策在 M2（Object Lock、本機記住最後的 snapshot key）。
- `hostname()` 在 macOS 會是 "unknown"：M2 換 `gethostname` crate。
