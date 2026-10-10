# ADR 026：v4 格式——從需求重新推導

日期：2026-10-10　狀態：**設計已確認（2026-10-10，負責人：「全部按照你的推薦」），實作中。** 規格全文
在 `docs/format-v4.md`；kist-core、kist-mount、kist-cli 在第 4–6 段遷移之前仍是 v3，`docs/format.md` 繼續描述
它們。本 ADR 取代同編號的第一版草稿（commit `054c7e1`）。

## 一頁白話

**v4 要解決的三件事**

1. **刪錯東西。** v3 的 prune 要很多條件同時成立才安全：後端時間、各台機器的時鐘、同時只有一個
   prune、備份主機寫的 index 都要可靠。過去資料遺失方向的 bug 全都出在這些條件的接縫上。重新設計時
   還發現，在 v3 裡備份主機寫的 index 資料能影響 prune 刪哪些 pack。
2. **物件太多。** v3 每個目錄是一個獨立物件，沒變的目錄每次 backup 也要請求一次；`--files-from` 列
   2000 個檔案就是 2000 個物件（V21）。
3. **用猜的。** v3 從內容猜一個 root 是檔案還是目錄，會猜錯（V19）。

**v4 的做法**

- **只有一個 prune 能刪。** prune 要先成功建立下一個世代的 head（只在不存在時建立），才可以刪東西。
  兩個 prune 同時跑，只有一個建得起來，另一個什麼都不刪。
- **prune 只相信自己驗證過的東西。** 依據是 pack 自己的 trailer 與解密之後的內容；備份主機寫的 delta
  只是給別的 backup 用的提示。
- **時間只比 head。** 一個 pack 被標成 condemned 之後，要等「後端時間」與「寫 head 那台機器的時間」
  都過了一個 grace 才能刪。時鐘出錯只會晚刪，不會早刪。
- **什麼都不覆寫**，連 config 也不覆寫。S3 可以用 bucket policy 強制所有角色都只能新增。
- **目錄、chunk 清單、root 清單都放進 metadata pack**，大目錄依內容切段，改一個檔只重寫一段。
- **root 記下自己是檔案、目錄還是符號連結**，不再用猜的。
- **backup 只沿用信得過的副本**：自己寫的、prune 驗證過的、或自己下載驗證過的。被入侵的主機不能讓
  別台的 backup 回報成功卻還原不了。

**代價**

- 第一次沿用別台寫的內容時，要先下載驗證一次。
- 沒人用的資料最多要約兩個 grace（預設各 72 小時）再加上 prune 的間隔才會刪掉。
- backup 必須在 grace 減 1 小時內完成。
- prune 只能在支援條件寫入的後端上執行。rclone、B2、Wasabi 可以 backup，但不能 prune。
- KDF 的成本在 init 時就定死，之後不能調高。

**留下來的**（和替代方案比過之後才留）：keyed BLAKE3、XChaCha20-Poly1305、CBOR 規則、FastCDC、
pack 排版、snapshot 是唯一的 commit point、先標記再刪的兩階段刪除、Entry 的 metadata 聯集。

**狀態**：設計已確認，實作中。實作的第一段是可執行的 GC 模型、proptest 與外部審查，通過之後才寫正式
程式碼。v4 不讀 v3，也沒有遷移。

## 背景

- ADR 017（2026-09-26，草稿）主張 v4 只做一項格式改動：把目錄樹搬進 pack。ADR 025 留下兩項要改格式
  才能修的問題：V19、V21。
- 負責人 2026-10-10 先後說了三句話：「整個系統都還沒有真實上線過，相容性完全不重要，可以趁機改」；
  看過第一版草稿 `054c7e1`（v3 加上樹進 pack、`Root.t`、GC 補條款）之後說「需要重新設計 v4」；
  之後又說「不需要支援上一代，目前都還在內部開發階段」。
- **對「重新設計」的理解**：從需求與 bug 歷史推導每一層，不因為 v3 已經這樣做就保留。v3 的部分要在
  文件上贏過具名的替代方案才留下。這不等於「全部換掉」：贏的就留。負責人若指的是別的（例如只重做
  某一塊），以負責人的指示為準。
- **第一版的問題**（負責人沒有說明理由，以下是重新檢視後的判斷）：它照 ADR 017 的保守立場，把 v3 的
  GC 原樣留下再打補丁，而資料遺失方向的 bug 都出在這一塊。它也留著 v3 的 index blob 與 supersedes，
  也就是後來發現的那個缺口：備份角色寫的資料能進入 prune 的刪除決定。
- **這次的做法**：
  1. 整理證據：需求、L1–L13 的教訓與機制、審查發現、量測數字，並把 ADR 017 否決過的方案依理由分成
     兩類：安全理由（除非有新證據，維持否決）與成本、相容或喜好理由（前提已經不在，重新打開）。
  2. 查證 14 項外部事實（restic、rustic、Kopia、Duplicacy、bupstash、Borg 2、各家 S3 的條件寫入、
     `object_store`），都對照發行版的原始碼、官方文件或 changelog。
  3. 三位設計者各自從一個角度提出完整方案：A 看 GC 與並行安全，B 看物件儲存成本與還原效能，C 看
     抗勒索、金鑰角色與完整性。
  4. 綜合成一份設計。
  5. 兩輪對抗式審查：一輪找 GC 的競態，一輪找還原、加密與完整性的問題。
  6. advisor 再審一次。
  7. 寫規格時自己再查一遍。
- **v3 的信任缺口**：C 在 `e06b25a` 的副本上用探針重現過。細節不寫進 repo，這裡只記類別：v3 裡備份
  角色寫的 index 資料可以影響 prune 刪除哪些 pack。v4 以下面的 R2 從結構上排除這一類。

## 需求

| # | 需求 | 出處 |
| --- | --- | --- |
| R1 | 已 commit、未 forget 的 snapshot 引用的每個 ID，永遠有一個沒被 kist 刪掉的持有者 | PLAN 的目標順序（資料安全第一）、L1–L11 |
| R2 | 備份角色（只能新增、不能刪，但持有主金鑰）寫的資料不能左右維護角色刪什麼 | v3 的信任缺口 |
| R3 | 任何物件都不覆寫，所有角色都能以 bucket policy 強制只能新增；刪除只屬於維護角色 | PLAN 抗勒索、ADR 004 §3 |
| R4 | 讀取端遇到通不過驗證的副本就換下一個；被入侵的主機不能讓別台的 backup 沿用假副本 | 還原可靠排第二 |
| R5 | 沒變的 backup 請求數與目錄數無關；很多 root 是一個物件；大目錄改一個檔只重寫一段 | M2 量測（沒變的 backup 35 s）、V21 |
| R6 | 記下事實，不從內容猜 | V19 |
| R7 | 不讀 v3、沒有遷移 | 負責人 |

## 教訓與對策

| 教訓 | v3 的機制 | v4 |
| --- | --- | --- |
| L1 repack 一步拿掉舊 pack | walk 之後才 commit 的 backup 引用的 chunk 從 index 消失 | 不會發生：舊 pack 留在 map、保留全部項目，走 condemned → tombstone |
| L2 重疊的 prune 各自 supersede | 已刪的 pack 復活成正本，真正的持有者被刪 | 不會發生：一代只有一個寫入者（柵欄） |
| L3 刪 pack 前沒先改 index | 之後的 backup 沿用已刪的 pack | 不會發生：tombstone 先寫進 head g+1 才刪 |
| L4 commit 只逐 pack 檢查 | repack 拿掉了正在用的 chunk | 記錄永遠保留全部項目（只移除證明不實的），逐 pack 檢查就夠 |
| L5 樹的 HEAD→DELETE 視窗 | 刪掉剛被重寫的樹 | 不會發生：沒有樹物件；pack 名稱帶亂數 nonce，不會被同名重寫 |
| L6 backup 跑超過 grace | 樹被標記、刪除 | 以 B4 的單一規則 E < grace − M 擋住，E 計入休眠 |
| L7 touch 沒刷新 mtime | 沿用的樹被刪 | 不會發生：沒有 touch |
| L8 同秒比較方向 | 復活判斷差一秒 | 不會發生：比較都在小時級，而且只比 head |
| L9 SFTP 沒有 mtime | 被當成 epoch，判成過期 | 只有 head 的時間承擔安全性；沒有修改時間 → prune 拒絕（P1） |
| L10 帶舊標記的新樹 | 誤拒 commit | 不會發生：沒有樹標記 |
| L11 未排序的 index 重新解析 | 誤拒 commit | 不會發生：B4 不重新解析，查的是去重時記下的 pack |
| L12 index 合併寫死「既有位置未標記」 | 反覆重傳 | 排序寫成規範（live 先、`since` 小先、名稱小先），向量釘住 |
| L13 config 被竄改 | 快取認錯 repo，以為 chunk 已存在 | 保留並加強：槽的 AAD 綁整份 config |
| grace 是各主機自己的設定 | 兩台主機的設定互相看不到，要手動一致（README） | grace 放在 head，全 repo 一個值，只有維護角色能改 |
| v3 的信任缺口 | 備份角色的 index 資料進入刪除決定 | 不會發生：刪除只看 prune 讀過、驗證過的資料 |
| V19、V21 | 猜 root 的種類；每個 root 一個樹物件 | `Root.t`；root 清單是 metadata item |
| ADR 019 C3 | 持有者規則不在規格裡 | P5–P7 是規範 |
| ADR 017 的條件 | 樹段沒有位元組上限、規格寫 PutIfAbsent 程式卻用普通 put、repack 不寫 parity、`.r1` 的退路不一致 | 段 2 MiB、xattrs 256 KiB；全部只在不存在時建立；repack 寫 parity；讀取端一律「主體或 `.r1`」 |

