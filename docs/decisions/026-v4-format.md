# ADR 026：v4 格式——從需求重新推導

日期：2026-10-10　狀態：**設計已確認（2026-10-10，負責人：「全部按照你的推薦」），實作七段全部完成
（第 1–6 段 2026-10-11，第 7 段收尾 2026-10-18；負責人對第 7 段疑問的決定與後續 2026-10-19，見「實作第七段之二」）。** 規格全文在 `docs/format-v4.md`；kist-core、kist-mount、CLI
與 kist-app 全部是 v4，v3 的 kist-core／kist-mount 模組、測試與 fixture 已在第 6 段移除，kist-format 與 kist-crypto
根下的 v3 模組與 v3 的規格 `docs/format.md` 已在第 7 段移除（`git show 0b3d425:docs/format.md` 取回）。本 ADR 取代
同編號的第一版草稿（commit `054c7e1`）。

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
| S3 能以 bucket policy 強制 `If-None-Match` | AWS：查證（外部事實 11c）；MinIO RELEASE.2025-09-07：不能——政策的條件 key `s3:if-none-match` 建不起來（實測，實作第七段之二）；其他 S3 相容服務未查 |
| `maint/gen/` 的條件建立是原子的 | S3、R2、Azure、Ceph、MinIO：文件或原始碼（外部事實 11a、13）；GCS：generation-match 0；SFTP 的 hardlink、本機的 link：v3 已在用；B2、Wasabi：沒有文件。P0 每輪實測，但探針通過不代表每次都原子。rclone 橋接不是原子的（先 stat 再 rename），P0 依種類拒絕（實作第三段，第 15 項） |
| LIST 在寫入與刪除之後一致 | AWS 有文件；MinIO 只在 xfs/zfs/btrfs（v3 已記）；其他後端：猜測 |
| head 可見的延遲、E 的低估、snapshot PUT 的延遲合計小於 1 小時 | 猜測 |
| 備份角色寫不了 `maint/`、`keys/`、`config` | S3：bucket policy（部署）；本機與 SFTP：與 v3 同樣的信任 |
| E 的單調時鐘計入休眠 | Linux 的 `CLOCK_BOOTTIME`：`clock_gettime(2)` man page；macOS 的 `CLOCK_MONOTONIC`：Apple 的 clock_gettime(3)；Windows 的 QueryPerformanceCounter：Microsoft〈Acquiring high-resolution time stamps〉（實作第七段之二查證；macOS 原本用的 `Instant` 是 `CLOCK_UPTIME_RAW`，休眠時不走，已改）。macOS 與 Windows 沒有在實機上休眠驗證 |
| 真實目錄平均每項約 128 bytes | 量測（2026-10-10，兩個目錄共 36 476 項） |
| mpack 8 MiB、資料 pack 64 MiB | 猜測，放在 head 的 settings，可以改 |
| grace 72 小時、M 1 小時、探針容差 5 分鐘 | 沿用 v3 的 grace 與邊際；容差同 Kopia（外部事實 7c） |
| 沿用前先驗證的成本可以接受 | 猜測：一群相似的機器，每台要下載一次別台寫的內容 |
| 100 萬個 ID 的 map 約 40 MB | 估計（設計者 A） |
| backup 記憶體裡的持有者表（`Map(g_s)` 的每一項 40 bytes 的索引，加上記錄本身每項約 60 bytes）100 萬個 ID 約 100 MB | 估計（實作第 4 段）。第 7 段量：100 萬檔的第一次 backup 峰值 326 MiB（A 集）／362 MiB（B 集），第二次 150／278 MiB；第七段之二（`b48e007`）264／314 MiB、143／285 MiB；沒有單獨量持有者表 |
| backup 記憶體不高於 v3、prune 150–250 MiB | 第 7 段量：backup 成立（A 集 326 MiB 對 v3 的 373–431 MiB）；**prune 估錯**：604–638 MiB，512 MiB 門檻下被殺（dhat 歸因見「實作第七段」）。第七段之二修後（`668c02c`）：第一輪 233／236 MiB、之後 161／171 MiB，門檻內 |
| check 的成本：每一代的 head 讀兩份、讀取端那一代的分片全讀，走過的 ID 各記 32 bytes（目錄、清單、chunk 各一個集合） | 第 7 段量：100 萬個 chunk 的 repo `check` 6 秒、228 MiB；`--read-data` 14 秒、473 MiB（門檻跑法 432／443 MiB）。第七段之二（`b48e007`）：`check` 229／240 MiB、`--read-data` 10 秒、441／436 MiB，門檻內 |
| mount 第一次讀一個檔案時解析整個 chunk 清單（同 v3）就夠快 | 猜測；ChunkIndex 的 `size` 可以讓隨機讀只讀需要的段，沒做 |
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

- **沒變的 backup**：v4 估計約 6–11 個請求，與目錄數無關。實作第 4 段在記憶體後端上數過（`v4_backup_dedup.rs`）：
  10 個與 1000 個目錄都是 11 個請求（pack 還只在 delta 裡時，B4 讀一個 trailer：HEAD 加兩個 range），維護角色
  收錄進 map 之後 8 個（4 個 list、2 個 get、1 個 head、1 個 put）。v3 估計每個目錄約 4 個
  請求（設計者 B 讀碼）；M2 在 v2 時代量過 1 102 個目錄的沒變 backup：MinIO 35 秒，本機 0.8 秒。
  **實測（第 7 段，MinIO，`mc admin trace`；2026-10-19 更正，含 LIST）**：prune 之前 13 個（6 GET、2 HEAD、4 LIST、
  1 PUT）、1.1 秒；prune 寫了新的一代之後第一次 76 個（70 GET：那一代的 66 片 map 分片第一次進本地快取，加每次都有的
  4 個）、之後 10 個（4 GET、1 HEAD、4 LIST、1 PUT）。10-18 記的 9／74／6 漏數了 LIST（見「實作第七段之二」）。
- **2000 個 root**：v4 是一個 root 清單，寫進一個 mpack。v3 是 2000 個樹物件，量測 10.96 秒。**實測（第 7 段）**：
  v4 0.23 秒（同樣的檔案放在一個目錄裡 0.17 秒）、1 個 mpack、24 次 fsync；20 000 個 root 1.01 秒（v3 162 秒）。
- **還原**：合併 range GET 屬於 client 端的改動，與格式無關，v4 前後都可以做。
- **prune**：每輪寫一個 head 與有變動的分片（256 片，改動少就寫得少），全部 map 約 40 MB／100 萬個 ID。
  刪除之前要驗證被引用的 ID；下載量與 condemned 的 pack 有多少有效項目成正比，`by_prune` 的正本不必
  再下載。**實測（第 7 段）**：100 萬個 ID 的分片解碼後 64 MiB，但 prune 的峰值 658 MiB（dhat；主要是以 32 bytes
  的 ID 當鍵的兩張 HashMap 與記錄集合的多份 clone），超過 512 MiB——見「實作第七段」。
- **沿用前先驗證**：別台寫的內容，每台主機第一次沿用時下載一次，之後存在本地快取。沒有採用的替代方案
  是「由 prune 驗證每個新進 pack 的全部項目」：prune 的工作量會變成與新資料量成正比，而且在 prune
  收錄之前的沿用仍然沒有保護。「全部信任」不在 v4 裡；如果日後要加，只能是標明不安全的選項（Kopia 的
  `--safety=none` 也是這樣標示）。
- **刪除延遲**：沒人用的 pack 被標成 condemned 之後，要等到有一個 head 比它晚一個 grace，下一次 prune
  才把它變成 tombstone 並刪除：約一個 grace 加兩次 prune 的間隔。剛寫入不久的 pack 還要先比 grace 老
  才會被標，所以最多約兩個 grace。tombstone 的記錄再留一個 grace。**實測（第 7 段，真實時鐘，grace 2 小時；
  見「實作第七段」）**：每個階段確實要兩輪——「現在的 T」是這一輪 P1 讀到的 head 的時間，等過一個 grace 之後的
  第一輪只是寫一個把 T 推到現在的 head，第二輪才標記；第一次的驗收腳本一個階段只跑一輪、在標記那裡斷言失敗，
  錯的是腳本。第二次跑看到兩件事：(1) 這份資料集刪掉的目錄的 pack 沒有一個是整個沒人引用的（同長度的小文字檔
  跨目錄相同、去重成一個 chunk），走的是 P7c 的 repack——舊 pack 一樣標成 condemned，但數在 repacked 不在
  condemned；(2) init 之後 `settings --grace 2h` 是從 72 小時調低，開始一個 72 小時的 drain（`settings` 印得出來），
  期間的新標記都以 72 小時當 age（§11.4），所以兩小時後的第三階段刪不到東西——這一句是讀碼推的（prune.rs 的 `age`），
  那次跑在第二階段就停了。新 repo 要用短的 grace 只能在 head 0 給：第十步（`9acbafb`）加了 `init --grace`（與 replicas、兩個
  target）。第三次跑（`init --grace 2h`，沒有 drain）實測了刪除：repack 之後再等一個 grace，第一輪什麼都不做、第二輪 tombstone 並
  刪掉那 8 個 pack（521 MiB）——與標記同樣的節奏。照這個規則推算（沒有量）：每天一輪 prune、grace 72 小時，從沒人用到刪掉約 8 天。

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

## 實作第四段：backup（2026-10-11）

負責人：「繼續」（第三段的回報之後）。

**內容**（commit `021d189`、`a15c363`、`a0270f3`、第三步的測試與這份文件的 commit）：
- kist-format：parity 的 v4 排版（v3 的排版、`v` 是 4；向量與 v3 凍結的 `parity-golden.txt` 只差那一個 byte，
  測試直接從 v3 的向量改那個 byte 比對）。邊加邊切的 `Segmenter` 與 `DirBuilder`：backup 走訪一百萬項的目錄時
  不必先收齊全部 entry；`segment` 與 `build_dir` 改用它們實作，§7 的獨立向量照樣通過。
- `kist_core::v4`：`repo`（init 寫 config、槽、head 0 與 `.r1`；open 先只讀 `v`、只 GET 一個槽）、`cache`
  （看過的世代、自己最後一個 snapshot、以名稱存的分片／delta／mpack、驗證過的 (item, pack)）、`state` 與
  `read`（讀取端的世代、§10 的持有者排序、依序嘗試並驗證、mpack 整份讀並快取、B4 的 trailer 檢查；第 5 段的
  restore、mount、check 沿用）、`backup`（B1–B5）。B2 照 GC 模型的 `trusted_holder`：`by_prune`、自己的前綴、
  驗證過的才可信，不可信的候選先取回驗證。E ＝ max(牆鐘經過, `CLOCK_BOOTTIME` 經過)。
- 測試：`v4_backup.rs`、`v4_backup_gc.rs`（B1–B5 每一條拒絕的情境，多數有照常 commit 的對照）、
  `v4_backup_dedup.rs`（跨 client 的取回驗證與快取、竄改的副本、`by_prune`、請求數）、`v4_backup_backends.rs`
  （MinIO、SFTP、rclone，依環境變數）。反向檢查 22 項：每條規則拿掉之後只有對應的測試失敗，改回就通過。

**找到的規格錯誤**（接續第三段的編號）：

| # | 誰找到的 | 問題 | 嚴重度 | 修正 |
| --- | --- | --- | --- | --- |
| 16 | 實作 B4 時對照 GC 模型的 `gate`（advisor 提醒） | §11.2 B4 只要求持有者在 `Map(g_c)` 裡不是 tombstone 或 unreadable。只從 delta 得知的 pack 可以在 B2 與 B4 之間被 P4 收成 unknown（trailer 解得開、版本不認得）；照原文 B4 放行，snapshot 引用的 ID 就只剩一個不算持有者的 pack（§10）。模型的 gate 三種都停，所以模型裡沒有這個問題 | GC 停住：P5 找不到持有者就拒絕，直到有人 forget 那個 snapshot；資料沒有被刪（unknown 的 pack 不刪） | B4 對 unknown 也停止；`b4_refuses_a_holder_that_was_indexed_as_unknown`，反向對照：拿掉 unknown 就失敗 |

