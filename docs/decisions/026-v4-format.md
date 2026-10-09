# ADR 026：v4 格式——樹進 pack、root 記種類

日期：2026-10-10　狀態：**草稿，待負責人確認後才實作**。規格全文在 `docs/format-v4.md`；程式碼
仍是 v3，`docs/format.md` 在確認之前仍是權威。

## 背景

- ADR 017（2026-09-26，草稿）提出 v4 只做一項格式改動：把目錄樹從獨立物件搬進 pack（決定 1），
  同一次補 GC 規則（決定 2）。是否採用、派生字串留 `kist/v3` 還是改 `v4`，都待負責人裁定。
- 排除規則與部分還原的審查（ADR 025）留下兩項要改格式才能修的：V19（目錄 root 恰好只含一個同名
  檔案時被還原成檔案）、V21（每個 root 一個樹物件：`--files-from` 列 2000 個檔案要 10.96 秒，同樣
  的檔案放在一個目錄裡是 0.16 秒）。
- 負責人 2026-10-10：同意「先修完審查、合進 main、再開 v4 分支」，並說明「整個系統都還沒有真實
  上線過，相容性完全不重要，可以趁機改」。

這份 ADR 把 v4 的範圍與每個要選的地方寫定。規格全文在 `docs/format-v4.md`，以 `diff
docs/format.md docs/format-v4.md` 對照 v3。

## 決定

### 1. 採用 ADR 017 決定 1：樹進 `mpacks/`

樹段改成 metadata pack 的一項，排版、trailer、index、gc 標記、parity 都與資料 pack 相同。
`trees/`、`touch/`、樹的 `.r1`、樹的 gc 標記、樹的覆寫修復、commit 前的逐樹檢查全部移除。沿用
樹的規則只剩一句：index 查得到、持有者存在且沒被標記才沿用，否則重新寫進新的 mpack，與 chunk
相同（規格 §13.5）。

理由照 ADR 017：資料遺失方向的 bug 有四件出在「樹的存活靠後端 mtime 表達」這一塊，樹改走 pack
已有 200 案 proptest 的生命週期。V21 的成本主要來自每個樹物件各自一次寫入與 fsync（2000 個 root
時 8017 次 fsync 佔 14.2 秒中的 11.1 秒），樹段打包之後這部分消失。這一點要等實作後重量。

### 2. 採用 ADR 017 決定 2：GC 補條款

寫進規格 §13：活躍 client 規則同秒擋住、刪除要嚴格晚於；時鐘偏差自檢；一次一個 prune（主機檔案
鎖），刪前重讀標記；prune 先列 `indexes/`、讀完 index blob，再列 pack；刪除順序 `.r1` → parity →
主體 → 標記；
壞 index blob 在 backup 與 restore 寬鬆跳過、在 prune 嚴格拒絕，另有維護命令刪除；repack 補寫
parity。prune 的持有者集合（含年輕物件規則）也寫成規範，這是 ADR 019 C3 要求、還沒做的。

對 ADR 017 用字的一處修正：017 寫「寫下第一個標記後 HEAD 它」，但只刪不標的一輪不寫標記，會漏掉
檢查。規格改成在刪除之前，HEAD 本輪寫出的第一個物件（標記，或刪除前必寫的 index blob）。

### 3. `Root` 加 `t`，必填

與 `Entry.t` 同碼（0 檔案、1 目錄、2 符號連結），一律輸出，缺席就解碼拒絕。不採「缺席代表目錄」
的零值省略：那樣缺席與檔案的碼會互相混淆，正是 V19 那一類的陷阱。讀取端驗證 `t≠1` 時 tree 恰好
一個類型相符、名稱等於定位最後一段的節點；`t=1` 對節點沒有限制。restore 與 mount 用同一條映射
（規格 §9），不再從內容猜。

### 4. 樹段的位元組上限 `TREE_SEGMENT_MAX` ＝ 8 MiB

樹段進 pack 之後要有解壓與緩衝上限；v3 只限一段 10 000 個節點，xattrs 沒有上限。寫入端在加入下一
個節點會超過任一上限時開新段；單一節點就超過 8 MiB 時記成該節點的錯誤、不寫入。

實測（2026-10-10，以目前的 kist 備份兩個真實目錄）：`/usr/include` 與 `/usr/share/doc` 共
36 476 個節點，樹物件合計 4 654 315 bytes，平均每個節點約 128 bytes。所以 10 000 個節點的一段
約 1.3 MB，一般目錄由節點數上限決定。位元組上限只在直接 chunk 清單很長（每個節點最多 256 × 34
bytes ≈ 8.7 KB，約 950 個節點一段）或 xattrs 很大時起作用。選 8 MiB 是因為它等於 `chunker.max`
的預設值，與資料 chunk 的緩衝同一個量級。這是要負責人看過的數字。

### 5. 樹段用 chunk key 封

照 ADR 017：樹段與 chunk 同一種封裝（`alg byte ‖ CBOR`、可 zstd、AAD 是自己的 ID），pack 的讀寫碼
原樣共用。meta key 從此只用於 snapshot。

### 6. mpack 在 `replicas=1` 時寫 `.r1`

這推翻 v3 §7「pack 不做副本」：parity 是選配、修不了整顆遺失，而沒有樹就什麼都還原不了（ADR 017
條件 2）。資料 pack 照舊不寫副本。刪除順序 `.r1` → parity → 主體 → 標記。

### 7. `IndexPack.kind`

0 ＝ `packs/`、1 ＝ `mpacks/`，其他值拒絕。查 chunk 只看資料 pack 的持有者，查樹段只看 mpack 的
持有者。讀取端保留一個 ID 的全部持有者、依正本順序逐一嘗試；這會改本地快取的固定長度紀錄
（ADR 011），記憶體 bench 要重跑。