## 每一層的選項與決定

### 1. 物件與命名空間

- (a) v3：`trees/`、`touch/`、`gc/`、`indexes/`；(b) 一種 pack 混放 chunk 與 metadata；(c) `packs/`、
  `mpacks/`、`deltas/`、`snapshots/` 都依 client 分前綴，GC 狀態放 `maint/`（head 與 map 分片）。
- **決定 (c)。** 拿掉所有靠 mtime 表達存活的物件（L5、L7–L10）與唯一的覆寫（`touch/`）。mpack 與資料
  pack 分開：走訪目錄不必讀資料 pack，也可以只替 metadata 寫 `.r1`。依 client 分前綴讓 S3 憑證能限定
  每台主機只寫自己的前綴，出事時也追得到是哪一台。

### 2. 識別碼與去重範圍

- (a) v3：chunk 與樹共用一把 key；(b) 每台主機各自的 hash key（bupstash 的 put key）；(c) chunk ID 與
  meta ID 用兩把 key，整個 repo 共用。
- **決定 (c)。** (b) 會失去跨主機去重（外部事實 9b）。(c) 讓一個 ID 是資料還是 metadata 由 key 決定，
  不靠它放在哪裡。

### 3. 金鑰與角色

- (a) v3：master 包在 config 裡，換密碼要覆寫 config；(b) 唯寫金鑰（bupstash 式）；(c) 以密碼推導名稱的
  槽加上不可變的 config；(d) (c) 再加每個 client 的 Ed25519 簽章。
- **決定 (c)，(b) 與 (d) 交給負責人。**
  - (a) 需要覆寫，違反 R3。
  - (c) 回答了 ADR 017 否決 `keys/` 的三條安全理由：開啟時只 GET 一個由密碼推導出名稱的槽，塞進再多槽
    也沒用；KDF 參數只來自不可變的 config，槽裡沒有參數可塞；刪掉最後一個槽需要 Delete，與刪 pack
    同一種權限，而 `key remove` 拒絕刪最後一個。
  - (b) 會讓 restore 需要一個沒有主機持有的秘密，也會失去去重與「沿用前先驗證」。
  - (d) 防的是被入侵的主機偽造別台的 snapshot（影響 `latest` 與 forget 的保留）。S3 上以前綴憑證就能
    擋住大部分。
- **字串**：`kist/v4/*`。沒有舊資料，ADR 017 留 v3 字串的理由（省掉重錄向量）已經不在，向量本來就要
  全部重錄。

### 4. 切塊

- (a) v3 的 FastCDC 與凍結的 gear 表；(b) keyed gear。
- **決定 (a)。** 沒有 bug 歷史，(b) 會破壞去重。小檔不內嵌進 metadata，mpack 只放 metadata。

### 5. Pack 排版

- (a) v3 排版；(b) 加明文 header 與每個 pack 的金鑰（只有唯寫金鑰需要）；(c) 兩種 magic 區分種類。
- **決定 (a)，trailer 加 `kind`。** fuzz 96 小時零 crash、沒有 bug 歷史。`kind` 放在受認證的 trailer
  裡，比只靠 magic 區分更難偽造。metadata 的解壓上限固定 2 MiB，與 trailer 宣稱的長度無關（第二輪
  審查 F5）。

### 6. Index

- (a) v3：index blob 加 supersedes，任何寫入者都能 supersede；(b) Kopia 式 epoch；(c) trailer 就是
  index，不另外存；(d) prune 從 trailer 建 map，以世代 head 串起來；backup 的 delta 只是提示。
- **決定 (d)。**
  - (a) 讓備份角色的資料進入刪除決定，違反 R2。
  - (b) 混用兩種時鐘，維護擁有者也沒有強制：Kopia 的「排他鎖」其實是本機 `flock`（外部事實 5、6b）。
  - (c) 冷啟動要每個 pack 一次 GET，也失去 chunk map 的第二份（ADR 017 的否決理由）。
  - (d) 保留了第二份（map 加 `.r1`），而且只有 prune 寫它。

### 7. 目錄與 metadata

- (a) v3：每個目錄一個樹物件，10 000 項一段、以 prev 串起來；(b) 整份 snapshot 一個 manifest；(c) 依內容
  切段加 Dir 索引，放進 mpack；(d) 固定大小切段。
- **決定 (c)。**
  - (a) 改一項要重寫後面整條鏈，每個目錄還要請求一次。
  - (b) 失去子樹沿用（ADR 017 的否決理由，仍然成立）。
  - (d) 插入一項會讓後面所有的切點移位。
  - 切點：段內至少 1024 項之後，名稱的 keyed hash 低 12 位元為 0 就切，平均每段約 5 100 項。以實測的
    每項約 128 bytes 計算，約 650 KB 一段。
  - `Root.t` 修 V19；root 清單本身也照目錄的方式切段，修 V21。Entry 的聯集沒有 bug 歷史，原樣留下。
  - 大檔案的 chunk 清單也照同樣的規則切段（以 chunk ID 代替名稱），多段時以 ChunkIndex 索引，記下每段
    涵蓋的 bytes 給 mount 跳著讀。只用一個 2 MiB 的 ChunkList 的話，檔案上限約 120 GiB（平均 2 MiB
    的 chunk，每個 ID 34 bytes），v3 沒有這個限制（清單本身當資料切 chunk）。

### 8. Snapshot

- (a) v3：唯一的 commit point 加 parent；(b) 再加簽章與前一份的 hash 鏈。
- **決定 (a)**，加上依 client 分前綴，roots 指向一個 metadata item。(b) 屬於負責人決定 1。
- 格式之外的 client 行為（不在規格裡）：記住自己上一個 snapshot 與看過的最大世代，消失或變小時警告。

### 9. GC 與並行（最緊的限制，最先決定）

| 方案 | 結果 |
| --- | --- |
| v3：標記、grace、活躍 client、BackupTooLong | 否決：要比對三種時鐘，不強制只有一個 prune，L1–L11 靠個別補丁修 |
| rustic 的 `packs_to_delete` | 否決：標記時間是寫入者的時鐘，寫在任何持鑰者都能寫的 index 裡，可以偽造（外部事實 3） |
| Kopia 的 epoch | 否決：混用時鐘，擁有者沒有跨主機強制（外部事實 5–7）。留下「寫入者只新增、一個擁有者整理、至少隔兩代」的想法 |
| Duplicacy 的 fossil | 否決：改名不是只在不存在時建立；閒置 ID 的豁免是官方文件記載的遺失途徑（外部事實 8） |
| prune 租約（restic、Borg 的鎖） | 否決：持有者停住超過期限就需要柵欄，crash 又會卡住 GC。柵欄本身不需要過期 |
| B 的「v3 加五個修補」 | 否決：仍假設一個 repo 只有一個 prune（部署規則），仍讀備份角色寫的 index |
| **世代柵欄、只比 head 的時間、單一 backup 時長規則、刪前驗證** | **採用** |

- **ADR 017 以安全理由否決過「用自己寫的時間戳、MAC、密封的計畫或 epoch 取代後端時間」**，v4 的 head
  看起來是同一類，差別如下：
  - 只有維護角色能寫 head（S3 上以 bucket policy 強制），備份角色寫不了。
  - head 的時間用的是寫入後的後端時間 `T`，並且要求 `w` 也過期：兩個都過期才刪。
  - ADR 017 找到的致命時間線各自有回答：「取樣時間與寫入時間相差數小時」→ `T` 是 head 寫入後的後端
    時間；「一個 now 兩種用途」→ `T` 與 `w` 分開比；「重疊的 prune 打破 epoch」→ 柵欄只讓一個成功。
- **租約與心跳**：不用。
- **一個 repo 一個 prune**：從部署規則變成協定保證。主機檔案鎖只是讓同一台不要白做工。

### 10. 冗餘