**對規格的其他修改**（format-v4.md）：
1. §9、§12：snapshot 主體得到「已存在」時先讀回比對：bytes 相同就是自己寫進去的（S3 的 5xx 重試，第三段的
   回報提過），不同才 +1 奈秒、重新密封、`replicas=1` 時重寫 `.r1`；讀回失敗是 commit 不明。原本只寫 +1 奈秒，
   自己的寫入會變成兩份內容相同的 snapshot。
2. §11.2 B4：B2 時來自 `Map(g_s)` 的持有者不在 `Map(g_c)` 裡就停止。原文沒寫；誠實的流程裡不會發生（記錄要
   先成為 tombstone、再過一個 grace 才移除），模型是停止。
3. §13：parity 的排版定義完成（欄位、片數、上限、明文的理由、向量）；讀取與修復在第 5 段。§15 加 parity。

**與「實作階段」第 4 項的偏離與實作上的選擇**：
- CLI、kist-app、kist-mount 沒換：`kist backup` 還是 v3，v4 的 backup 只在函式庫與測試裡。restore 與 prune 還是
  v3，提早換會讓其他命令讀不了新的 repo；v3 移除時（第 6 段）一起換。
- init 與 open 放在這一段：backup 需要 v4 的 repo，實作階段的清單沒有列 init。
- 沿用 parent 的子樹只比對同一條路徑上的 item；GC 模型比對 parent 裡任何位置的 item。比較窄只會少沿用、多判斷，
  不影響安全；要比對任何位置得先讀完 parent 的整棵樹。沿用由下往上收合，`report.subtrees` 連巢狀的一起算。
- 數值是猜的：delta 每 16 個 pack 一份（最後再一份）；B1 的 lone head 等 5 秒再列一次；記憶體裡的 mpack 最多
  64 MiB。
- 本地快取多了 v3 沒有的兩樣：mpack 的 bytes（沒變的 backup 不必下載 parent 的樹，R5 靠它）與驗證過的
  (item, pack)（§3 的快取只說 index；ADR 第 8 層的「記住看過的世代」也在這裡）。
- 走訪的骨架（排除、記號檔、不跨檔案系統、深度上限、硬連結、快速路徑）是 v3 的複製，約 600 行，v3 移除時
  消失；v3 的 `Marker` 欄位改成 `pub(crate)` 讓兩邊共用。
- v3 的進度回報（progress callback）與 `BackupSummary` 的序列化沒有搬：CLI 與 kist-app 換過來時一起做。
- xattrs 合計 256 KiB 的上限在 ext4 上做不出來（單一 xattr 的值最多一個 block）；「放不進格式的項目記成那一項
  的錯誤」改用超過 1 KiB 的 S3 etag 測。

**量測**：見「成本」一節的「沒變的 backup」（記憶體後端上數請求，與目錄數無關）。

**環境**：同第三段，MinIO 用本機快取的 elestio/minio:latest，以 `docker tag` 標成腳本釘的名稱，腳本沒有改。

**延到後面的**：
- S3 依 client 分前綴的權限（負責人決定 1）在 MinIO 上實測：第 7 段；要在 `tests/minio-setup.sh` 加一個使用者
  與政策（容器裡有 `mc`）。
- 記憶體：持有者表（見「假設」）、驗證過的 (item, pack) 集合、這次寫過的 ID 集合，第 7 段的記憶體 bench 量。
- parity 的讀取與修復、restore 依序嘗試持有者全部失敗時重新載入一次：第 5 段。
- 真的 S3 上的請求數：第 7 段。
- Linux 以外的 E：`Instant` 是否計入休眠沒有驗證（§11.1 的 UNVERIFIED）。

**gate**：fmt、clippy、windows-gnu clippy 乾淨；nextest 從 643 增加到 691 個通過，9 個略過；
`v4_backup_backends` 在 MinIO、SFTP、rclone 上都過。

## 實作第五段：讀取路徑（2026-10-11）

負責人：「繼續」（第四段的回報之後）。

**內容**（commit `17bdd22`、`039ecaf`、`e689d0c`、`8e54441`、`85cbe94`、`3c36131`、`fc6cea9` 與這份文件的 commit）：
- 讀取端（`kist_core::v4::live`、`snapshots`、`read`）：`usable_state` 由大到小取第一個 head 與全部分片都讀得了的
  世代，跳過的回傳原因；`Live::run` 在持有者全部失敗（新的錯誤 `Unavailable`）時重新載入一次再試，限流 60 秒、
  以 epoch 判斷別人是不是已經載入過；`Live::refresh` 不限流，給 mount 用。snapshot 主體讀不了讀 `.r1`，兩份都
  不在是 `SnapshotNotFound`；列出、選取 snapshot（同 v3 的指定方式）。`Reader::lookup` 沿 Dir 的索引只讀一段。
- restore 與 dry run（`kist_core::v4::restore`）：落點照 `Root.t`；部分還原、SNAPSHOT:PATH、dry run 的規則同 v3。
- mount 的核心（`kist_mount::v4::FsCore`）：與 v3 同樣的方法；頂層照 `Root.t` 展開、目錄依索引查找、chunk 的
  長度取自持有者的 `r`、兩條重新載入各自限流。
- check（`kist_core::v4::check`）：§13.1，規格原本沒有這一節。
- 第四段留下的不穩定測試（見下）。
- 測試：`v4_read.rs`（9）、`v4_restore.rs`（28，其中 19 個搬自 v3 的 `restore_hardening.rs`）、
  `v4_restore_select.rs`（21，搬自 `restore_select.rs`）、`v4_restore_fd_limit.rs`（1）、`v4_check.rs`（25）、
  kist-mount 的 `v4_fs_read.rs`（13，搬自 `fs_read.rs` 再加 v4 的）與 4 個單元測試；`v4_backup_backends.rs` 在
  真的後端上加了 restore 與 `check --read-data`。反向檢查 47 項（讀取端 8、restore 9、mount 4、check 26），每一項
  都讓對應的測試失敗。

**找到的規格錯誤**（接續第四段的編號）：

| # | 誰找到的 | 問題 | 嚴重度 | 修正 |
| --- | --- | --- | --- | --- |
| 17 | 實作 `check --repair` 時（advisor 指出 P7c、P7d 的後果） | §13 說 parity「讀取與修復（`check --repair`）在實作第 5 段」，但 §1、§0.4 不許覆寫，§11.5 的不變式 1 說只有 P9 會刪 pack：在的、但損壞的 pack 沒有合規的方法把修好的 bytes 寫回去。更糟的是 P7c、P7d 驗證失敗就把項目從記錄移除（當成不實的），不先試 parity | parity 在現在的規格裡救不回任何東西：位元損壞的項目若是那個 ID 唯一的持有者，prune 驗證時把它移除，P5 就一直拒絕、直到有人 forget，那個 snapshot 也還原不了那一項——即使 parity 修得回。資料不會被 kist 刪掉（P7d 驗證失敗時 pack 留在 condemned） | 這一段不修：check 只證明修得回（parity 修出的 bytes 重算 BLAKE3 等於名稱，或 `.r1` 完好），列在 `repairable`、不寫回；選項放進回報的疑問（見下） |

選項（給負責人）：
- **A** `check --repair` 先刪再建立：要維護角色，多一個 P9 以外的刪除者（違反不變式 1 的字面），而且刪與建之間
  當掉會失去整個 pack（parity 需要損壞的那一份當資料片），必須先把修好的 bytes 寫進本地暫存。
- **C** 讀取路徑在持有者全部失敗時以 parity 在記憶體修復：不寫任何東西，代價是每次失敗下載整個 pack 與 parity，
  要一個修好的 pack 的小快取；修不了 P7c、P7d 的問題。
- **D** P7c、P7d 在宣告項目不實之前先以 parity 在記憶體修復，修得回（重算 BLAKE3 等於名稱）就經 P7c 既有的
  `by_prune` 路徑寫進新的 pack：符合「只有 P9 刪」與「記錄只來自 prune 驗證過的資料」，是第 6 段的工作。建議 D，
  restore 要不要再加 C 另外決定。

**對規格的其他修改**（format-v4.md）：
1. §9：主體讀不了讀 `.r1`；兩份都不在是「找不到」；列出時只列主體。
2. §10：退回較舊世代的條件（head 與全部分片）與 check 回報跳過的世代；「重新載入一次」寫明內容、60 秒的限流與
   epoch 規則（原文只說「一次」，限流沿用 v3 的 restore）；mount 看到新 snapshot 的重新載入另外計；目錄依索引查找。
3. §13：在的、但損壞的物件不寫回（第 17 項）；新的 §13.1 check：不讀資料、`--read-data`、`--repair` 的六種只在
   不存在時建立的修復、不檢查的東西。§15 加讀取端與 check 的情境。

**與「實作階段」第 5 項的偏離與實作上的選擇**：
- CLI 的 `kist restore`、`kist mount`、`kist check` 與 FUSE 介面沒換，還是 v3；v4 的讀取路徑只在函式庫與測試裡，
  v3 移除時（第 6 段）一起換。
- restore 的檔案系統那一層不複製（與第四段的走訪不同）：v3 `restore.rs` 的暫存檔加 rename、擋路的東西、目錄
  handle、硬連結、唯讀目錄、選取、metadata 的順序改成 `pub(crate)`，吃 Entry 的函式改用 `RestoreEntry` trait，
  v3 與 v4 的 Entry 各實作一份。理由：那一層是 ADR 019 的強化，複製一份就失去 v3 那 62 個測試的覆蓋；共用之後
  v3 的測試照樣覆蓋它，v4 只搬走訪層的測試。v4 只寫走訪。代價是這一段改到了 CLI 還在用的 v3 restore（只有可見性
  與型別，v3 的測試全過）。v3 移除時把這一層搬進 v4。
- mount 的核心是新寫的（約 700 行），v3 的 `corefs.rs` 與 `vpath.rs` 在 v3 移除時刪掉；v4 的去重 inode 表與 chunk
  LRU 照 v3。
- mount 第一次讀一個檔案時解析整個 chunk 清單（同 v3），ChunkIndex 的 `size` 沒有用來跳段。
- 兩條重新載入原本共用一個時間戳：mount 看到新 snapshot 的那一次會擋住之後 60 秒內的失敗重試，寫測試時才發現，
  改成各自限流（v3 本來就分開）。mount 開始時先列出 snapshot 再載入世代，那些 snapshot 不觸發重新載入。
- check 的成本與記憶體（見「假設」）沒有量；`--repair` 只建立物件、不做 P0（不需要刪除權限）。
- 第四段的 `one_changed_file_in_a_large_directory_rewrites_one_segment` 不穩定：兩萬個固定名稱在亂數的 K_cut 下約
  1% 完全不切段。實驗：3000 把亂數金鑰有 21 把不切（0.70%）；重跑原測試 220 次失敗 5 次；改用依這個 repo 的
  K_cut 挑名稱（`names_with_cuts`）之後 300 次 0 次失敗。

**環境**：同第四段（MinIO 用本機快取的映像以 `docker tag` 標成腳本釘的名稱，腳本沒有改）。

**延到後面的**：
- 第 17 項的修法（建議 D，第 6 段）與要不要加 C。
- ChunkIndex 的跳段讀、check 的成本與記憶體：第 7 段的 bench。
- CLI 與 FUSE 介面換成 v4、`snapshots`／`restore`／`check` 的輸出與 JSON：第 6 段。