### 8. backup 角色零覆寫

pack、mpack、parity、index blob、snapshot 一律 PutIfAbsent。v3 規格就寫 PutIfAbsent，程式卻是
普通 put：資料 pack 在 `backup.rs:2075`，index blob 在 `repo.rs:462`，repack 的新 pack 在
`prune.rs:911` 與 `prune.rs:924`（2026-10-10 讀碼）。改完之後 S3 的 bucket policy 可以強制
create-only，不必替任何前綴開例外（規格 §15）。

### 9. 派生字串與 AAD 改 `kist/v4/*`

ADR 019 待決 4 指出這一題從來與 Go 無關，留下的權衡是「字串與版號一致」對「升版後金鑰分屬不同
域」。分析：

- 不同 repo 的 master key 各自隨機，派生出的金鑰本來就不同，字串不影響跨 repo 的隔離。
- 同一個 repo 不會同時有 v3 與 v4 的物件：v4 不讀 v3、沒有遷移。
- 改字串的好處是第二道關：`open` 先驗 `config.v` 與 `min_reader` 再解開金鑰（`repo.rs` 的 open，
  2026-10-10 讀碼），就算某條路徑略過了版本檢查，v3 的 wrapped master 在 `"kist/v4/master"` 下也
  認證不過。
- 成本是重錄金鑰向量（`kist-crypto/tests/poc_keys.rs`），golden 本來就要因版號重錄。

所以改。gc 標記內容一併改 `KISTGC4\n`。

### 10. v4 不讀 v3

`v < 4` 一律拒絕，訊息指向最後一個 v3 版本；不寫遷移工具。實作開始前在 main 目前的 commit 打一個
v3 的 tag（建議 `v3-final`）。凍結的 v3 fixture 改當反向向量，另凍結一份 v4 的（規格 §18）。

### 11. 不在 v4 範圍

- ADR 017 決定 3 的其他債：restore 合併 range GET、`key` 命令、`read_snapshot` 的 `.r1` 退路對齊、
  ADR 014 表格的 rclone 列。都不改格式，排在 v4 前後都可以。
- ADR 025 沒修的 V22a、V22b、V23a、V23d。

順帶：v3 規格 §9 寫 `s3://bucket/` 切不出段、內容直接落在目標，程式實際切出 `bucket` 一段。ADR 019
裁定改文件以符合程式，但 `format.md` 沒有改到；v4 規格寫的是程式的行為。

## 假設

| 假設 | 來源 |
| --- | --- |
| v3 沒有需要讀回的真實資料 | 負責人陳述（2026-10-10） |
| 樹段打包能消掉 V21 的大部分成本 | 機制量過（fsync 佔 14.2 秒中的 11.1 秒）；v4 的效果 UNVERIFIED，要等實作 |
| 真實目錄平均每個節點約 128 bytes | 量測（上面第 4 點，兩個目錄） |
| 版本檢查在解開金鑰之前 | 讀碼（`repo.rs` 的 open：`config.validate` 與 `min_reader` 在 `unlock_key_slot` 之前） |
| pack 與 index blob 目前用普通 put | 讀碼（第 8 點列的行號） |
| 8 MiB 上限對寫入端記憶體沒有影響 | 猜測：一般目錄一段約 1.3 MB，受節點數限制；記憶體 bench 會驗 |
| S3 bucket policy 能以 `s3:if-none-match` 強制條件寫入 | ADR 017 查證（2024-11 起）；本 ADR 沒有重查 |
| ADR 017 列的其他外部事實 | ADR 017 查證；本 ADR 沒有重查 |

## 實作階段

每一段完成就 commit、回報、等確認，照 PLAN.md 的流程：

1. kist-format：版號 4、`kist/v4/*`、`Root.t`、`IndexPack.kind`、`TREE_SEGMENT_MAX`、樹段 payload
   的編解碼、v3 拒絕；golden 與金鑰向量重錄；`format-v4.md` 取代 `format.md`。
2. backup 寫入路徑：樹段寫進 mpack、沿用規則與驗證、零覆寫、mpack 的 `.r1`、`Root.t`；刪除 touch。
3. 讀取路徑：restore（新的映射）、check、mount（`corefs.rs`）、讀取端的重試。
4. prune 與 GC：持有者集合含 mpack、決定 2 的條款、repack 補 parity、刪除順序、小 mpack 的合併。
5. `rebuild-index` 掃兩個命名空間；壞 index blob 的處理與維護命令。
6. fixture：v3 改當反向向量、凍結 v4；gc_race 把樹納入持有者模型重跑 200 案，並補兩個缺口。
7. fuzz 重新播種（pack、cbor）、記憶體 bench（ADR 011、prune）、MinIO 與 SFTP 整合重跑、重量 V21。

工時沒有估過。

## 殘餘風險（ADR 017 列過的）

- 一顆 mpack 遺失的影響比一個樹物件大：一顆裝很多目錄段。`.r1`（本機）與 parity 是對策。
- 列目錄現在也依賴 index；退路是 `rebuild-index` 掃 mpack 的 trailer，要測。
- 每次有變更的 backup 至少多寫一顆小 mpack，要有合併政策（第 4 段定）。
- 所有速度數字都 UNVERIFIED，README 的 S3 數字是 v1 時代量的。

## 待負責人確認

1. 範圍：上面決定 1–11。
2. `TREE_SEGMENT_MAX` ＝ 8 MiB，或其他值。
3. 派生字串改 `kist/v4/*`（或留 `kist/v3/*`，省重錄金鑰向量）。
4. v3 tag 的名稱（建議 `v3-final`），打在 main 目前的 commit。
5. 小 mpack 的合併政策：第 4 段再定，或現在就定。