- (a) v3：parity 選配，樹寫 `.r1`；(b) 全部用 Reed-Solomon；(c) mpack、snapshot、head、map 分片寫
  `.r1`，parity 選配。
- **決定 (c)。** parity 修不了整顆遺失，而沒有 metadata 就什麼都還原不了。repack 也寫 parity（ADR 017
  的條件）。

### 11. 後端契約

- (a) v3：只在不存在時建立，加上覆寫（`touch/`、config、自我修復）；(b) 每次寫入都是只在不存在時建立的
  單一 PUT，不需要覆寫、分段上傳或條件刪除。
- **決定 (b)。** 不用分段上傳的原因：bucket policy 要替它開例外（外部事實 11c），它的 Last-Modified 是
  開始上傳的時間（外部事實 12），`object_store` 也不能對它做條件建立（外部事實 14b）。

### 12. 版本與向量

- config 不可變，版本靠各物件自己的 `v`。head 的 `v` 管住所有 client：較舊的 backup 與 prune 讀不懂
  最大世代就停止。snapshot 的密封方式在這一代內固定，較舊的 prune 才永遠能解開較新的 snapshot。
- 每條規範性規則都有資料檔向量或測試。v3 的 fixture 移除，凍結一份 v4 的。

## 審查結果

**第一輪（GC 競態），全部採用**

| # | 問題 | 修正 |
| --- | --- | --- |
| F1 | 讀到的世代不一定是最新的 | 以列出來的最大世代為準；讀不了或版本不認得，backup 停止，只有 restore 與 mount 可以退回較舊的世代 |
| F2 | 只從 delta 得知的 pack 可能已不存在，或不含 delta 宣稱的項目 | B4 對每個來自 delta 的 pack 做 HEAD，不在 map 裡就讀 trailer 確認；`E ≥ grace − M` 之後不寫 delta |
| F3、F4 | 舊的 head 刪掉之後 `T(c)` 查不到；分片的刪除範圍不明確；prune 的刪除沒有整輪時限 | head 永不刪除；分片只依明確的集合刪除；整輪 prune 有時限 |
| F5 | 只靠一個時鐘 | head 記下寫入者的時間 `w`，`T` 與 `w` 兩個都過期才刪；E 計入休眠 |
| F6 | 調低 grace 時的競態 | 調低時 drain 一個舊的 grace |
| F7 | 沿用 parent 的子樹，parent 卻被 forget | B4 對 parent 做 HEAD；forget 不刪每個 client 最新的那一份 |
| F8 | repack 寫出的 pack 沒有驗證 | 讀回驗證之後才記為 `by_prune` |
| F9 | 分片遺失後重建，會丟掉 condemned 與 tombstone 的狀態 | 重建也是一個走柵欄的世代，pack 以 condemned 進入，過一個 grace 之前不復活 |
| 小項 | — | 只有勝出的 prune 寫 head 的 `.r1`；只有 tombstone 持有者的 ID → 拒絕；E 在每次寫 snapshot 前量；`g_c < g_s` 是錯誤；parity 也列出、過期清理；forget 先刪 `.r1` |

**第二輪（還原、加密、完整性），全部採用**

| # | 問題 | 修正 |
| --- | --- | --- |
| F1 | 沿用別台的副本沒有驗證 | 「沿用前先驗證」寫成規範，不是選項 |
| F2 | 持有者的信任與換下一個 | 讀取端遇到驗證失敗的副本就換下一個持有者；prune 每次解密發現不實的項目就移除並回報；新進者不會取代既有的正本 |
| F3 | 通不過認證的物件沒有生命週期 | trailer 讀不了的 pack 記為 unreadable、過 grace 後回收；snapshot 兩份都解不開，記下來，一個 grace 之後不再當成起點 |
| F4 | Dir 與段沒有讀取端驗證 | `first` 嚴格遞增、段非空、第一項等於 `first`、段內排序且不重複 |
| F5 | metadata 解壓上限依 trailer 宣稱的長度 | 固定 2 MiB |
| 小項 | — | Dir 可以巢狀（最多三層）；head 的 AAD 是 key 路徑；delta 以內容命名，AAD 改綁 client id；分片號碼綁在明文裡；`ct=0` 最多 256 個 chunk |

**advisor**：

- P1 原本在 `|T(g) − w(g)| > 5 分鐘` 或 `T(g) < T(g−1)` 時拒絕，曾經寫錯時鐘的主機留下的 head 會讓 GC
  永遠停住。改成 P0 的探針：建立兩次必須得到「已存在」，再比較後端時間與本機時鐘；較舊 head 之間的
  異常只警告。
- init 寫 head 0。
- settings（grace、replicas、pack 大小）放在 head，改 settings 是一個不刪任何東西的世代。
- P9 刪掉已不再當成起點的壞 snapshot。
- 寫明沿用前先驗證的成本與沒採用的替代方案。

**寫規格時補的**：

- 第一次看到壞 snapshot 時，要以一個不刪任何東西的世代記進 `bad`。原本寫「拒絕並記錄」，可是拒絕就
  寫不了 head。
- 新進 pack 的 trailer 解得開但版本不認得 → 拒絕，不記為 unreadable：prune 不刪看不懂的東西。
- unreadable 的記錄在 pack 消失後移除，不讓它變成 condemned 之後觸發 P3 的誤拒。
- 只有 `.r1` 的 snapshot（可能是 commit 前中斷）照樣當成起點，不自動補主體。

**advisor 審規格時補的**：

- chunk 清單切段、加 ChunkIndex（見上面第 7 層）：原本單一個 2 MiB 的 ChunkList 把檔案上限壓到約
  120 GiB。
- repack 讀回驗證失敗：新 pack 記為 unreadable，舊的維持 live。
- 壞 snapshot 只在第一次看到時寫一個記錄用的世代；維護角色 `forget <key>` 可以立刻解除。
- `config.v` 代表這一代，同一代內各物件的 `v` 可以往上加。

**外部審查（實作第一段，2026-10-10）**：一個獨立的 agent 在 `8dc76bd` 的模型上對照規格逐條檢查並嘗試
破壞，寫了 19 個測試，搬進 repo 成為 `crates/kist-gcmodel/tests/review.rs`（m1 與 f4 兩處依修正改了斷言，
檔頭註明）。五項發現都修了，每一項都先在原碼上失敗、修正後通過，並有恢復舊規則的突變開關證明修正承擔
安全性：

| # | 問題 | 嚴重度 | 修正 | 紅→綠的測試；反向對照 |
| --- | --- | --- | --- | --- |
| F1 | drain 的結束只比 `T`：寫調低 grace 的 head 時後端時鐘慢 4 小時，drain 提早結束，B4 已過的 backup 用到的 pack 以新的 2 小時 age 被刪 | 資料遺失 | 兩個時鐘都要過（age 的選擇、drain 的清除、settings 的檢查三處） | `f1_drain_ends_on_backend_clock_alone_break`；`mutation_one_clock_drain_end_loses_data` |
| F2 | hold 的結束只比 `T(lost)`：遺失世代的刪除者以自己的時鐘計整輪時限，後端時鐘慢 2 小時時 hold 提早結束 | 資料遺失（配合偽造的 snapshot）；GC 停住 | Hold 加 `from`，兩個時鐘都要過 `from..lost` 的最大值 + grace | `f2_hold_ends_on_backend_clock_alone_data_loss_break`、`…_gc_stuck_break`；`mutation_one_clock_hold_end_loses_data` |
| F3 | 重建的沿用條件只比兩端 head 的 `T`，後端時鐘倒退 10 分鐘就不成立 | 資料遺失 | 遺失世代的 `T` 或 `w` 任一個的最大減最小不到一個 age | `f3_rebuild_keep_rule_assumes_monotone_t_break`；`mutation_one_clock_keep_rule_loses_data` |
| F4 | trailer 版本不認得的 pack 讓 P4 永遠拒絕；再遇上分片遺失，重建也失敗、所有 backup 停 | GC 永久停住、backup 全停 | 記為 unknown：不使用、不刪除、不擋 prune 與重建 | `f4_unknown_version_pack_stalls_gc_forever`、`f4_unknown_version_pack_and_a_shard_loss_stop_all_backups` |
| F5 | 基礎世代已被超越時，P3 誤報資料遺失 | 誤報 | P3、P5 拒絕之前重新列出 `maint/gen/` | `f5_superseded_prune_reports_false_data_loss`；`mutation_no_superseded_check_raises_a_false_alarm` |

- 模型本身與規格不符、一併修正的：強制 forget 的程序規則原本只在開始時檢查（m1，改成刪除完成前那組
  roots 都不能開始 backup）；物件修改時間的 ε 讓誠實時鐘下較晚的 head 拿到較早的 `T`（m2，改成請求不會
  早於寫入者讀到 head 的時間，時鐘故障照樣保留）；P7d 原本在同一輪重新驗證後就標 tombstone，改成照規格
  那一輪維持 condemned；重建案例 B 的 age 照 drain；解不開的 delta 過一個 grace 刪除；規格裡 pack 名稱
  的驗證改寫成 range read 做得到的方式。