**gate**：fmt、clippy、windows-gnu clippy 乾淨；nextest 從 691 增加到 792 個通過，9 個略過；帶 S3、SFTP、rclone
的環境變數也是 792 個通過，`v4_backup_backends` 的 3 個在 MinIO、SFTP、rclone 上都過。

## 實作第六段：prune 與維護、前端換成 v4、移除 v3（2026-10-11）

負責人：「繼續」（第五段的回報之後；規格錯誤第 17 項照建議採修法 D、不做 C，也是這一句）。

**內容**（commit `168d8aa`：第 12–19 步；`0b3d425`：第 20–22 步；與這份文件的 commit。逐步的歷史在標籤
`v4-steps-archive`）：
- 維護角色的共用步驟（`kist_core::v4::maint`）：同一台主機一次一個維護命令的檔案鎖（repo 的本地快取目錄裡的
  `maint.lock`，沒有快取時在系統暫存目錄、以快取目錄的名稱區分 repo）；P0 的探針以命令自己的時鐘讀本機牆鐘
  （kist-backend 新加 `Backend::probe_with`）；P1 讀列出來的最大世代，`T(g)` 取讀到的那一份的修改時間，`T(g)`
  比本機時鐘晚超過 M 就回報、以本機時鐘 + M 當作現在，列表裡有主體沒有 `.r1` 就補寫；柵欄（先 HEAD 下一代的
  `.r1`，再只在不存在時建立主體，成功後寫 `.r1`，失敗就停止、什麼都不刪）；兩個時鐘的 `window_passed` 與
  「現在」取小。維護命令一律讀後端、不用本地快取。
- 改 settings（§11.4）：P0、P1 之後以柵欄寫 head g+1，分片、bad、hold 照抄；範圍照 §10（kist-format 的
  `Settings::check`）；grace 調低開始 drain，drain 期間不接受 grace 的變更、其他 settings 照改；沒有改變不寫。
- forget（§9，`kist_core::v4::forget`）：分組 (client, roots 的 `path` 與 `t`)，每一組最新的一份是下一次 backup
  的 parent、不刪，明確指定它要 `--retire`；完整的 key 不解密也刪得掉（只有 `.r1` 的、兩份都解不開的，P5 停住時
  的解除方法）；先刪 `.r1` 再主體。保留政策搬到與版本無關的 `crate::retention`。
- prune（§11.3，`kist_core::v4::prune`）：P0–P9 照 GC 模型逐段實作；P3、P5 拒絕前先重新列出世代；P4 讀 trailer
  時後端的暫時錯誤停止這一輪，不把好的 pack 記成 unreadable；P7c 的 repack（一個舊的對一個新的、讀回驗證、舊的
  保留全部項目）與第 17 項的修法 D（見下）；P8 只寫有變動的分片；P9 刪不掉的回報、記錄留在 tombstone 下一輪
  再試，超過整輪時限就停止並回報沒做完（已寫的世代有效）。kist-format 的 trailer 與 snapshot 解碼先只讀 `v`
  （`peek_version`），版本不認得時不論其他欄位都回 Version 錯誤，P4 才分得出「看不懂」與「壞掉」。
- 重建（§11.4，`kist_core::v4::rebuild`）：照模型的 Rebuild；沿用條件只看 `w`；已有 hold 時合併；分片全部重寫、
  補寫 `g_lost` 缺的 `.r1`。prune 的「列出 pack」與「新進者的記錄」抽成共用。
- key add／list／remove（§3，`kist_core::v4::repo`）：Repo 記著 master（離開時清零）、config 原樣的 bytes（槽的
  AAD 綁 BLAKE3(config)，重新編碼可能丟掉未知的欄位）與開啟時用的槽；`key remove` 拿維護鎖、不走柵欄。
- 移除版本不認得的 pack（負責人決定 10，`Repo::condemn_unknown`，CLI 的 `remove-unknown-packs`）：維護角色明確
  下令，以柵欄寫一代把 unknown 的記錄改成 condemned，之後照一般的兩階段刪除，不另外多一個刪除者；寫進 §11.4。
- backup 的進度回報（`BackupOptions::progress`，事件與 v3 的 `BackupProgress` 相同），kist-app 的網頁要顯示。
- 前端（第 20 步）：CLI 每個命令開 v4 的 Repo；`rebuild-index` 改名 `rebuild`；新加 `settings`（不帶選項是顯示）、
  `key add／list／remove`、`remove-unknown-packs`；`prune` 只剩 `--repack-below`（v3 的 `--grace`、`--dry-run`、
  `--inactive-after`、`--clock-skew` 不再接受），刪不掉的或沒做完的以結束碼 3 回報；`backup` 拿掉 `--gc-grace`；
  `forget` 新加 `--retire`，`--prune` 與 `--dry-run` 互斥；`--json` 的欄位改成 v4 的形（backup 的 `snapshot`、
  `generation`、roots 是 `{path, type}`；prune 與 rebuild 印 PruneReport／RebuildReport；settings 與 key list 也有
  `--json`）。kist-app：工作、daemon 與網頁走 v4；設定拿掉 `backup.gc_grace`、`prune.grace`、`prune.clock_skew`、
  `prune.inactive_after`（現在是 unknown field 錯誤），`prune.repack_below` 對應到 `PruneOptions`；`cache_dir` 沒設
  時改用使用者的快取目錄（同 CLI：維護鎖在 repo 的快取目錄裡，預設不同就不是同一個鎖檔；v3 時沒設就不用快取）。
  kist-mount 的 mount 收 v4 的 Repo。kist-core 為前端補的：`list_snapshots_with_roots`、`Repo::settings`、
  可序列化的 `BackupSummary`、`Live::run_send`、轉出 `ClientId`。
- v3 的測試搬進 v4（第 21 步）：移除 v3 之前，先把「v3 移除後仍在的程式碼、卻只有 v3 的測試覆蓋」的測試搬過來
  （v4 的走訪是 v3 的複製；共用的排除規則、fsmeta 與保留政策原本只由 v3 的測試覆蓋）：`v4_backup_exclude.rs`
  （14）、`v4_source_backup.rs`（9）、`v4_backup_robustness.rs`（8；大檔快速路徑的兩個不搬，v4 沿用 chunk 清單時
  把它當成沿用的子樹）、`v4_backup_restore.rs`（8；跨段的那個改用依這個 repo 的 K_cut 挑的名稱）、`retention.rs`
  （6），`v4_restore.rs` 與 `v4_forget.rs` 各加一個；不搬的是 v3 才有的機制（index、trees、touch）。反向檢查 13
  項，每一項都讓對應的測試失敗；其中搬過來的 `fast_path_uses_parent_start_time` 原本抓不到快速路徑用錯時間
  （整個 kist-core 沒有任何測試抓得到），補上前半：第一個 backup 的時鐘撥早一小時，第二個一個都不能沿用。
- 移除 v3（第 22 步）：kist-core 刪掉 v3 的 backup、cache、check、forget、index、pack、prune、reach、rebuild、
  repo、snapshots，kist-mount 刪掉 `corefs` 的 FsCore 與 vpath，連同 v3 的測試、`tests/common` 與凍結的 v3
  fixture。留下的共用部分：來源與排除規則的型別（`SourceSpec`、`Exclusions`、`Marker`）搬進 v4 的 backup；
  restore 的檔案系統那一層留在 `crate::restore`；`fsmeta`、`filter`、`retention`；mount 介面的型別搬進 kist-mount
  的 v4。`CoreError` 拿掉只有 v3 用的變體。restore 的三個單元測試（ADR 019 A2、A3）原本只測 v3 的走訪，測試接縫
  加進 v4 的走訪、測試搬過去，三項反向檢查各自讓對應的測試失敗。
- 測試：`v4_settings.rs`（19）、`v4_forget.rs`（12）、`v4_prune.rs`（44）、`v4_rebuild.rs`（11）、`v4_keys.rs`
  （6）；`v4_backup_backends.rs` 在 MinIO 與 SFTP 上多跑 prune（寫出下一代、沒有到期的東西就不刪、之後
  `check --read-data` 乾淨），rclone 上 P0 拒絕、什麼都不寫（時間不能快轉，刪除的路徑只在虛擬時鐘的測試裡跑）；
  CLI 的 7 個改成 v4 的語意、kist-app 拿掉 v3 grace 的 6 個、設定新加 2 個。測試的虛擬時鐘（`tests/v4_common`
  的 VClock）：hooked 後端的修改時間是寫入當下的虛擬後端時間，維護命令與 backup 的兩個時鐘也從它讀，兩邊各有
  自己的偏差。反向檢查（scratchpad 的 mutate6.py，對照 GC 模型的 `Mutations`）：settings 12、forget 6、prune
  43、repack 與修法 D 12、重建 10、key 3、unknown 1，合計 87 項，每一項都讓對應的測試失敗；其中 5 項原本照樣
  通過，測試改過才抓得到（第 14 步的 commit 列出）。第五段 restore 與 mount 的 11 項反向檢查在移除 v3 之後重跑，
  全部仍讓測試失敗。

**找到的規格錯誤**（接續第五段的編號；第 17 項在這一段修）：

| # | 誰找到的 | 問題 | 嚴重度 | 修正 |
| --- | --- | --- | --- | --- |
| 17（修法） | 第五段的回報，負責人「繼續」 | 在的、但損壞的 pack 沒有合規的方法修回去；P7c、P7d 驗證失敗就移除項目 | 見第五段 | **修法 D**（第 15 步）：prune 讀到的副本驗證失敗時（P5 的 metadata item、P7c 的候選、P7d 的正本）先整份讀那個 pack，名稱與內容不符而有 parity 就在記憶體裡修復，重算 BLAKE3 等於名稱才算數；修得回的副本不移除、照樣走訪，修好的 pack 一律 repack 寫進新的 pack，損壞的那個 condemned、之後照常兩階段刪除——不覆寫任何東西、只有 P9 會刪。P7d 不憑修好的副本把 condemned 的持有者標成 tombstone，留一輪。名稱相符的 pack（某一項本來就是假的）parity 救不了，照樣移除。修法 C（restore 以 parity 在記憶體修復）沒做（見「留給負責人的」） |
| 18 | 實作 forget 時（第 13 步） | §12 寫寬鬆後端上「prune 與其他維護命令不能用」，理由是柵欄需要原子的條件建立；forget 不建立物件、不走柵欄、不需要 P0，照原文會把一個安全的操作擋掉（`rclone://` 上的 repo 永遠刪不了 snapshot，壞 snapshot 讓 prune 停住時也解不了） | 低：只是過度限制，沒有資料遺失方向 | §12 改成：prune、重建、改 settings、`remove-unknown-packs`（都要 P0 與柵欄）與 `check --repair` 不能用；forget 與 `key add／list／remove` 可以用 |

**實作的漏洞**（不是規格錯誤）：check 對只在 delta 裡、map 還沒收錄的 mpack 不檢查兩份（第一次 prune 之前每個
mpack 都是這種）：`.r1` 不見不回報、`--repair` 不補、主體不見也不回報。換 CLI 時的測試發現（第 20 步），改成
delta 提到的 mpack 的兩份照記錄的方式檢查與修復（delta 提到卻完全不在列表裡的 pack 仍只是警告），`v4_check.rs`
加一個（修之前在 `has no .r1` 的斷言失敗）；§13.1 補一句。

**對規格的其他修改**（format-v4.md）：
1. §11.3 P5、P7c、P7d：修法 D 的規則；§13 的「損壞的副本怎麼修回來還沒有定義」改成指向它，並寫明 restore 與
   mount 不用 parity、稠密的 live 資料 pack 裡損壞的 chunk 不會被修（C 沒做）。
