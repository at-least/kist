# ADR 026：v4 格式——從需求重新推導

日期：2026-10-10　狀態：**設計已確認（2026-10-10，負責人：「全部按照你的推薦」），實作中。** 規格全文
在 `docs/format-v4.md`；實作第 2 段（kist-format）完成之前，程式碼仍是 v3，`docs/format.md` 仍描述現有
程式碼。本 ADR 取代同編號的第一版草稿（commit `054c7e1`）。

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

## 假設

| 假設 | 狀態 |
| --- | --- |
| 沒有要讀回的 v3 資料，不需要遷移 | 負責人陳述（2026-10-10） |
| S3 能以 bucket policy 強制 `If-None-Match` | 查證（外部事實 11c） |
| `maint/gen/` 的條件建立是原子的 | S3、R2、Azure、Ceph、MinIO：文件或原始碼（外部事實 11a、13）；GCS：generation-match 0；SFTP 的 hardlink、本機的 link：v3 已在用；B2、Wasabi：沒有文件。P0 每輪實測，但探針通過不代表每次都原子 |
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
| 單一 PUT 的大小上限放得下一個 pack | 未查（UNVERIFIED） |

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

- 安全性的證明是手寫的。實作第一段的可執行模型、proptest 與外部審查是對策；ADR 016 的教訓是「GC
  鄰接面的每個『顯然』都要過 proptest 與外部審查」。
- 柵欄只和後端的條件寫入一樣可靠。MinIO 在讀取 quorum 錯誤時會略過檢查：這是讀原始碼得到的，沒有
  實測。
- 被入侵的備份主機仍然可以：讀全部資料（它持有主金鑰）、塞滿空間、寫假的 pack（別台沿用前會驗證，
  還原遇到時會換下一份）、偽造自己前綴下的 snapshot。沒有前綴憑證的部署（本機、SFTP），它也能偽造
  別台的 snapshot（負責人決定 1）。
- backup 在 B4 之後、snapshot PUT 完成之前停住：B5 量兩次 E、E 計入休眠，可以把視窗縮到 PUT 本身的
  延遲，但那個延遲仍然是假設。
- **以壞 snapshot 讓 GC 停住**：任何能在 `snapshots/` 寫入的角色（不需要金鑰）都能放一個解不開的
  snapshot，讓所有 prune 停一個 grace；每個 grace 放一個就能讓 GC 一直停。對策：prune 每次都回報，
  維護角色 `forget <key>` 立刻解除；S3 上依 client 分前綴的憑證讓它追得到是哪一台。資料不會因此遺失，
  只是空間回收延後。
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