- 照設計維持、沒有改的：偽造且引用沒有持有者的 item 的 snapshot，以及版本 5 的 snapshot，照樣讓 prune
  拒絕到有人 `forget`——它們可能代表真的資料遺失，所以大聲失敗（`f4_attacker_snapshots_stall_gc_until_forgotten`
  釘住這個行為，殘餘風險見下）。
- F1–F3 是同一類：「自某個世代起已過一個 grace」只看了一個時鐘。兩輪設計審查與 advisor 都沒發現，是
  模型上的外部審查找到的。規格 §11.1 現在把兩個時鐘的規則寫成一條總則。

**第二輪外部審查（2026-10-10，`0529f65`）**：負責人「按你建議的做」之後，另一個獨立的 agent 審查第一輪
之後的所有修改，並專攻「只看一個時鐘」這一類。它的 35 個測試（一個是 ignored 的統計）搬進 repo 成為
`crates/kist-gcmodel/tests/review2.rs`（G1 改用模型新加的 `.r1` PUT 失敗故障，檔頭註明；另加 3 個修正後的
反向對照）。它的 worktree
從 `main` 開出來，它自己 reset 到 `0529f65` 才開始；`0529f65..` 它那一支只多了 review2.rs。

| # | 問題 | 嚴重度 | 修正 | 紅→綠的測試；反向對照 |
| --- | --- | --- | --- | --- |
| D1 | 晚寫的 `.r1`（補寫，或寫入者在兩次 PUT 之間停住）加上主體遺失，`T` 改由 `.r1` 讀到；重建時 `T` 的最大減最小是 0（`w` 是 72 小時），沿用 `Map(g_r)`，遺失世代裡被標成 tombstone 的 pack 復活成 live；它的主人沿用，停在 P9 之前的刪除者接著把它刪掉 | 資料遺失（誠實的時鐘） | 沿用條件只看 `w`（審查者建議「每一代都還有主體才算 T」；`check --repair` 會重建主體、換掉修改時間，所以沒採用） | `d1_late_r1_then_body_loss_fools_the_keep_rule_break`、`d1_stalled_writer_late_r1_then_body_loss_break`；`mutation_keep_rule_t_clause_loses_data` |
| C1–C3 | 兩個時鐘在不同時間各錯一次：後端時鐘在寫 c（或調低 grace、或刪除者）時慢，之後某台主機在寫 base head 時快；`T` 那一半和 `w` 那一半各被一個故障騙過，P7d、drain 的結束、hold 的結束提早成立 | 資料遺失（兩個故障；規格原本的假設沒有排除） | 「現在」每個時鐘取 base head 的值與 P0 的讀數中較小的（P5 本來就只用 P0）；假設改成「一個 age 的時間窗內不會兩個時鐘都錯」 | `c1_p7d_…_break`、`c2_drain_end_…_break`、`c3_hold_end_…_break`、`c1_block_search_with_both_clock_kinds_break`；`mutation_now_from_base_head_only_loses_data` |
| G1 | head 的 `.r1` PUT 失敗，P9 照樣刪；之後只有一份的主體遺失，刪除的依據跟著消失 | GC 永久停住；配合偽造的 snapshot 資料遺失 | `.r1` 寫入失敗就不刪、停止（三個寫 head 的角色都一樣） | `g1_p9_after_a_failed_head_r1_put_break`、`g1_failed_head_r1_put_then_rewrite_loses_data_break`；`mutation_p9_ignores_head_r1_failure_stops_gc_or_loses_data` |
| M1 | 模型的 ε 讓 head 的 `.r1` 修改時間可能比主體早到 20 分鐘；規格的順序是主體先寫 | 模型保真度 | `.r1` 的修改時間以寫主體的時間為下限 | `m1_head_r1_mtime_can_precede_its_body` |

- 疑點 B（B4 讀到只有一份的 head）：審查者構造不出問題（`a7_b4_on_a_lone_head`），理由和這裡相同。
- 改了一個舊的反向對照：`mutation_only_writer_clock_deletes_early` 原本靠「prune e 的主機在 P0 之後快
  200 小時」，「現在」取小之後下一輪 P0 的牆鐘是對的，那條交錯騙不到只比 `w` 的規則了；改成寫 head c 的
  主機在 P1 之後慢 80 小時（`w(c)` 太小），原協定照樣擋住（`a_writer_clock_lag_alone_does_not_delete_early`）。
- 審查者補了一個之前沒有的情境：重建案例 B 的 age 也要照 drain（`a4_case_b_age_one_clock_windows_loses_data`）。
- 隨機策略：原本的策略碰不到時鐘時間窗這一類（時鐘偏差會一直留著，多半撞上某一輪的 P0 而被拒絕；每個
  case 只錯一種時鐘；backup 很少停在 B4 之後同時有幾輪 prune）。審查者的「區塊」策略（帶偏差的 prune／
  settings 只在那個角色執行期間出錯、讓 backup 停在 B4 之後、兩種時鐘都可能錯、±200 小時、小的資料集）
  在對應的時鐘模式下找得到 `only_writer_clock`、`only_backend_clock`、`one_clock_windows`、
  `no_elapsed_check`，也在原協定上找到 C 類。它留在 review2.rs、跑在 gate 裡：`s1`（四個突變都找得到）、
  `s2`（原協定在單一時鐘模式下找不到）、`c1_block_search_with_both_clock_kinds_break`（兩種時鐘都錯的
  模式下，修正後的原協定找不到）。

## 假設

| 假設 | 狀態 |
| --- | --- |
| 沒有要讀回的 v3 資料，不需要遷移 | 負責人陳述（2026-10-10） |
| S3 能以 bucket policy 強制 `If-None-Match` | 查證（外部事實 11c） |
| `maint/gen/` 的條件建立是原子的 | S3、R2、Azure、Ceph、MinIO：文件或原始碼（外部事實 11a、13）；GCS：generation-match 0；SFTP 的 hardlink、本機的 link：v3 已在用；B2、Wasabi：沒有文件。P0 每輪實測，但探針通過不代表每次都原子。rclone 橋接不是原子的（先 stat 再 rename），P0 依種類拒絕（實作第三段，第 15 項） |
| LIST 在寫入與刪除之後一致 | AWS 有文件；MinIO 只在 xfs/zfs/btrfs（v3 已記）；其他後端：猜測 |
| head 可見的延遲、E 的低估、snapshot PUT 的延遲合計小於 1 小時 | 猜測 |
| 備份角色寫不了 `maint/`、`keys/`、`config` | S3：bucket policy（部署）；本機與 SFTP：與 v3 同樣的信任 |
| `CLOCK_BOOTTIME` 計入休眠 | Linux 的 `clock_gettime(2)` man page；其他平台未查（UNVERIFIED） |
| 真實目錄平均每項約 128 bytes | 量測（2026-10-10，兩個目錄共 36 476 項） |
| mpack 8 MiB、資料 pack 64 MiB | 猜測，放在 head 的 settings，可以改 |
| grace 72 小時、M 1 小時、探針容差 5 分鐘 | 沿用 v3 的 grace 與邊際；容差同 Kopia（外部事實 7c） |
| 沿用前先驗證的成本可以接受 | 猜測：一群相似的機器，每台要下載一次別台寫的內容 |
| 100 萬個 ID 的 map 約 40 MB | 估計（設計者 A） |
| backup 記憶體不高於 v3、prune 150–250 MiB | 估計 |
| 單一 PUT 的大小上限放得下一個 pack | AWS S3 的單一 PUT 最多 5 GB（官方文件，2026-10-11 查）。預設的 pack_target 64 MiB 遠低於它；pack_target 接近 4 GiB、chunk 又很小（avg 接近 256 bytes）時，trailer 可能讓 pack 超過，PUT 失敗而 backup 報錯，不會無聲出錯。其他 S3 相容服務未查 |

## 外部事實（2026-10-10 查證）