2. §11.3 P7c：候選的條件寫成 `repack_below`（預設 50%、0 不依比例）；修好的 pack 不論比例一律 repack；一個舊的
   對一個新的；寫入失敗只回報。§13：repack 為新 pack 寫 parity 的片數取自舊 pack 的 parity（settings 沒有 parity
   的欄位，實作上的選擇）。
3. §11.3 P7d：驗證反覆到沒有變動；修好的副本那一輪不讓 tombstone 成立。
4. §11.4：drain 的「現在」是最新 head 的時間，所以調低之後要有更新的世代 drain 才會結束；drain 期間其他 settings
   照改；沒有改變不寫 head。新的「移除版本不認得的 pack」命令（負責人決定 10）。
5. §10、§11.3 P1：`T(g)` 是讀到的那一份（主體，不行就 `.r1`）的修改時間。§11.3 開頭：檔案鎖的位置、哪些命令
   共用它、維護命令不用快取。
6. §3：`key add` 的規則；`key remove` 不走柵欄、拿維護鎖，不同主機同時互刪時可能兩個都刪掉（接受）；config
   原樣的 bytes。
7. §9：forget 的分組、roots 讀不出來的 snapshot、明確指定的寫法、完整的 key 不解密也刪得掉、不走柵欄。
8. §12：第 18 項。§13.1：delta 提到的 mpack 的兩份照記錄的方式檢查與修復。§15：v3 的 fixture 已移除。
9. 開頭的狀態。

**與「實作階段」第 6 項的偏離與實作上的選擇**：
- 第 6 項的清單多了前端切換、v3 的測試搬進 v4、移除 v3：第四、五段都寫「v3 移除時（第 6 段）一起換」。
- restore 的檔案系統那一層留在 `crate::restore`，不是第五段寫的「搬進 v4」：它與 `filter`、`fsmeta`、`retention`
  一樣不屬於某個格式版本；實際只刪掉 v3 的走訪與 `ReloadableIndex`，路徑不變。
- 數值與選擇：`--repack-below` 預設 50（同 v3）；D 只修 prune 讀到的項目；parity 的片數沿用舊 pack；unknown 改成
  condemned 時 `age` 用目前的 age；kist-app 的 `cache_dir` 預設改變（見上）。
- `check --repair` 不做 P0、只建立物件（不需要刪除權限）；`forget`、`key add`、`key list` 不拿維護鎖；`key remove`
  拿鎖但不走 P0 與柵欄。
- `Backend::put` 不再有正式的使用者，只剩測試（造出損壞的物件、後端的合約測試）；要不要移除放進「留給負責人的」。
- `fuzz/` 編不過（第 22 步之前就是：pack 與 parity 兩個 target 跟不上之前的改動），四個 target 都用 v3；第 7 段
  重新播種時改成 v4。
- kist-format 與 kist-crypto 根下的 v3 模組還在：v4 與其他 crate 還用到其中的 `cbor`、`config::ChunkerParams`、
  `tree::meta_kind`，以及 kist-crypto 根的共用型別（`MasterKey`、`KdfCost`、`CryptoError`、`random_bytes`；
  以 grep 逐一查過使用者）；v3 才有的（`index`、`keys`、`snapshot`、`tree` 的結構、v3 的 `pack` 排版與 `parity`、
  v3 的金鑰綁定 `KeyBinding`、v3 的 golden 與向量測試）第 7 段整理：留下共用的、移除其餘，`docs/format.md` 同時
  刪除。第 2 段寫的「`format-v4.md` 取代 `format.md`」到那時才完成。
- 搬過來的排除規則測試有 2 個只在 Windows 或非 unix 上編譯，Linux 上跑不到，只由 windows-gnu clippy 型別檢查。
- kist-app 的 prune metrics：`kist_prune_marked_objects` 與 `kist_prune_live_packs` 從工作結果的 JSON 讀 `marked` 與
  `live_packs`，v4 的 `PruneReport` 沒有這兩個欄位（只讀碼，UNVERIFIED：沒有跑 daemon 確認），所以 v4 之後它們不會
  更新；`kist_prune_deleted_bytes_total` 讀的 `deleted_bytes` 還在。寫這份文件時發現，放進第 7 段。

**留給負責人的**：
- 修法 C 要不要做：D 只修 prune 讀到的項目（被引用的 metadata item、repack 候選、刪前驗證的正本），稠密、live
  的資料 pack 損壞時 restore 仍讀不到那一份；要修它要嘛 restore 自己以 parity 在記憶體修復（C：每次失敗下載整個
  pack 與 parity），要嘛另加一個整份讀每個 pack 的 prune 模式。
- `Backend::put` 要不要移除（只剩測試在用）。
- 第 7 段的清單（PLAN M12）要不要調整。

**環境**：同第五段（MinIO 用本機快取的映像以 `docker tag` 標成腳本釘的名稱，腳本沒有改）。

**延到後面的（第 7 段）**：凍結 v4 fixture；fuzz 改成 v4 並重新播種；記憶體 bench（持有者表、驗證過的 (item, pack)
集合、這次寫過的 ID 集合、check 的成本與記憶體）；MinIO 與 SFTP 整合重跑、S3 依 client 分前綴的權限在 MinIO 上
實測（`tests/minio-setup.sh` 加使用者與政策）；重量 V21 與 M2 的請求數、真的 S3 上的請求數；ChunkIndex 的跳段讀；
kist-format／kist-crypto 的 v3 根模組與 golden、`docs/format.md`；README 的驗收數據重量；`Backend::put`；
kist-app 的 prune metrics；Linux 以外的 E（§11.1 的 UNVERIFIED）。

**gate**（這份文件的 commit 之前在 `0b3d425` 的樹上重跑，第一手）：fmt、clippy 乾淨；windows-gnu clippy
（cargo-zigbuild，workspace 不含 kist-mount／kist-cli，與 kist-cli 單獨）乾淨；nextest 700 個通過、8 個略過
（第五段 792＋9 → 第 19 步 884＋9 → 第 21 步 927＋9 → 移除 v3 後 700＋8：刪掉的檔案裡 230 個，其中 2 個只在
Windows 或非 unix 上編譯）；`KIST_TEST_FUSE=1` 的 `mount_round_trip` 在第 20、22 步過（這份文件的 commit 沒有重跑）。
## 實作第七段：收尾（2026-10-18）

負責人：「全部按照你的建議做到完」（第六段的回報之後）。建議的內容：第 7 段整段做完，而且第六段「留給負責人的」
三項都照建議處理——`Backend::put` 移除、修法 C 不做而改加 `prune --read-data`、第 7 段的清單照 PLAN 再加上第六段
文件步驟發現的 prune metrics。一步一個 commit、不 squash、不 push。

**內容**（每一步一個 commit，gate 寫在各 commit 訊息裡）：
1. `879570e` 移除 `Backend::put`：v4 不覆寫任何物件（§1、§12），`Backend` 從此沒有覆寫的 API——kist 的任何
   角色都發不出會覆寫的請求，不只是「沒有人呼叫」。SFTP 後端拿掉 `PutMode::Overwrite` 的分支；v3 的契約測試
   `contract.rs` 刪除；需要「把物件換成壞的」的測試改用測試用的 `overwrite`（直接走 `object_store`，不經 `Backend`）。
2. `2c74d3a` 移除 kist-format／kist-crypto 根下的 v3 模組、刪 `docs/format.md`：kist-format 根下只留 v4 也用的
   `cbor`、`chunker`（原 `config::ChunkerParams`）、`ids`、`parity`（`encode_version`／`parse_version`，v3 的包裝與
   key 拿掉）；kist-crypto 根下只留共用型別（`MasterKey`、`KdfCost`、`CryptoError`、`random_bytes`、nonce 與 tag
   長度），zstd 依賴拿掉；v3 的 golden 與向量測試刪除。`format-v4.md` 原本把切塊演算法交給 `format.md`，刪它之前
   把規範（gear 表的摘要、兩個 mask、邊界函數、與緩衝區切法無關）從 v2 的 `format.md` §12 搬成 §6.1，逐句對照
   kist-chunker 的程式碼。golden-v4 與向量一個 byte 都沒變——這就是格式沒被改到的檢查。
3. `3236728` fuzz 改成 v4 並重新播種：四個 target 都對準 v4 的讀取端（`cbor` 以第一個 byte 選九種結構；`pack`
   走 `trailer_of`、chunk 與 metadata item 的壓縮→密封→解開 roundtrip、`split_trailer`／`parse_footer`／`unframe`；
   `parity` 以 golden-v4 的 parity 當凍結的修復基準；`chunker` 只改參數的路徑），seeds 從 golden-v4 重新產生。
   每個 target 煙霧 120 秒：`cbor` 跑了幾十秒就抓到一個 panic（下一項）；修後 `pack` 64 562、`parity` 111 180、
   `chunker` 43 318、`cbor` 10 211 619 次執行，都乾淨。這一步的 gate 還抓到 cargo deny 的四個 advisory
   （rustls 0.23.45，RUSTSEC-2026-0346～0349；kist 沒有自訂 session store 也沒開 ECH），`a3833a4` 升到 0.23.46。
4. `5002aa8` fuzz 找到的 bug：`Snapshot::decode` 驗 parent 欄位時把 key 裡的時間戳交給 `time` 的 parser，
   `20260904T1200000[0000.1Z` 這種字串讓 time 0.3.55 在 debug（與 fuzz 的 `-Cdebug-assertions`）下減法溢位 panic。
   讀取端不能因為一個壞掉的欄位 panic：改成先驗形狀（恰好 25 個 ASCII 字元 `YYYYMMDDTHHMMSSnnnnnnnnnZ`、數字自己
   讀）再以 `time` 的 `Result` 建構子組出時間、最後重新格式化必須等於輸入；規範寫法的結果不變（canonical 測試、
   fixture、golden 都照過）。先紅後綠的單元測試（fuzz 的輸入加十五種壞寫法）；crash 輸入留在
   `fuzz/seeds/cbor/4_snapshot_bad_parent_time` 當回歸種子。
5. `dafc043` `prune --read-data`（修法 C 的替代）：修法 D 只修 prune 讀到的項目（P5 被引用的 metadata item、P7c 的
   repack 候選、P7d 刪前驗證的正本），稠密、live 的資料 pack 裡位元損壞的 chunk 誰也不讀，有 parity 也修不回。
   不做 C（restore 自己以 parity 在記憶體修復、不寫回：每次都重新下載、資料永遠壞著）；改加一個 prune 模式：
   P7c 之外，每個 live、當正本的資料 pack 整份讀、逐項驗證它當正本的 chunk（mpack 不必：P5 已逐一取回驗證），
   驗證失敗就走既有的 parity 修復與 repack（損壞的那個 condemned、之後照常刪除，不覆寫）；修不回的項目從記錄
   移除並回報（同 P7c 對不實項目的處理；它若是唯一的持有者，下一輪 P5 拒絕——資料真的遺失了，大聲失敗）。
   整輪的時間用掉一半（elapsed × 2 + M ≥ grace）就停、回報讀了幾個，剩下的留給下一次，這一輪照樣寫 head。
   CLI 的 `prune --read-data`（`forget --prune` 也吃）、設定檔 `[prune] read_data`、`PruneReport.read_packs`；
   規格 §11.3 P7c 與 §13（`check --read-data` 回報 repairable 之後接著跑它）。先紅後綠四個測試，反向檢查（拿掉
   `read_data` 的分支）四個都紅。
6. `2e6b20b` kist-app 的 prune metrics：`kist_prune_marked_objects` 與 `kist_prune_live_packs` 讀的是 v3 report
   的 `marked` 與 `live_packs`，v4 的 `PruneReport` 沒有這兩個鍵，換 v4 之後永遠是 0；原本的測試 fixture 手寫成 v3
   的形狀所以沒抓到。`PruneReport` 加 `live`（這一輪結束時 live 的記錄數），metrics 改讀 `condemned` 與 `live`；
   測試的 fixture 改成直接從 `PruneReport` 序列化（欄位改名時測試會跟著紅）。
7. `1f70388` 凍結 v4 fixture（`crates/kist-core/tests/fixtures/v4/repo`，264 KiB）：固定密碼、client id、mtime
   與小參數做出來的兩個 snapshot。四個測試：打得開、check 乾淨、兩個 snapshot 都還原得出來；每個物件（config、
   head、snapshot、每個 mpack 的每一項）解碼後重新編碼逐 byte 相同；同樣的來源再備份一次不寫新 chunk；每個 parity
   修得回它的 pack、刪掉正本後還原走 `.r1`、`check --repair` 從 `.r1` 重建、之後 check 乾淨。寫 fixture 的測試留著
   （ignored，`KIST_WRITE_FIXTURE=1`）；只在 unix 跑。
8. `f24675b` S3 依 client 分前綴的權限在 MinIO 上實測（負責人決定 1 的部署到這一步才有實測）：
   `tests/minio-setup.sh` 多建 `kistclient`，政策只允許在 `kist-test/*/{packs,mpacks,parity,deltas,snapshots}/<固定
   client id>/*` PutObject、GetObject 全部、ListBucket；MinIO RELEASE.2025-09-07 接受 resource 中段的 `*`
   （第一次跑就過）。測試：client 憑證備得了份；對別台 client 的前綴、`maint/probe`、`maint/gen/1`、`keys/` 的建立
   都被擋且物件不存在；刪自己的 snapshot 被擋；冒充另一個 client id 備份失敗、沒有多出 snapshot；prune 在 P0
   探針建不起來就失敗、`maint/` 底下只剩世代 0 的兩份 head。整合全開（MinIO、SFTP、rclone、FUSE）的 nextest 抓到
   一個第六段留下的過期測試：kist-cli 的 `s3_repo_url_works_end_to_end` 還在等 v3 的「開 bucket versioning」提醒，
   `0b3d425` 換成 v4 時把它換成「留一份 config 與密碼」（config 不再覆寫），MinIO 沒開所以 gate 沒跑到；斷言改成
   新的提醒。
9. `23d0b01` 量測：bench 與驗收腳本改成 v4（下一節），log 進 `tests/acceptance/`。
10. `55f8e56` 量測找到的：讀取端的「驗證過的 (item, pack)」只有 backup 需要。`Reader` 對每一筆從別台的副本讀到
    並驗證成功的 (item, pack) 都記進一個 HashSet 與一個 Vec，為了 backup 結束時寫回本地快取（B2 的第三種可信）；
    restore、mount、check 的讀取端（`live.rs`、`check.rs`）以 `own = None` 建立，每個 chunk 都算「別台的」，記了卻沒有
    人取走——dhat 在 100 萬個 chunk 的 restore 裡量到 195 MiB（HashSet，含 rehash 時的雙份）加 64 MiB（Vec）。改成只有
    `with_verified`（backup）的讀取端才記；`trusted` 在沒有記錄時只看 `by_prune` 與自己的前綴（與原本空集合的行為
    相同）。先紅後綠：`v4_read.rs` 新加一個測試——沒有 `with_verified` 的讀取端 `note_verified` 之後 `take_new_verified`
    是空的、`trusted` 仍是 false；有的那個記下來、變成可信。重量：restore A 集 362 → 148 MiB（門檻跑法 156，過），B 集 789 → 538 MiB（門檻下仍被殺：剩下的是
    `open_placement` 一次把整個目錄的 100 萬個 `Entry` 解進記憶體的 336 MiB，dhat 裡是單一一個配置）；`check --read-data`
    沒有變（它的峰值不是這個集合，見下）。M1／M2 的驗收 log 是修之前的 binary 跑的（restore 與 check 的時間與 RSS
    不受影響：M1 的 restore 同一台 client，M1／M2 的 RSS 峰值在 backup）。
11. `9acbafb` `init` 可以指定 head 0 的 settings（`--grace`、`--replicas`、`--pack-target`、`--mpack-target`；parser 同
    `settings`，沒給的用預設、replicas 看後端，範圍由 `Head::encode` 的檢查擋在寫任何東西之前）。真實時鐘的 GC 驗收
    找到的：init 之後 `settings --grace 2h` 是調低，開始 72 小時的 drain，期間的標記都以 72 小時當 age（§11.4），兩小時後
    的刪除階段永遠刪不到——新 repo 要用短的 grace 在命令列上沒有辦法（core 的 `InitOptions.settings` 早就有）。先紅後綠：
    `cli.rs` 的 init_takes_the_settings_of_generation_0_without_a_drain（含 `--grace 1h` 被擋且沒寫 config、init 之後調低的
    對照）。規格 §4 加一句。
12. 這份文件的 commit：ADR、PLAN、README 的驗收數據、`format-v4.md` §15、`bench/memory/README.md`、
    `tests/acceptance/README.md` 與 `run_gc.py`（三次跑的教訓）、GC 的 log。

**量測**（release build；16 核／62 GiB／ext4；詳細的命令與 log 在各目錄。binary 的來歷有三種：M1、M2、V21、請求數與
bench 的 backup／prune／check 是 `2e6b20b` 的程式碼（第 7 項之後的 commit 到那時只有測試與腳本）；restore 與 check
的重量是 `55f8e56`（第 10 項修後）；GC 是 `9acbafb` 的 binary（第 11 項之後的第三次跑；前兩次失敗的原因在那一項）：

- 記憶體 bench（`bench/memory/run-v4.sh`，兩個 100 萬檔的測試集，數字是 `memory.current − file − slab_reclaimable`
  的取樣峰值；門檻 = `MemoryMax=512M` 不被殺）：
| 命令 | A 集（1000 dir × 1000 檔） | B 集（單一平面 1M 檔） | 512 MiB gate |
  |---|---|---|---|
  | backup 第一次 | 326 MiB，90 s | 362 MiB，68 s | 過（285／354） |
  | backup 第二次（快速路徑） | 150 MiB，41 s | 278 MiB，18 s | 過（141／281） |
  | prune 第一輪（收錄 16 個 pack、5 個 mpack）／第二輪 | 604／638 MiB，10 s | 587／638 MiB，8 s | **A、B 都被 OOM 殺掉** |
  | `check` | 228 MiB，6 s | 238 MiB，6 s | 過（262／236） |
  | `check --read-data` | 473 MiB，14 s | 428 MiB，10 s | 過（432／443） |
  | `restore latest`（第 10 項修後；修前 362／789） | 148 MiB，74 s | 538 MiB，84 s | A 過（156）、**B 被 OOM 殺掉**（開始 2 秒內） |

  backup 的 M5 目標（< 512 MiB）v4 仍達到，A 集比 v3 的 373–431 MiB 低（B 集 v3 是 442–524 MiB）。
  **prune 在 100 萬個 chunk 的 repo 上超過 512 MiB**：量測跑法 604–638 MiB（A）、587–638 MiB（B），門檻跑法兩個都在十秒內被 OOM 殺掉。dhat（另一個
  target dir 編 `--features dhat`，A 集第三輪）：峰值 658 MiB——`canon: HashMap<ItemId, ObjectId>`（P6 的正本）195 MiB
  （rehash 時新舊兩張表同時在；每筆 64 bytes 的鍵值加 hashbrown 的額外空間）、`referenced: HashMap<ItemId, Kind>` 68 MiB、
  100 萬個 entry 的記錄集合三到四份各約 50 MiB（`load_map` 的分片解碼一份、`base`／`map`／`records` 的 clone）、`MapView`
  的 ID → 持有者表兩份各 40 MiB（`load_map` 與 `canonical` 各建一次）、`fetch` 的 mpack bytes 36 MiB。
  v3 的 `prune --dry-run` 在同一個 A 集是 550 MiB（ADR 011 的「沒做」），v4 的估計是 150–250 MiB（「假設」表），估錯了。
- check 的成本（同一個 repo，A 集 100 萬 chunk、16 個 pack、5 個 mpack）：`check` 6 s、228 MiB（B 集 6 s、238 MiB）；`check --read-data` 14 s、473 MiB（B 集 10 s、428 MiB），
  門檻跑法 432／443 MiB。dhat（A 集，第 10 項修後；堆積峰值 395 MiB，取樣的 cgroup 數字含配置器的額外空間）：
  `--read-data` 正在驗的那一個 pack 整份在記憶體裡兩次（`object_store` 讀回的 bytes 與複製出來的 Vec，各 67 MiB）、
  走過的 chunk ID 的 HashSet 66 MiB（「假設」表那一項）、map 分片解碼 64 MiB、`read_data` 裡記錄的一份 clone 54 MiB、
  `MapView` 的 ID 表 40 MiB、mpack 快取 32 MiB。「走過的 ID 各記 32 bytes」估得對，少算的是 map 本身與 pack 的兩份。
- M1（本機，`tests/acceptance/run.py`，100 000 檔 9.75 GiB，`m1-acceptance-v4-2026-10-18.log`）：第一次 backup
  57 s、77 個 pack 與 2 個 mpack（repo 4.90 GiB）；第二次 2.8 s、0 個新 pack、0 個新 chunk、0 個新 mpack；restore
  30 s、`diff -r` 相同；`check` 0.7 s；`check --read-data` 20.5 s；翻一個 bit 之後 `check --read-data` 以非 0 結束並
  指名那個 pack；峰值 RSS 236 MiB（v3：54 s／0.8 s／41 s／0.3 s／17 s／186 MiB）。第二次 backup 的 `--json` report：`files_reused` 100 000、`subtrees` 1 102、`items_reused` 1 102、
  `verified` 0——全部走快速路徑、每個目錄都沿用 parent 的子樹。比 v3 的 0.8 s 慢 2 秒，沒有 profile（UNVERIFIED 原因）。
- M2（MinIO，`tests/acceptance/run_s3.py`，同一份資料，`m2-acceptance-s3-v4-2026-10-18.log`）：第一次 backup 42 s、77 個 pack 與 1 個 mpack（S3 上 mpack 預設不寫 `.r1`，負責人決定 3；repo
  4.90 GiB）；第二次 2.1 s、0 個新 pack；restore 121 s、`diff -r` 相同；`check` 0.5 s；`check --read-data` 12.8 s；
  `prune`（第一輪，收錄 78 個 pack 進 map，不刪東西）1.0 s；之後 `check` 0.7 s、沒變的 backup 2.2 s 與 3.2 s；翻一個
  bit 之後 `check --read-data` 以非 0 結束並指名那個 pack；峰值 RSS 216 MiB（v3：157 s／35 s／213 s／2 s／28 s／
  242 MiB）。
- 沒變的 backup 在真的 S3（MinIO）上的請求數（`mc admin trace --json` 數，依 API 分類）：**這一項的數字錯了，2026-10-19
  更正**——計數只看 trace 的 path 裡有沒有 repo 的前綴，LIST 的 path 是 bucket 本身、前綴在 query，所以 4 個 LIST
  都沒數到；原本寫的「74 個裡的 72 個 GET 是 72 片 map 分片、path 逐一對過」也不對：每次 backup 都有 config、金鑰槽、
  head、parent snapshot 4 個 GET（10-19 的 trace 逐一看過），所以那一代是 68 片——由 72 減 4 推得，那一代的分片已經
  不在、沒有數。原本記的是 prune 之前 9 個（6 GET、2 HEAD、1 PUT）、新的一代
  第一次 74 個、之後 6 個；修正後重量的是 13／76／10（「實作第七段之二」）。都與目錄數（1 102）無關；新一代的一次性
  成本上限是 256 片加 head。
  第 4 段在記憶體後端上數的是 11（收錄進 map 之後 8）；v2 時代同一份資料在 MinIO 上 35 秒。