| 對象 | 事實 | 用在哪裡 |
| --- | --- | --- |
| restic 0.19.1 | prune 拿排他鎖、backup 拿共享鎖；鎖 30 分鐘沒更新就過期，比對的是擁有者與檢查者兩台機器的時鐘 | 租約方案的代價 |
| rustic | 兩階段 prune，刪除標記與時間寫在一般 index 裡，時間是 prune 主機的時鐘，keep-delete 預設 23 小時 | 寫入者時鐘可以偽造 |
| Kopia 0.23.1 | metadata pack（`q`）與資料 pack（`p`）分開；epoch 由維護擁有者推進；「排他鎖」是本機 `flock`；時鐘偏差超過 5 分鐘就拒絕維護 | mpack、P0 的容差、擁有者需要柵欄 |
| Duplicacy 3.2.5 | fossil 清單存在執行 prune 的那台機器；7 天沒備份的 ID 被忽略，文件列為可能誤刪 | 否決 fossil |
| bupstash | put key 有自己的 hash key 與切塊 key，跨 key 不去重 | 唯寫金鑰的代價 |
| Borg 2.0.0b25 | b22 起改回 pack；鎖要在寫入者時鐘與儲存時鐘兩邊都過期才算 stale | 兩個時鐘都要過期 |
| AWS S3 | `If-None-Match: *`（2024-08）、bucket policy 強制（2024-11）、條件刪除（2025-09）；分段上傳的 Last-Modified 是開始時間 | create-only、不分段 |
| 其他 S3 相容後端 | MinIO（repo 已封存，讀取 quorum 錯誤時會略過檢查）、R2、Ceph、Azure 支援條件建立；GCS 用 generation-match 0；B2、Wasabi 沒有文件 | 後端契約 |
| `object_store` 0.14.2 | `PutMode::Create` 有；沒有條件刪除，分段上傳不能條件建立 | 不靠條件刪除、不分段 |

## 成本（估計，實作後重量）

- **沒變的 backup**：v4 估計約 6–11 個請求，與目錄數無關。v3 估計每個目錄約 4 個請求（設計者 B 讀碼）；
  M2 在 v2 時代量過 1 102 個目錄的沒變 backup：MinIO 35 秒，本機 0.8 秒。
- **2000 個 root**：v4 是一個 root 清單，寫進一個 mpack。v3 是 2000 個樹物件，量測 10.96 秒。
- **還原**：合併 range GET 屬於 client 端的改動，與格式無關，v4 前後都可以做。
- **prune**：每輪寫一個 head 與有變動的分片（256 片，改動少就寫得少），全部 map 約 40 MB／100 萬個 ID。
  刪除之前要驗證被引用的 ID；下載量與 condemned 的 pack 有多少有效項目成正比，`by_prune` 的正本不必
  再下載。
- **沿用前先驗證**：別台寫的內容，每台主機第一次沿用時下載一次，之後存在本地快取。沒有採用的替代方案
  是「由 prune 驗證每個新進 pack 的全部項目」：prune 的工作量會變成與新資料量成正比，而且在 prune
  收錄之前的沿用仍然沒有保護。「全部信任」不在 v4 裡；如果日後要加，只能是標明不安全的選項（Kopia 的
  `--safety=none` 也是這樣標示）。
- **刪除延遲**：沒人用的 pack 被標成 condemned 之後，要等到有一個 head 比它晚一個 grace，下一次 prune
  才把它變成 tombstone 並刪除：約一個 grace 加兩次 prune 的間隔。剛寫入不久的 pack 還要先比 grace 老
  才會被標，所以最多約兩個 grace。tombstone 的記錄再留一個 grace。

## 殘餘風險

- 安全性的證明是手寫的。實作第一段的可執行模型、proptest 與兩輪外部審查已經做了，找到並修正了十四個
  規格錯誤（見最後一節）。第一輪審查看的是 `8dc76bd`，第二輪看的是 `0529f65`；第二輪之後的修正（沿用
  條件只看 `w`、「現在」與 P0 的讀數取小、head 的 `.r1` 寫入失敗就不刪、模型的 ε 界限）只經過模型的情境
  與 proptest，每一項都有反向對照。剩下的風險：模型省略的部分（crate 說明列出）；模型證明的是規格，不是
  之後的程式碼，後面各段的實作要以模型的情境為驗收。ADR 016 的教訓是「GC 鄰接面的每個『顯然』都要過
  proptest 與外部審查」。
- **時間比較一再漏網**：drain 的結束、hold 的結束、重建的沿用條件都只看了後端時間，兩輪設計審查與
  advisor 都沒發現，是模型上的外部審查找到的；修正之後，第二輪外部審查又找到兩個：沿用條件的 `T` 會因
  晚寫的 `.r1` 變動，兩個時鐘在不同時間各錯一次也能騙過兩邊。§11.1 現在有一條總則（兩個時鐘、「現在」
  與 P0 的讀數取小、「不可能已過」只看 `w`），並列出剩下只看一個時鐘的比較與它們不承擔安全性的理由；以後
  改 GC 規則時，任何時間比較都要先對照這一條，並用 review2.rs 的區塊策略跑過。
- 柵欄只和後端的條件寫入一樣可靠。MinIO 在讀取 quorum 錯誤時會略過檢查：這是讀原始碼得到的，沒有
  實測。
- 被入侵的備份主機仍然可以：讀全部資料（它持有主金鑰）、塞滿空間、寫假的 pack（別台沿用前會驗證，
  還原遇到時會換下一份）、偽造自己前綴下的 snapshot。沒有前綴憑證的部署（本機、SFTP），它也能偽造
  別台的 snapshot（負責人決定 1）。
- backup 在 B4 之後、snapshot PUT 完成之前停住：B5 量兩次 E、E 計入休眠，可以把視窗縮到 PUT 本身的
  延遲，但那個延遲仍然是假設。
- **以壞 snapshot 讓 GC 停住**：任何能在 `snapshots/` 寫入的角色（不需要金鑰）都能放一個解不開的
  snapshot，讓所有 prune 停一個 grace；每個 grace 放一個就能讓 GC 一直停。持有金鑰的被入侵主機還能放
  解得開、但引用沒有持有者的 item 的 snapshot，或版本 5 的 snapshot，這兩種會讓 prune 一直拒絕，直到有人
  處理——它們可能代表真的資料遺失，所以刻意大聲失敗，不自己過期。對策：prune 每次都回報是哪一個
  snapshot，維護角色 `forget <key>` 立刻解除；S3 上依 client 分前綴的憑證讓它追得到是哪一台。資料不會
  因此遺失，只是空間回收延後。
- **版本不認得的 pack 永遠保留**：prune 不刪看不懂的東西，所以這種 pack 只佔空間，要由維護命令移除。
- **head 的主體與 `.r1` 一起遺失**：柵欄靠 `.r1` 擋住主體遺失後被重新建立；兩份都遺失時，較早讀過 head
  的寫入者可以重新建立那一代。主體寫成、`.r1` 還沒寫就當掉、之後主體才遺失的路徑已加進模型的故障，
  找到的兩個錯誤（第 10、11 項）已修正；模型不讓主體在活著的程序兩次 PUT 之間的幾毫秒內遺失。
- **一個 age 的時間窗內兩個時鐘都錯**：兩個時鐘的規則只保證單一時鐘出錯時延後刪除。原本的假設寫「不會
  同時往同一個方向錯」，沒有排除兩個時鐘在不同時間各錯一次（第二輪審查的 C1–C3，已修正）；「現在」與 P0
  的讀數取小之後，剩下的組合要一個時鐘在某一代寫成時錯、另一個時鐘從 base head 寫成一直錯到那一輪的
  P0。
- 一顆 mpack 裝很多目錄的段，遺失的影響比 v3 的一個樹物件大。對策是 `.r1`（本機）與 parity。
- ID 超過 1 000 萬時 map 的大小要再評估。
- 所有成本數字都是估計（UNVERIFIED）。

## 實作階段

每一段完成就 commit、回報、等確認，照 PLAN.md 的流程。工時沒有估過。

1. **GC 可執行模型。** 把 B1–B5、P0–P9、重建、settings、grace 的 drain 寫成純邏輯模型，搭配記憶體後端、
   可控制的時鐘、可以注入的延遲與故障。以 proptest 跑各種交錯：重疊的 prune、停住的 prune、輸掉柵欄、
   重建、grace 調低再調高、晚到的 delta、parent 被 forget、不實的 trailer、讀不了的物件、時鐘出錯。
   斷言 R1 的不變式，之後送外部審查。這一段通過之前不寫正式程式碼；模型找到的問題先回頭改規格。
2. **kist-format。** v4 的結構與欄位表、pack 的 `kind`、切段與 Dir 驗證、`Root.t`、head、分片、記錄、
   delta、金鑰槽；向量資料檔；`v` 不是 4 就拒絕；`format-v4.md` 取代 `format.md`。
3. **後端。** 所有寫入都改成只在不存在時建立的單一 PUT；P0 的探針；依 client 分的前綴；以 start-after
   列出；修改時間。
4. **backup。** mpack 的寫入與切段、B1–B5、delta、沿用前先驗證的快取、parent 子樹的沿用。
5. **讀取路徑。** restore（新的映射、依序嘗試持有者）、mount、check、`check --repair`。
6. **prune 與維護。** P0–P9、forget、重建、settings 命令、grace 的 drain、`key add`／`key remove`。
7. **收尾。** 凍結 v4 fixture、fuzz 重新播種、記憶體 bench、MinIO 與 SFTP 整合重跑、重量 V21 與 M2 的
   請求數。