- V21（2000 個 root，`--files-from`，本機後端，release build，機器閒置）：2000 個 root 0.23 s（三次：0.34、0.23、0.23），同樣的檔案放在一個目錄裡 0.17 s；20 000 個 root
  1.01 s 對 0.69 s；兩種都是 1 個 pack、1 個 mpack（加 `.r1`）、1 個 delta；strace 數到 24 次 fsync。
  v3 是 10.96 s（2000 個樹物件各一個 `.r1`，8017 次 fsync）；同樣的檔案放在一個目錄裡 v3 是 0.16 s。
- GC（真實時鐘，`tests/acceptance/run_gc.py`，grace 2 h，`m3-acceptance-gc-v4-2026-10-18.log`）：第三次跑才過（binary `9acbafb`，2026-10-18 19:29–23:35；前兩次失敗都是腳本的錯，原因在第 11 項）：
  `init --grace 2h`（世代 0，沒有 drain）；backup 100 000 檔 44 s（4.90 GiB，77 個 pack、2 個 mpack）；刪掉
  dir000–dir009 再 backup 2 s；forget 舊的；prune 1 收錄 79 個（1 s）；等 7316 s；prune 2a 什麼都不做（它的「現在的 T」
  還是 prune 1 的 head 的）；prune 2b repack 8 個 pack（1 s；condemned 0——這份資料集刪掉的目錄的 pack 都還剩幾個跨目錄
  相同的小檔的 chunk，走 P7c：舊的 8 個標成 condemned、新的 8 個 by_prune 的 pack（第二次跑時量過：7 個各 68–90 KiB、
  1 個 17 MiB），repo 4.90 → 4.93 GiB）；等 7320 s；prune 3a 什麼都不做；prune 3b tombstone 並刪掉那 8 個（521 MiB，
  不到一秒；repo 4.42 GiB，少 9.8%）；prune 4 無事可做；`check --read-data` 8 s 無錯誤（87 個 pack 整份讀）；restore
  17 s、`diff -r` 完全相同；整段峰值 RSS 230 MiB。v3 的 M3（`prune --grace 0s`，2026-09-05）：標記 3.3 s、刪除 1.1 s
  （520 MiB，4.90 → 4.41 GiB）、restore 57 s、RSS 233 MiB——v4 多的只是等兩個 grace 與每個階段多一輪。

**找到的**（不是規格錯誤；規格錯誤的編號停在第 18 項）：
- 實作的 bug 兩個：fuzz 找到的 panic（第 4 項）；讀取端白記的驗證集合（第 10 項，bench 找到的）。
- 第六段留下的兩件：prune metrics 讀 v3 的欄位（第 6 項）；過期的 S3 CLI 測試（第 8 項）。
- 用法的缺口一個：新 repo 要用短的 grace，命令列上沒有地方給——init 之後調低就排空 72 小時（第 11 項，真實時鐘的
  GC 驗收找到的；`init --grace`）。
- 記憶體：prune 在 100 萬個 chunk 的 repo 上超過 512 MiB；restore 單一平面目錄的 100 萬個項目也超過（789 MiB，B 集；
  A 集的 1000 個目錄 362 MiB 在門檻內）。backup 與 check 都在門檻內。restore 的 dhat 歸因：峰值 738 MiB（B 集，修第 10 項之前）——`restore::open_placement` 的一個 336 MiB 配置（整個單一目錄的
  100 萬個 `Entry` 一次解進記憶體：目錄雖然依內容切了段，restore 仍先取整份清單）、`Reader::note_verified` 的 195 MiB
  HashSet 加 64 MiB Vec（第 10 項修掉）、map 分片解碼 64 MiB、`MapView` 的 ID 表 40 MiB、mpack 快取 32 MiB。A 集的
  1000 個目錄沒有第一項，所以 362 → 148 MiB。

**對規格的修改**：§4（init 可以指定 head 0 的 settings，第 11 項）；§6.1（切塊的規範從 v3 的 `format.md` 搬進來；原本
只有參考）；§11.3 P7c（`prune --read-data`）；
§13（損壞的物件：`check --read-data` 回報 repairable 之後跑 `prune --read-data`）；§15（fixture 的位置與測試）；
標頭（`format.md` 已刪，取回的方法）。§8 與 §8.1 只有措辭（`Backend::put` 移除後「覆寫」的說法）。

**偏離建議的**：沒有。第 7 段清單裡「ChunkIndex 的跳段讀」（第五段延後的）沒有做：它是讀取端的最佳化、不改格式，
第五段已記為「沒做」，這一段只量不改。

**留給負責人的**（2026-10-19 的決定與結果見下一節「實作第七段之二」）：
- prune 與 restore 的記憶體：100 萬個 chunk 的 repo 上 prune 要 ~640 MiB，512 MiB 的 M5 門檻只訂給 backup，但 prune 在同一台
  小機器上跑的話一樣會被殺。看 dhat 的歸因，不改格式就能省掉大半：`canon` 與 `referenced` 改用 ID 表的列號（u32）而不是
  32 bytes 的 ID 當鍵（195＋68 → 約 10 MiB），並預先配置容量避免 rehash 的雙份；記錄集合只留一份（`base`、`map`、
  `records`、`MapView` 現在各自 clone）；ID 表只建一次。估計 658 → 250–300 MiB，一天的工；要不要做、門檻要不要也
  訂給 prune 與 restore，請決定。restore 單一目錄 100 萬項的 789 MiB 見下一項的 dhat。
  restore 單一目錄 100 萬項的 538 MiB：`open_placement` 改成逐段（`Seg`）取目錄的項目、邊取邊還原，
  不先 collect 整個 `Vec<Entry>`（336 B × 項目數）；mount 的目錄查找已經是依索引逐段，restore 可以走同樣的路。半天到
  一天；一般的目錄（A 集每個 1000 項）不受影響，要不要做看會不會有這種目錄。
- `s3:if-none-match` 的 bucket policy 條件（§1 的部署建議）沒有實測：MinIO 的政策引擎支不支援這個條件 key
  UNVERIFIED；這一段只測了前綴的 PutObject 限制。
- Linux 以外的 E（§11.1 的 `CLOCK_BOOTTIME`）仍是 UNVERIFIED。
- 沒有 push（等明確的指示）。

**gate**（這份文件的 commit 之前在最終的樹上重跑，第一手）：最終的樹是 `9acbafb`（第 11 項）：fmt、clippy 乾淨；windows-gnu clippy（cargo-zigbuild，workspace 不含
kist-mount／kist-cli，與 kist-cli 單獨）乾淨；nextest 659 個通過、9 個略過（沒設整合的環境變數）；`cargo deny check`
advisories／bans／licenses／sources 都 ok。MinIO、SFTP、rclone、FUSE 全開的 nextest 在第 8 項的樹（`f24675b`）跑過：
657 個全過；之後的程式碼改動只有第 10 項（讀取端）與第 11 項（init 的選項），它們的測試不需要環境變數。這份文件的
commit 只有文件、驗收腳本與 GC 的 log。

## 實作第七段之二：負責人的決定與後續（2026-10-19）

**負責人的決定**（回第七段的「還有什麼疑問」）：「都做，如果需要改格式也可以」。逐項：prune 的記憶體——做；512 MiB 的
門檻也訂給 prune 與 restore——是；restore 單一目錄 100 萬項——做；`init` 的四個選項（第七段第 11 項，為了 GC 驗收加的）
——「都做」解讀為四個都留，沒有改動；`s3:if-none-match` 與 Linux 以外的 E——查證、能測的實測；第二次 backup 2.8 s
對 v3 的 0.8 s——profile、找到原因就修。**格式沒有改**：每一項都在 client 端，golden-v4 與凍結的 v4 fixture（第七段
第 7 項）的測試照過。

**每一步**（一步一個 commit，gate 寫在 commit 訊息裡；編號接第七段）：
- `8807849` 測試：`keep_daily_keeps_the_newest_of_each_day` 以「現在減 400 天」當起點、加幾個小時，保留政策以 UTC
  分天，UTC 15 點之後就跨日——第十二步在 UTC 21 點（本地 05 點）跑 gate 時失敗（原碼也失敗），改從 UTC 零點起算。
- `668c02c` 第十二步 prune 的記憶體：記錄只留一份（`Repo::load_map_records` 不建 ID 表、move 進 `map`）；基礎世代的
  記錄集合換成 256 片明文的 BLAKE3（`Shard::encode_borrowed` 借用記錄編碼，bytes 與 `encode` 相同）；P5 建一次位置
  索引（每個項目一列、依 (ID, §10 的順序) 排好，dead／referenced／canon 三張依 (記錄, 項目) 的 bool 表），取代以 32
  bytes 的 ID 當鍵的 `referenced` 與 `canon` 兩張 HashMap 與每個階段各 clone 一份記錄集合建的 `MapView`；不實的項目
  先標 dead、P8 之前才移除（索引的位置才一直有效）；repack 一項一項打開寫進新 pack。
- `479871b` 第十三步 restore 一段一段讀目錄：`Reader::dir_segments`／`segment_entries`，root 的落點、`restore_dir`、
  dry run 的 `plan_children` 都改成一段一段取（§7 一段最多 2 MiB 的 CBOR）。它的 commit 訊息說「某一段讀不了與原本
  整份讀不了一樣」，不對——見第十八步。
- `a5118b7` 第十四步 macOS 上 E 的單調時鐘改用 `CLOCK_MONOTONIC`。查證：安裝的 std 原始碼的平台表——Darwin 的
  `Instant` 是 `CLOCK_UPTIME_RAW`、Windows 是 QueryPerformanceCounter；Apple 的 clock_gettime(3)：`CLOCK_UPTIME_RAW`
  「does not increment while the system is asleep」、`CLOCK_MONOTONIC`「will continue to increment while the system is
  asleep」；Microsoft〈Acquiring high-resolution time stamps〉：QPC「including the time when the machine was in a sleep
  state such as standby, hibernate, or connected standby」。所以 macOS 原本的那一半在休眠時停住（牆鐘同時被往回調才
  會少算），Windows 本來就對。兩個 macOS 目標的 `cargo-zigbuild check` 過，探針（macOS 分支換成不存在的 ClockId）證明
  編到的是那個分支。
- `3131598` 第十五步 `s3:if-none-match`：AWS 的寫法（`"Null": {"s3:if-none-match": "false"}`）在 MinIO
  RELEASE.2025-09-07 上 `mc admin policy create` 回「invalid condition key 's3:if-none-match'」——MinIO 上只能靠前綴與
  沒有 Delete，覆寫同名物件擋不住。minio-setup.sh 試建 kistcond、建不起來就不 export；新測試在支援的 S3 上才跑。
- `4c06772` 第十六步 沒變的 backup：`try_reuse` 每個檔案一次 `spawn_blocking` 的來回（只為了候選不可信時要
  `block_on` 取回驗證），改成先在 async 端以 `Dedup::decide_cached` 判斷，回 `NeedFetch` 才換執行緒。
- `b48e007` 第十七步 本機來源每個條目只 lstat 一次：`SortedItems::posix()` 交出列舉時同一次 lstat 的 metadata
  （SourceItem 不加欄位：遠端來源的清單常駐記憶體）。
- `728532c` 文件：ADR、README、PLAN、`bench/memory/README.md`、`tests/acceptance`（`run_s3.py` 的 LIST 計數、10-19 的
  M1／M2 log、README）。
- `8430af6` 第十八步 第十三步的回歸（審查在文件之後抓到）：`restore_dir` 的 `segments.next(...).await?` 在走訪的迴圈裡，
  第一段之後的某一段讀不了時，在前面的子項目寫了之後 return——既有的唯讀目錄已被暫時加上擁有者寫入權，收尾沒跑，
  目錄留成可寫（原本整份 `entries()` 在寫任何子項目之前就讀完，沒有這個路徑）。改成停止讀、照常收尾、最後回錯。先紅
  後綠：唯讀的多段目錄還原兩次、第二次讓第二段的 mpack 讀兩次都失敗，原碼的 mode 是 0o755、改後回到 0o555。最上層
  （root 的落點）同樣的 `?` 不放寬權限（落點沒有 metadata），但現在是寫了一部分之後整個 restore 回錯（原本是寫任何
  東西之前）。
- 這一節的更新（第十八步、gate）：這份文件的下一個 commit。

**量測**（release build、同一台機器；binary 都是 `b48e007`，`~/kist-acceptance-m1/kist-b48e007`；之後的 `8430af6` 只改了
讀不了某一段時的錯誤路徑，沒有重量）：
- 記憶體 bench（`bench/memory/run-v4.sh` 一次跑完兩個測試集，log `~/kist-bench/run-v4-2026-10-19.log`；數字是量測跑法的
  `sampled_max_ex_file_slab`，括號是 512 MiB 門檻跑法的同一個數字）：

  | 命令 | A 集（1000 dir × 1000 檔） | B 集（單一平面 1M 檔） |
  | --- | --- | --- |
  | backup，第一次 | 264 MiB（292），54 s | 314 MiB（394），63 s |
  | backup，第二次（快速路徑） | 143 MiB（149），12 s | 285 MiB（285），20 s |
  | prune，第一輪（收錄全部 pack）／第二輪；門檻跑法是第三輪 | 233／161 MiB（159），7／6 s | 236／171 MiB（168），6／5 s |
  | `check` | 229 MiB（230），6 s | 240 MiB（271），5 s |
  | `check --read-data` | 441 MiB（440），10 s | 436 MiB（432），10 s |
  | `restore latest` | 151 MiB（156），79 s | 151 MiB（161），98 s |

  26 次全部 exit 0、沒有 OOM。

  修之前：prune A 676 MiB（`9acbafb`，10-19 修改之前在同一個 repo 上量的基準）、B 587–638 MiB（第七段，`2e6b20b`），
  門檻下都被殺；restore B 529 MiB（`9acbafb`，10-19 的基準），門檻下被殺。
- 沒變的 backup（M1 的資料集、另建的 repo、同樣的 backup 連跑，取第一次之後的——第一次較慢，1.5–2.3 s；user＋sys ≈
  wall，CPU 綁住；計數是 `strace -f -c`，第一列的計數是 `9acbafb` 的 binary 量的，它與 `3131598` 之間只有 prune、restore
  與時鐘的改動，不碰走訪；perf 在 `perf_event_paranoid=2` 下只取得到 user 的樣本）：

  | 樹 | wall | user | sys | futex | statx |
  | --- | --- | --- | --- | --- | --- |
  | 第十五步（`3131598`） | 1.77 s | 1.05 s | 0.75 s | 421 132 | 202 288 |
  | 第十六步（`4c06772`） | 1.15 s | 0.72 s | 0.41 s | 9 374 | 202 342 |
  | 第十七步（`b48e007`） | 0.92 s | 0.57 s | 0.34 s | 9 249 | 101 272 |

  report 三次都一樣（files_reused 100 000、subtrees 1 102、chunks_new 0、verified 0）。第十七步之後剩下的 user time（perf 的 user 樣本，line-tables 的 release build）：parent 樹的 CBOR 解碼約 24%、`Deltas::holders`（B2 每個 chunk 查持有者；這個 repo 沒有 prune 過，pack 還只在 delta 裡）14%、libc 的複製約 10%、argon2（開 repo 的 KDF）9%——沒有單一的大項。
- M1 重跑（`run.py`，`tests/acceptance/m1-acceptance-v4-2026-10-19.log`，與 10-18 同一份腳本與順序）：第一次 backup
  33 s（10-18 是 57 s；來源這次在 page cache 裡，不歸因於程式）、**第二次 backup 1.1 s**（10-18 是 2.8 s，v3 0.8 s）、
  restore 31 s、`check` 0.7 s、`check --read-data` 17.4 s、峰值 RSS 192 MiB。
- M2 重跑（`run_s3.py`，`tests/acceptance/m2-acceptance-s3-v4-2026-10-19.log`）：第一次 backup 30 s、第二次 1.1 s、
  restore 88 s（同一天前一次 99 s）、`check` 0.5 s、`check --read-data` 9.5 s、prune 0.7 s、峰值 RSS 188 MiB。沒變的
  backup 的請求數（含 LIST）：prune 之前 13 個（6 GET、2 HEAD、4 LIST、1 PUT）；prune 寫了新的一代之後第一次 76 個（70
  GET：66 片 map 分片第一次進本地快取，加每次都有的 config、金鑰槽、head、parent snapshot；1 HEAD、4 LIST、1 PUT；分片數
  另以 `mc ls` 數過）；快取有了之後 10 個（4 GET、1 HEAD、4 LIST、1 PUT）。4 個 LIST 是 B1 的 head 與 delta、找 parent
  的自己的 snapshot、B4 的 head。

**找到的**：
- 第七段自己的量測錯誤：`run_s3.py` 漏數 LIST（LIST 的 trace 記錄裡 path 是 bucket，前綴在 query），第七段的 9／74／6
  都少了 4 個；「74 個裡的 72 個 GET 是 72 片分片、path 逐一對過」也不對（那一代由 72 減 4 推得是 68 片）。成本一節與第七段
  的量測一節已就地更正。
- 實作的問題：prune 與 restore 的記憶體（第七段量到、這裡修）；macOS 上 E 的單調時鐘休眠時停住（查證找到）；沒變的
  backup 每個檔案一次執行緒的來回（profile 找到；v3 也是每個條目兩次 lstat、一次 llistxattr，差在這個來回）。
- 我在第十三步自己引入的回歸：讀不了目錄的某一段時唯讀目錄留成可寫（審查抓到，第十八步修）。
- 測試的問題：`keep_daily` 的測試依當下的時刻。

**剩下的**：
- `check --read-data` 是現在離 512 MiB 門檻最近的命令：這一輪量測 441／436 MiB、門檻跑法 440／432 MiB（第十三步那一輪
  量到的 B 集門檻跑法是 464 MiB）。dhat 的歸因在第七段（正在驗的 pack 在記憶體裡兩份、走過的 chunk ID 的 HashSet、
  map 分片解碼與記錄的 clone）；負責人的決定沒有包含它，沒改。
- mount 的 opendir 對單一目錄仍整份解碼（`Reader::entries`，FUSE 的 readdir 以 handle 保存整份清單才能依 offset 續讀）；
  單一目錄 100 萬項的 mount 沒有量（UNVERIFIED）。
- 每個條目一次 llistxattr（10 萬檔 101 100 次，v3 也是）：快速路徑可以沿用 parent 的 xattr，前提是「改 xattr 一定改
  ctime」——Linux 的本機檔案系統的 setxattr 會更新 ctime，macOS 沒有查證；第十七步拿掉同樣多的 statx 省了 0.23 s，
  這一項的量級應該相近（沒有量）。多一個沒驗證的前提換不到與 v3 對等以外的東西，沒做。
- macOS 與 Windows 的 E 依據是廠商文件，沒有在實機上休眠驗證（UNVERIFIED，環境：沒有那兩種機器）。
- `s3_conditional_write_policy_blocks_plain_puts` 沒有在支援這個條件的 S3 上跑過（UNVERIFIED，環境）。
- restore 對 S3 比本機慢（88 s 對 31 s），沒有 profile。
- 沒有 push（等明確的指示）。

**gate**（第一手）：程式碼的最終樹是 `8430af6`（第十八步）：fmt、clippy、windows-gnu clippy 乾淨，nextest 665 個通過、
9 個略過（沒設整合的環境變數；第十八步的測試不需要）。以下是 `b48e007` 上的整合全開與 cargo deny——`b48e007`：fmt、clippy、windows-gnu clippy（cargo-zigbuild，workspace 不含 kist-mount／kist-cli，與
kist-cli 單獨）乾淨；MinIO、SFTP、rclone、FUSE 全開（四個 setup 腳本 export 了 14 個 `KIST_TEST_` 變數）的 nextest 664 個
通過、9 個略過（`#[ignore]` 的）。env 沒設的測試會提早 return、照樣算通過，所以 SFTP、rclone、FUSE 的那幾個另外帶輸出
重跑：`sftp_v4_backup_round_trips`、`rclone_v4_backup_round_trips`、`rclone_bridge_meets_the_v4_contract`、kist-cli 的
`mount_round_trip` 與 `one_file_system_keeps_mount_points_empty`、kist-mount 的
`snapshots_written_after_mounting_are_readable` 都真的跑了（1.2–13.9 s，沒有略過的訊息）；
`s3_conditional_write_policy_blocks_plain_puts` 照設計略過。`cargo deny check` 的 advisories／bans／licenses／sources
都 ok。文件的兩個 commit（`728532c` 與這一節的更新）只有文件、`tests/acceptance/run_s3.py` 與 10-19 的 M1／M2 log。
setup 腳本的雜訊（第七段之前
就有、無害、沒改）：`rclone-setup.sh` 與 `fuse-setup.sh` 把 `rclone version`、`fusermount3 --version` 印到 stdout，
被 `eval` 當成命令跑（`unknown command "v1.75.2"`、`fusermount3: extra arguments after the mountpoint`）。

## 實作第七段之三：remote-to-remote（2026-10-19）

**負責人**：「remote to remote backup 是我們很重要的應用」——遠端來源（`kist backup sftp://…`、`s3://…`，client 當轉運）
寫進遠端的 repo，ADR 016 的動機。到這裡為止的量測與驗收都只用本機來源；第一次量遠端來源就找到 SFTP 來源把整個檔案讀進
記憶體、一個檔案一個檔案讀、沒有快速路徑。建議四項——SFTP 來源一塊一塊讀；測試與驗收加上真的遠端來源；SFTP 的讀取
少一次來回、一個檔案同時好幾個 READ；遠端來源跨檔案預讀——與一個決定：SFTP 加一個選用的快速路徑，預設關，看大小與
mtime，至少每隔一段時間整份讀一次。負責人：「都按你的建議」。**這一節修改 ADR 016 第 4 項的分級**：sftp「沒有快速路徑」
仍是預設，多一個選用的（規格 §8.1 的表加一列；ADR 016 開頭加了指向這裡的一行，原文不改）。**格式沒有改**：選用的快速
路徑是 client 的行為，Entry 沒有新欄位，「上一次整份讀」的紀錄只在本機快取裡（`full-read/<hex>`）。往遠端還原（restore
寫到 SFTP 或 S3）是當時的一個問題、不是建議，沒有做。

**每一步**（一步一個 commit，gate 在 commit 訊息裡）：
- `cf67b23` 第一步 真的遠端來源的測試（`v4_remote_sources.rs`：對 tests/sftp-setup.sh 與 minio-setup.sh 的容器
  backup、讀回、沒變的第二次；在這之前沒有一個 v4 的測試用真的 SFTP 或 S3 當來源）。
- `8148329` 第二步 SFTP 來源一塊一塊讀：來源模式的 `get` 原本以 `read_whole` 把整個檔案讀進一個 Vec。
- `f4c6589` 測試：SFTP 的目錄深度測試在同一個伺服器上只有第一次跑得過（寫固定的 key、不刪），改成每次一個目錄。
- `7a98d48` 第三步 依列表的大小讀、不先 STAT（每個檔案三次來回變兩次）。它引入兩個錯，第四步與第十一步修。
- `0deb7ba` 第四步 openssh-sftp-client 的 `File::read` 讀完把 offset 推「要的」長度、不是收到的：伺服器少送時跳過資料，
  第三步的依列表讀把它變成默默的錯位與截短。每個 READ 自己定位；`sftp/fake.rs`（只會 INIT、OPEN、READ、CLOSE 的 SFTP v3
  假伺服器，接在 `tokio::io::duplex` 上）重現。