## 負責人決定（2026-10-10）

負責人：「全部按照你的推薦」。上面對「重新設計」的理解沒有被指正，照這份設計實作。

1. **不加每個 client 的簽章。** S3 上以「每台主機只能寫自己前綴」的憑證防止被入侵的主機偽造別台的
   snapshot；本機與 SFTP 接受這個風險。
2. **寬鬆後端上的 prune**（rclone、B2、Wasabi）：拒絕。
3. **S3 上 mpack 預設不寫 `.r1`**（replicas 0），parity 選配。
4. **grace 預設 72 小時。** backup 必須在 71 小時內完成，沒人用的資料最多約兩個 grace 加 prune 間隔才刪。
5. **config 不可變。** KDF 的成本在 init 時決定，之後不能調高。
6. **唯寫金鑰不在 v4**，因為代價是失去跨主機去重。
7. **實作第一段先做可執行的 GC 模型、proptest 與外部審查**，通過之後才寫正式程式碼。
8. **v3 的信任缺口不在 main 上另外修**：v4 取代 v3，系統也還沒上線。

實作第一段的回報之後，負責人：「按你建議的做」（同一天）。照設計維持的兩項記為確認：

9. **攻擊者寫的、解得開的 snapshot 讓 prune 拒絕到有人 `forget`**（引用沒有持有者的 item，或版本 5）：
   它們可能代表真的資料遺失，所以大聲失敗，不自己過期。
10. **版本不認得的 pack 保留到維護命令移除**：prune 不刪看不懂的東西。移除命令在實作維護命令的那一段
    定義（PLAN M12）。

## 實作第一段：GC 可執行模型（2026-10-10）

位置：`crates/kist-gcmodel`。不是正式程式碼：只依賴 std，proptest 是 dev-dependency。

**做法**

- 每個 actor（backup、prune、forget、restore、改 settings、重建）是一個 phase 狀態機，一步一個後端操作。
  排程器依 proptest 產生的 action 清單推進時間、啟動 actor、推進一步、把某個 actor 一口氣跑完、讓它當掉、
  注入故障或惡意寫入；proptest 縮小的是這份清單，失敗時得到最短的交錯。
- 真相與協定狀態分開：item 的 DAG、每個 item 的真副本、誠實 client commit 過的 snapshot 都在模型的世界
  裡，actor 只能經由後端操作看到 repo。
- 每一步之後檢查：R1（commit 過、沒有 forget 的 snapshot 引用的每個 item 都有真副本）；照讀取端的規則
  找得到每個 item 的真副本；最新 head 讀得全（分片故障期間除外）；restore 沒有在 snapshot 還在時讀失敗；
  沒有 P3 的「資料遺失」拒絕（模型沒有遺失 pack 的故障，出現就是誤報）。
- 每個隨機 case 結束時做收尾活性檢查：停掉所有 actor、時鐘歸零、最新 head 讀不了就重建、forget 被入侵
  主機的 snapshot，先等過出現過的最大時鐘偏差，再跑六輪 prune，每輪推進 grace、drain 的舊 grace、hold
  的 grace 與 condemned 記錄裡最長的 age 之中最大的那個再加一小時。每一輪都要完成；最後誠實前綴下不能
  留下沒被任何 snapshot 引用的 pack，每個誠實的 snapshot 都要讀得回來。外部審查指出原本沒有活性檢查。
- 時間以秒計。物件的修改時間 = 後端時鐘 − ε（≤ 20 分鐘，但不早於寫入者讀到它所依賴的 head 的時間）；
  backup 的經過時間少算 u（0 到 30 分鐘均勻）；兩者合計小於 M，就是規格的時間假設。
- 時鐘故障：prune 主機的牆鐘與後端時鐘各自可以出錯、可以在 P0 之後才跳，偶爾差到 100 至 200 小時。
  其他故障：actor 當掉、map 分片遺失、head 主體遺失、repack 讀回失敗。
- 被入侵的備份主機只能在自己的前綴下建立物件（負責人決定 1 的 S3 部署）：假副本的 pack（可以挑比別人
  小的名稱、可以混一部分真副本）、替別台的 pack 宣稱內容的 delta、解不開的 snapshot、版本 5 的 snapshot、
  trailer 解不開或版本不認得的 pack、偽造的 snapshot。
- 省略的寫在 crate 說明裡：parity、Object Lock、Dir 與 ChunkIndex 的巢狀、+1 ns 重試、加密與壓縮（以
  真／假副本、讀得了／讀不了代替）、大小上限、本地 index 快取、分片的 `.r1`；分片 4 個而不是 256 個。

**找到並修正的規格錯誤**：每一項都有先紅後綠的測試，並有恢復舊規則的突變開關證明修正承擔安全性或
活性。

| # | 誰找到的 | 問題 | 嚴重度 | 修正 |
| --- | --- | --- | --- | --- |
| 1 | 模型的確定情境 | P5 的壞 snapshot 等「滿一個 grace」比的是 `T(g)`，等待期間每一輪都停住、不寫 head，`T(g)` 不前進 | GC 永久停住 | 比 P0 的探針時間與本機牆鐘 |
| 2–6 | 外部審查 F1–F5 | 見上面「審查結果」的外部審查一節 | 資料遺失（F1–F3）、GC 停住（F2、F4）、誤報（F5） | 同上 |
| 7 | 收尾活性檢查 | P1 遇到時間在未來的 head 就拒絕：後端時鐘跳快一次，GC 就停到真實時間追上它，連改 settings 都過不了，沒有任何維護命令能解 | GC 停住，長度等於那次跳動 | 以本機時鐘 + M 代替，只會更保守 |
| 8 | 處理審查時的隨機測試 | hold 的時間窗一過就清掉 held；重建之前已被刪掉的 pack，在 hold 很短（grace 被調低）時第一次 prune 就誤報資料遺失 | 誤報、GC 停住 | held 留到記錄被復活；held 的記錄消失就移除 |
| 9 | 深度隨機測試 | head 主體遺失、`.r1` 還在時，一個在那一代出現之前就讀了 head 的寫入者可以重新建立那一代，柵欄被繞過、map 分岔 | restore 找不到資料 | 柵欄先看那一代的 `.r1`；head 一律寫 `.r1` |
| 10 | 新故障（寫入者當掉後主體遺失）的隨機測試 | 寫入者在 head 主體與 `.r1` 之間當掉，維護角色在只有一份的 head 上建下一代；之後主體遺失，世代之間有空洞，重建要讀範圍內每一代的 head，永遠拒絕 | GC 與所有 backup 永久停住 | 維護角色讀最新 head 時補寫缺少的 `.r1`（P1、改 settings、重建） |
| 11 | 從模型推出、以情境重現 | backup 以只有一份的 head 決定沿用；那一份遺失後，同一代被另一個 prune 以沒看過偽造 snapshot 的計畫重新寫出，刪掉那些 pack | 資料遺失（要寫入者當掉、主體遺失、被入侵的主機與另一個 prune 同時發生） | B1 只用兩份都在的 head，否則稍候重列、再停止 |
| 12 | 第二輪外部審查 D1 | 晚寫的 `.r1` 加主體遺失讓 `T` 變晚，重建的沿用條件誤判、刪掉的 pack 復活 | 資料遺失（誠實的時鐘） | 沿用條件只看 `w` |
| 13 | 第二輪外部審查 C1–C3 | 兩個時鐘在不同時間各錯一次，P7d、drain、hold 的兩邊各被騙過 | 資料遺失（兩個故障） | 「現在」與 P0 的讀數取小；假設改成時間窗的寫法 |
| 14 | 第二輪外部審查 G1 | head 的 `.r1` 寫入失敗，P9 照樣刪 | GC 永久停住；配合偽造的 snapshot 資料遺失 | `.r1` 寫入失敗就不刪、停止 |

**每條規則都承擔安全性**：每一條各有一組情境，照規則通過、把那條規則拿掉（`Mutations`）就違反。