- `3081341` 第五步 SSH 連線的 `flush_interval` 設 0：openssh-sftp-client 預設把請求留在寫入緩衝最多 0.5 ms，tokio 的
  計時器以 ms 為單位，一次一個請求時每個來回平均多等約半個 ms。
- `ac4eef0` 第六步 一個檔案同時好幾個 READ：第一個 READ 要 1 MiB、量出伺服器一次給的上限（用戶端不公開它），之後每個
  READ 要那麼多、在等的最多 2 MiB（同 russh 的 channel window；1 到 8 MiB 量到一樣快，window 調大反而慢），照順序交出，
  少送的那段補要。
- `89ea332` 第七步 設定檔的 `[backup] paths` 可以是一個遠端來源：排程的 backup 原本沒有設來源，URL 被當成本機路徑
  （「No such file or directory」）。規則搬到 `SourceSpec::for_paths`，CLI 與 kist-app 共用。
- `4546979` 第八步 選用的 SFTP 快速路徑：`--trust-mtime`、`--reread-every`（預設 7d），設定檔 `trust_mtime`、
  `reread_every`。規則：parent 的 Entry 是 sftp、大小與 mtime 相同、mtime 那一秒在 parent 開始之前就結束；這次能不能用：
  有 parent、有本機快取、快取記著這台 client 上一次整份讀這個來源（backup 的開始）、那次距今不到 `every`、parent 不比那次
  舊，紀錄在未來當成沒有。紀錄只在整份讀的那次 commit 之後寫。摘要多一行（這次信任或整份讀的原因）。
- `25a8ba1` 第九步 遠端來源在一個目錄裡預讀：正在處理的加上 16 個，要讀內容（快速路徑不能沿用）而且不大於 1 MiB 的才先
  開始讀，遇到目錄就停；本機來源一次一個，同原本。
- `2afe22d` 第十步 遠端來源的清單換成緊湊的（名稱與 etag 在一個 buffer、一個條目一個 48 bytes 的列），SFTP 直接從
  READDIR、S3 以 `PaginatedListStore` 一頁一頁列：平面 100 萬項原本在 512 MiB 的門檻下被殺。
- `7cf9e2d` 第十一步 第三步的回歸：SFTP 來源的符號連結只讀到目標的前幾個 bytes（列表上是連結本身的大小）。符號連結改成
  列的時候 STAT 一次，目標是檔案就記它的大小與 mtime，不是檔案或 STAT 不了就是一個略過的條目。
- 文件：這一節、README、規格、PLAN、`bench/memory/README.md`、`tests/acceptance/README.md`、`run_r2r.py`、
  `run-remote.sh`、M1 的 remote-to-remote log。

**量測**（release build、同一台機器、兩個容器都在本機：SFTP 是 OpenSSH 10.3p1，S3 是 MinIO RELEASE.2025-09-07T16-13-09Z；
網路的延遲幾乎是零，真實網路上的表現沒有量，UNVERIFIED；每一步與前一步的 binary 交替跑，數字是範圍；這台機器同時有別的
負載，load average 9 到 39）：

| 項目 | 之前 | 之後 | 那一步 |
| --- | --- | --- | --- |
| SFTP 來源，一個 1 GiB 的檔案，峰值 RSS | 1 225 MiB | 161 MiB | 第二步 |
| SFTP 來源，1 980 個小檔（16 MiB） | 4.86 s | 3.86 s | 第三步 |
| 同上 | 3.86 s | 1.37–1.78 s | 第五步 |
| 同上，第一次 backup | 1.41–1.62 s | 0.32–0.44 s | 第九步 |
| SFTP 來源，20 個 4 MiB 的檔案 | 1.72–2.19 s | 0.58–0.78 s | 第六步 |
| SFTP 來源，一個 1 GiB 的檔案 | 21.7 s | 5.2 s | 第六步 |
| SFTP 來源，2 000 個檔案（96 MiB），第一次 | 2.14–2.53 s | 0.78–1.04 s | 第九步 |
| S3 來源，同一組，第一次 | 1.11–1.28 s | 0.48–0.50 s | 第九步 |
| SFTP 來源，同一組，沒變的第二次 | 3.08 s（整份讀） | 0.28–0.29 s（`--trust-mtime`） | 第八步 |

對照：同樣 20 個 4 MiB 的檔案從本機路徑備份 0.28–0.31 s；OpenSSH 自己的 sftp 用戶端下載 0.43–0.46 s（約 180 MiB/s）。

平面 100 萬項（記憶體 bench 的 B 集放進兩個容器，`bench/memory/run-remote.sh`，進一個新的本機 repo；
`sampled_max_ex_file_slab`，括號是 512 MiB 門檻跑法；最後一欄是這一節最後的 binary `7cf9e2d`）：

| 來源與 backup | 第九步 | 第十一步 |
| --- | --- | --- |
| SFTP，第一次 | 499 MiB（被殺） | 398 MiB（398） |
| SFTP，第二次（全部重讀，去重） | 381 MiB | 262 MiB（276） |
| S3，第一次 | 350 MiB（被殺：取樣漏了尖峰） | 468 MiB（465） |
| S3，第二次（全部以 etag 沿用） | 430 MiB（被殺） | 324 MiB（318） |

同一組檔案從本機路徑：314 MiB（394）。S3 的時間在同樣的 backup 之間跳：第一次 2 到 7 分鐘、第二次 23 s 到 8 分鐘，由
MinIO 決定（`mc ls` 列這個前綴一次 162 s），不列；量 S3 的那幾次，容器裡還有前一份驗收腳本留下、沒停掉的 `mc admin trace`（見「找到的」）。

M1 的資料集（100 000 個檔案、9.75 GiB）放進兩個容器，各自備份進 MinIO 上的一個 S3 repo（`tests/acceptance/run_r2r.py`，
log `tests/acceptance/r2r-acceptance-2026-10-19.log`，binary `7cf9e2d`；峰值 RSS 是每 0.1 s 取樣 /proc 的 VmRSS，很短的
命令取樣不到；請求數是容器裡的 `mc admin trace`）：

| 來源 | 命令 | 時間 | 峰值 RSS | 沿用的檔案 | repo 的請求 | 來源的請求 |
| --- | --- | --- | --- | --- | --- | --- |
| SFTP | 第一次 backup | 50.5 s | 275 MiB | 0 | 91（84 PUT） | — |
| SFTP | 沒變的第二次（整份重讀、去重） | 45.8 s | 100 MiB | 0 | 13 | — |
| SFTP | 再一次，`--trust-mtime` | 2.9 s | 76 MiB | 100 000 | 13 | — |
| SFTP | restore 到本機（`diff -r` 相同） | 86.5 s | 65 MiB | | | |
| S3 | 第一次 backup | 42.7 s | 231 MiB | 0 | 91 | 100 000 GET、1 102 LIST |
| S3 | 沒變的第二次（etag 與大小） | 17.7 s | 77 MiB | 100 000 | 13 | 1 102 LIST |
| S3 | restore 到本機（`diff -r` 相同） | 79.5 s | 61 MiB | | | |

兩個 repo 的 `check` 都過。同一份資料從本機路徑備份進本機 repo（README 的 M1）：第一次 33 s、沒變的 1.1 s。M1 有 1 101 個
目錄、每個不到 1 000 項：1 102 個 LIST 是 root 列了兩次（見「找到的」）。

**找到的**：
- 第三步自己引入的兩個錯，都只在還沒 push 的這幾個 commit 裡：伺服器少送時跳過資料（`File::read` 的 offset，第四步修）、
  符號連結只讀到目標的前幾個 bytes（第十一步修）。第二個是看 B 集在 SFTP 為什麼多一個檔案找到的。
- 排程的 backup 不能用遠端來源（第七步修）。
- openssh-sftp-client 的預設 `flush_interval` 是每個來回半個 ms（第五步修）。
- 遠端來源的清單：一項一個 88 bytes 的 SourceItem 加名稱與 etag 的小配置，整份留到目錄走完；S3 另外整份 `ListResult`
  再轉一次（第十步修）。dhat：第十一步之後 S3 的第一次在峰值時活著的是 324 MiB（資料 pack 的兩個 buffer 119 MiB、pack 的
  索引 35 MiB、去重的集合 33 MiB、沿用的清單 33 MiB、清單本身 67 MiB），量到的 465–497 MiB 比它多約 150 MiB，是 glibc
  的分配器留著的；SFTP 也一樣（第九步：活著 329 MiB、量到 499 MiB）。
- 遠端 root 的目錄列兩次：`plan_roots` 為了分辨 root 是檔案還是目錄，把整個 root 列一次、只看第一項，walk 再列一次
  （S3 來源 21 個目錄的 trace 是 22 個 LIST）。100 萬項的平面 root 就是兩次完整的列舉；沒有改。
- 驗收腳本的 trace：殺掉本機的 `docker exec` 不會停容器裡的 `mc admin trace`（沒有 tty，訊號不轉過去），留下來的訂閱者
  一個佔 MinIO 約 12% 的 CPU，累積之後新的 trace 收不到東西。`run_r2r.py` 改成在容器裡記下 PID、結束時 kill；
  `run_s3.py` 有同樣的寫法，沒有改。
- SFTP 來源把符號連結跟過去讀、不記成符號連結（ADR 016 的已知限制）；連到目錄的與斷掉的是略過的條目。
- `to_meta` 在伺服器沒給大小時當成 0（`unwrap_or(0)`）：那樣的伺服器的檔案會記成空檔。第三步之前 stat 走同一個轉換，
  不是新的；OpenSSH 都會給大小，沒有改。

**剩下的**：
- 真實網路（延遲、頻寬、russh 的 window）上的表現沒有量，UNVERIFIED；這一輪的容器都在本機。
- 沒有 limits@openssh.com 的伺服器：用戶端一次要 20 480 bytes，2 MiB 的上限是約 100 個 READ 在等——只在假伺服器上跑過，
  真實的這種伺服器沒有試，UNVERIFIED。會少送的真實伺服器或檔案系統也沒有找到。
- 分配器多留的約 150 MiB（上面）：沒有換分配器、沒有調 glibc 的參數。一個探針（SFTP 的平面 100 萬項、第一次 backup、`7cf9e2d`，交替各兩次）：預設 337、402 MiB，`MALLOC_ARENA_MAX=2` 326、340 MiB——最高的與變異都小一些，但次數太少，沒有據此改什麼。S3 的平面 100 萬項第一次離門檻只剩 30 到 47 MiB（第十步的 binary 482 MiB、最後的 465 MiB）。
- 遠端 root 列兩次（上面）。
- rclone 的 stdio 連線沒有改 `flush_interval`（沒量）。
- `--trust-mtime` 的 racy 判斷比的是伺服器的 mtime 與這台的時鐘：伺服器慢多少，判斷就弱多少；文件與 help 都寫了。
- 往遠端還原沒有做（上面）。
- 沒有 push（等明確的指示）。

**gate**：每一步的 commit 訊息裡（fmt、clippy、windows-gnu clippy 的 workspace 與 kist-cli、MinIO 與 SFTP 的環境變數開著的
nextest）。程式碼的最終樹 `7cf9e2d`：fmt、clippy、windows-gnu clippy 乾淨，nextest 689 個通過、9 個略過。這一節的
commit 只有文件與驗收的腳本、log。