| 規則 | 拿掉之後 | 證據 |
| --- | --- | --- |
| B3–B5 的經過時間檢查 | B4 之後停住超過一個 grace 的 backup，commit 了已被刪掉的 pack | 情境；隨機搜尋也找到 |
| P7d 刪前驗證 | repack 之後，假的正本排在前面，真的副本全被刪掉 | 情境；隨機搜尋也找到 |
| P7d 兩個時鐘都要過期 | 只比 `w`：寫 head c 的主機牆鐘在 P1 之後慢了就提早刪；只比 `T`：後端時鐘在 P0 之後慢了就提早刪 | 兩個情境；區塊策略的隨機搜尋也找到 |
| 「現在」與 P0 的讀數取小（第 13 項） | 恢復只用 base head 的值：兩個時鐘在不同時間各錯一次，P7d、drain、hold 提早成立 | 三個情境；區塊策略的隨機搜尋也找到 |
| 重建沿用條件只看 `w`（第 12 項） | 恢復 `T` 那一半：晚寫的 `.r1` 加主體遺失，誠實的時鐘下資料遺失 | 兩個情境 |
| head 的 `.r1` 寫入失敗就不刪（第 14 項） | P9 照刪：主體遺失後 GC 永久停住；配合偽造的 snapshot 資料遺失 | 兩個情境；隨機搜尋也找到 |
| 重建案例 B 的 age 照 drain | 恢復只看 `T`：資料遺失 | 情境（第二輪審查補的） |
| drain、hold、重建沿用條件也看兩個時鐘（第 2–4 項） | 恢復只看 `T`：外部審查的三個情境重新資料遺失 | 三個情境 |
| P8 的柵欄 | 輸掉柵欄的 prune 刪掉 delta，新 pack 既不在 map、也不在任何 delta 裡 | 情境；隨機搜尋也找到 |
| 柵欄先看 `.r1`（第 9 項） | 主體遺失後那一代被重新建立，map 分岔 | 情境；隨機搜尋也找到 |
| B2 只沿用 live 的持有者 | 沿用了已過期的 condemned，下一輪 prune 把它刪掉 | 情境；隨機搜尋也找到 |
| B2 沿用前驗證 | 沿用了假副本，commit 一個沒有人有的 chunk | 情境；隨機搜尋也找到 |
| B4 HEAD parent | parent 在 B4 之前被強制 forget，子樹裡的 chunk 被刪掉 | 情境 |
| grace 調低時的 drain | 在調低之前通過 B4 的 backup，用到的 pack 提早被刪 | 情境 |
| delta 只描述自己前綴的 pack、B4 確認記錄裡還有沿用的 ID | 各自單獨拿掉都還擋得住；兩個都拿掉才違反（兩層防護） | 四個情境 |
| P3、P5 拒絕前重新列出 head（第 6 項） | 誤報資料遺失 | 情境；隨機搜尋也找到 |
| P1 以本機時鐘代替未來的 `T`（第 7 項） | 時鐘修好之後 prune 仍拒絕 | 情境 |
| 重建的 hold | 慢速刪除完成時下一輪 prune 誤報資料遺失、GC 停住 | 情境；隨機搜尋也找到 |
| 維護角色補寫最新 head 缺少的 `.r1`（第 10 項） | 世代之間留下空洞，重建永遠拒絕 | 情境；隨機搜尋也找到 |
| B1 只用兩份都在的 head（第 11 項） | backup 照之後被改寫的那一代沿用，commit 了已被刪掉的 pack | 情境；隨機搜尋也找到（沿用的持有者找不到） |

另外兩項不是協定裡的規則，模型確認它們承擔安全性：**強制 forget 每組 roots 最新的那一份＝讓那組 roots
退役**（§9 的程序規則；parent 在 B4 之後才被強制 forget 時協定本身擋不住）；**時間假設**（backup 的經過
時間少算 2 小時、大於 M 時 R1 違反，少算 30 分鐘時 B5 擋下）。

模型裡沒有找到作用的兩條規則都保留：排序上「新進者不取代既有的正本」（假的正本已由刪前驗證擋住，它讓
正本不會隨著新副本來回換）；prune 的整輪時限（在模型裡它的作用與柵欄重疊，重建 hold 的長度是由它推出
來的）。

**head 的 `.r1` 當掉路徑**（負責人：「按你建議的做」）：模型加了一個故障——寫入者寫了 head 主體、還沒寫
`.r1` 就當掉，之後主體遺失。它只挑沒有活著的程序正要寫那個 `.r1` 的 head：活著的程序兩次 PUT 之間只有
幾毫秒，當掉之後才一直只有一份。改 settings 與重建原本一步寫完主體與 `.r1`，和規格的兩步不符，一併改成
兩步；重建的結果遺失時，分片遺失的記帳重新算。找到第 10、11 項。B4 沒有加同樣的檢查：在兩份都在的 `g_s`
裡 live 的 pack，被改寫的那一代仍然只能照時間規則處理它，模型裡沒有找到需要的交錯（第二輪審查也構造不出
來）。另外想到一個連帶問題：補寫的 `.r1` 可能很晚才寫，主體之後遺失時 `T(g)` 改由 `.r1` 讀到而變晚，重建
沿用條件裡 T 那一半的最大減最小可能被低估。我以為分片清理會讓 `g_r` 先變成讀不全而擋住，構造不出失敗的
交錯，就交給第二輪審查；審查者用一個停在 P9 之前的刪除者構造出來（刪除與分片清理都還沒發生時就重建），
成為第 12 項。

**隨機交錯**

- gate 裡三種設定各 512 個 case，每個都有收尾活性檢查：prune 主機的時鐘出錯；後端時鐘出錯；grace 6 小時
  且 replicas 0。每一批都要求 condemned、復活、tombstone、刪除、repack、抓到假項目、輸掉柵欄、重建、
  沒有 `.r1` 的 head 主體遺失、補寫 `.r1`、head `.r1` 寫入失敗、restore 完成都出現過，避免隨機測試本身
  空轉。另有第二輪審查的區塊策略（見「審查結果」的第二輪一節）。
- 深度版（不在 gate 裡）三種設定各 5000 個 case，沒有違反。三批合計 commit 68 767、tombstone 163 367、
  刪除 161 913、復活 10 708、抓到假項目 5 768、輸掉柵欄 4 242、重建 4 648、沒有 `.r1` 的 head 主體遺失
  3 747、補寫 `.r1` 6 328、head `.r1` 寫入失敗 10 159、restore 完成 84 734。
- 固定種子的突變搜尋（不在 gate 裡；每個突變、兩種時鐘出錯方式各 2000 個 case，預設設定與 grace 6 小時
  兩批，release 約 220 秒）：刪前驗證、柵欄、沿用前驗證、重建的 hold、拒絕前重新列出 head、補寫 `.r1`、
  B1 只用兩份都在的 head（找不到：backup 照之後消失的那一代沿用，不需要偽造的 snapshot）、`.r1` 寫入失敗
  就不刪（找不到：刪掉的 delta 只記在消失的那一代）在四組裡都找到反例；柵欄先看 `.r1`、只沿用 live 在三組
  找到；經過時間檢查在兩組找到。其餘由上表的情境證明：兩個時鐘各自單獨、看兩個時鐘的三個時間窗、「現在」
  與 P0 的讀數取小、沿用條件只看 `w`、B4 HEAD parent、drain、delta 前綴與 B4 記錄檢查（兩個都拿掉也沒
  找到）、P1 以本機時鐘代替未來的 `T`；其中時鐘那幾條，第二輪審查的區塊策略找得到（`s1`；「現在」取小
  是它在修正之前的原協定上找到的 C 類）。tests/found.rs 固定五條搜尋找到的交錯，都含收尾檢查（刪前驗證、
  柵欄、只沿用 live、沿用前驗證、補寫 `.r1`）：突變違反、原協定不違反。新進者排序與整輪時限仍然沒有找到
  反例（見上一段）。
- 原本的隨機策略碰不到時鐘時間窗這一類：兩個時鐘各自單獨、看兩個時鐘的三個時間窗、經過時間檢查，拿掉
  之後四組都沒有反例，F1–F3 也不是隨機測試找到的。經過時間檢查本身常常觸發（用 gate 的產生器與種子統計，
  三種設定各 512 個輸入：B4 中止 678／735／1604 次、B5 中止 116／123／122 次），但要出事還得在那段時間
  裡剛好有 prune 標記並刪掉它用到的 pack，這種交錯太稀有。第二輪審查的區塊策略找得到這一類（`s1`），
  它在 gate 裡。

## 實作第二段：kist-format（2026-10-10 至 10-11）

負責人：「按你建議」——不做第三輪外部審查，開始第 2 段。

**內容**：`kist_format::v4`（ids、keys、config、pack、meta、snapshot、index）與 `kist_crypto::v4`（KEK、
槽、八把子金鑰、§5 七種密封），六個 commit（`6a152cd` 到 `fc0ad50`）、一個修正（`1ad49f0`）與這份文件的 commit。v3 的模組
一行都沒動，兩個 crate 的 lib.rs 只多了 `pub mod v4`。

**與「實作階段」第 2 項的偏離**：那一項寫「`format-v4.md` 取代 `format.md`」。實際上 kist-core、kist-mount、
kist-cli 在第 3–6 段遷移之前都還是 v3 的程式碼，刪掉 `format.md` 會讓它們沒有規格。所以 `format-v4.md`
成為 v4 的權威副本，`format.md` 的開頭改成「只描述還沒遷移的 v3 程式碼」，v3 的模組移除時一併刪除。

**向量的做法**：兩種，看預期值能不能由另一份程式獨立算出。
- 獨立算出：金鑰推導、槽、config 與槽的 CBOR、子金鑰、七種密封、切段條件、目錄與 chunk 清單的切段結果
  與 ChunkIndex 的 `size`，由 `scripts/v4-vectors` 的 Go 程式（lukechampine.com/blake3、x/crypto 的
  argon2 與 chacha20poly1305、手寫的最短 CBOR 編碼）算出，Rust 逐 byte 相同。
- 由程式錄下再核對：trailer、pack 排版、Entry、snapshot、RootSeg、head、分片、記錄、delta。每種都有一份
  手寫預期 hex 的單元測試（欄位順序與省略照 §4 的表一欄一欄寫），golden 檔再以另寫的 CBOR 解碼器逐欄看過。
- 每條規則至少一個失敗案例；關鍵的幾處做了反向檢查（改一個派生字串、欄位順序、排序鍵、檢查條件，測試失敗，
  改回通過）。

**寫進規格的釐清**（實作時發現原文沒寫死或寫錯，照下面寫進 format-v4.md）：
1. 槽與探針的名稱是 16 bytes＝32 位 hex（原文 `keys/<hex16>`）。
2. parity 與它的 pack 同名；§2 原本把 parity 列進「名稱＝BLAKE3(bytes)」，與 §1、P9、§13 矛盾。
3. 切段條件的低 12 位元＝keyed hash 前兩個 byte 以 little-endian 讀成的整數；「至少 1024 項」是加入那一項
   之前的數目；大小照實算。
4. trailer 的 `r` 不含 alg byte；`l` 在 41 到上限＋41。壓縮的 1/16 用乘法比較（整數除法讓短原文留下沒省到
   的壓縮版，測試發現的）。
5. Dir 的每一項都小於下一段的 `first`（查找靠它）；ChunkList 非空；ChunkIndex 每段 `size` > 0；xattrs 合計
   含名稱與值；上限在解碼與建構兩處執行。
6. snapshot：`parent` 沒有就省略；內容的 client 與時間要等於 key。
7. head：`parent` 是 head g−1 密封後的 bytes 的 hash；明文的 `g` 要等於 key；settings 的範圍（v3 的
   `chunker.max ≤ pack_target` 不保留，理由寫在 §10）；drain、hold、bad 的定義性約束。
8. 記錄：分片內依名稱嚴格遞增；`status` 的欄位依 `s` 在場；`by_prune` 只在全零 client 的前綴下；排序裡
   tombstone 最後、unreadable 與 unknown 不是持有者。
9. delta：`DeltaPack` 沒有 client，key 由前綴決定；原文「略過不符的項目」不存在這種項目，改成明文 client
   與前綴不同的 delta 視同解不開。
10. 格式層刻意不檢查、使用時本來就會出錯的值，列在 §10。

**修正**：第二步把快取目錄名稱實作成 hex(`K_cache`[..16])，漏了 §3 的「換 repo 位置就不共用（同 v3）」，
commit 訊息還把它當成規格沒寫死。改成 keyed BLAKE3(`K_cache`, 位置) 的前 16 bytes，Go 向量一起改（`1ad49f0`）。

**延到後面的**：zstd 本身與解壓上限（讀寫路徑，第 4、5 段）；目錄巢狀 256 層（backup 與 restore）；快取
的使用（第 4 段）；parity 的 v4 排版（第一個寫 parity 的段，預定沿用 v3 的排版、`v` 改成 4）；改 settings
的命令要做哪些跨 config 的檢查（第 6 段）。

**gate**：fmt、clippy、windows-gnu clippy 乾淨；nextest 從 569 增加到 635 個通過，9 個略過。

## 實作第三段：後端（2026-10-11）

負責人：「繼續」（第二段的回報之後）。

**內容**（`crates/kist-backend`；一個程式的 commit `91aa8cc` 與這份文件的 commit）：
- 寫入：v4 的每一次寫入都是既有的 `put_if_absent`。S3 是帶 `If-None-Match: *` 的單一 PUT，本機（`object_store`
  的 local）與 SFTP 先寫暫存檔再 hard link（讀過 `object_store` 0.14.1 與 kist 的 SFTP store 的原始碼）。說明裡
  寫明結果的不確定性與呼叫端的處理：S3 回 5xx 時 client 會重試，第一次已寫入的話重試得到「已存在」；逾時的
  建立不重試，錯誤不代表沒寫入（讀 `object_store` 的 `retry.rs` 與 `aws/mod.rs`）。這條也寫進 §12。
- `Backend::probe` 是 P0；`Backend::create_only` 在 rclone 橋接是 false，P0 在寫任何東西之前就拒絕。容差
  `PROBE_TOLERANCE`（5 分鐘）放在 `kist_format::v4::index`。
- `Backend::list_after`：以 start-after 列出。S3 送 `start-after`（`PrefixStore` 會替起點加上 prefix），本機與
  SFTP 列出整個 prefix 再過濾。不含起點本身、含它的 `.r1`，所以讀取端可以從上次看過的世代起列。
- `Backend::delete`：刪除不存在的物件在各後端一致地算成功。先紅：本機的 `object_store` 回 NotFound，S3、SFTP、
  rclone 回成功。
- 測試：`tests/contract_v4.rs` 的六條（只在不存在時建立、修改時間、依 client 分的前綴、start-after、刪除、探針）
  在本機、MinIO、SFTP、rclone 上都跑過；`tests/probe.rs` 以包裝 `InMemory` 的 store 模擬忽略條件建立的服務、
  時鐘差 ±10 分鐘與 ±4 分鐘的後端、rclone 的位置。反向檢查：每條規則拿掉之後只有對應的測試失敗。

**找到的規格錯誤**（接續第一段的編號）：

| # | 誰找到的 | 問題 | 嚴重度 | 修正 |
| --- | --- | --- | --- | --- |
| 15 | 實作探針時讀 SFTP store 的寫入路徑，在真的 rclone v1.75.2 上重現 | §12 寫寬鬆後端「P0 不通過就拒絕」，但 rclone 橋接的條件建立是先 stat 再 rename：依序建立兩次照樣得到「已存在」，探針通過（「a relaxed backend passed the probe」）；兩個寫入者同時通過 stat 時，後寫的蓋掉先寫的（ADR 014 記過這個競態）。GC 模型的寬鬆後端是「第二次建立也成功」，和 rclone 不同，所以模型裡 P0 擋得住 | 柵欄失效：兩個 prune 可以都以為自己建立了同一代，§11.5 的前提（一代只有一個寫入者）不成立 | 後端依種類回報 `create_only`，P0 對 rclone 直接拒絕；雙重建立留著抓忽略 `If-None-Match` 的 S3 相容服務 |

**與「實作階段」第 3 項的偏離**：
- 「所有寫入都改成只在不存在時建立的單一 PUT」：v4 不呼叫覆寫，但覆寫的 `put` 還在，給還沒遷移的 v3（`config`
  與 GC 的 `touch/`）用；說明改成 v3 專用，v3 移除時一起移除（同第二段對 `format.md` 的處理）。
- 「依 client 分的前綴」：key 在第二段（`kist_format::v4::keys`）就定好了，後端不需要新程式碼；契約測試確認
  `packs/` 列出所有 client、`packs/<c>` 只列那一台。以 S3 憑證限定每台主機只能寫自己的前綴（負責人決定 1 的
  部署方式）沒有在 MinIO 上實測。
- 「修改時間」：沒有新程式碼。`list` 與 `head` 一致（截到秒）、剛建立的物件接近本機時鐘，寫進契約測試；SFTP
  伺服器沒送修改時間就讓整個操作失敗，是 v3 時已有的處理。

**順帶改變的 v3 行為**：本機刪除不存在的物件原本回 NotFound，v3 的 prune 刪 pack 的那一處把它記成 skipped、
留著標記；現在和 S3、SFTP 上一樣記成已刪。其他 v3 呼叫端本來就把 NotFound 當成功。

**查證**：假設表裡「單一 PUT 的大小上限放得下一個 pack」原本是 UNVERIFIED。AWS 官方文件：「With a single PUT
operation, you can upload a single object up to 5 GB in size」。結果寫進假設表與 §11.6。

**環境**：Docker Hub 的 `minio/minio` 現在回 pull access denied，`tests/minio-setup.sh` 起不來。這一段用本機快取的
`elestio/minio:latest`，裡面正是腳本釘的 RELEASE.2025-09-07T16-13-09Z。腳本沒有改。

**延到後面的**：寫入結果不確定時各呼叫端的處理（第 4、6 段照 §12 實作）；parity 的 v4 排版（這一段沒有寫
parity 的程式碼，改在第 4 段）。

**gate**：fmt、clippy、windows-gnu clippy 乾淨；nextest 從 635 增加到 643 個通過，9 個略過；帶 S3、SFTP、
rclone 的環境變數也是 643 個通過。
