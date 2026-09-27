# 017 — 上線前的重設計檢討：v4 只做「樹進 pack」一項格式改動

日期：2026-09-26。狀態：**草稿，待負責人裁定**（尚未實作、格式仍是 v3）。

## 背景

問題：kist 還沒上線、沒有真實資料，如果可以重新設計格式與整個架構，該怎麼做？

做法：六個獨立提案各自出方案（其中一個是「維持 v3、只付已知的債」的對照組；
其餘五個分別從 GC 生命週期、後端合約、物件模型與讀取路徑、營運與雙實作、
金鑰與 config 切入），每案各由三個對抗審查者攻擊（GC 競態、抗勒索與還原可靠、
證據對口味），外部事實由查證 agent 逐條對上游文件與原始碼。六案全部被降級，
包括對照組，所以本文每條決定都附條件。凡講「程式碼目前怎麼做」都是讀原始碼
所得、沒有執行過（file:line 引用都經親自核對）；審查者轉述而未親自核對的會註明。

評判準則沿用 PLAN 的目標順序：資料安全 > 還原可靠 > 抗勒索 > 效能 > 功能數量。

## 結論

不整個重來。v3 的密碼學核心、CBOR 規則、FastCDC、pack 排版、snapshot 當唯一
commit point、pack 的兩階段 GC 都沒有 bug 史，且有向量或 proptest 背書，重做只是
歸零重驗。唯一值得動格式的是**把目錄樹從獨立物件搬進 pack**（v4）。其餘建議
都是不改格式就能付的債，可以在 v4 之前先做。

理由：六件資料遺失方向的 bug 裡有四件住在「樹的存活靠後端 mtime 表達」這一塊
——touch 用 PutIfAbsent 的競態（ADR 016 §6）、SFTP 回傳零 mtime 讓樹的復活判斷
翻成死（PLAN 2026-09-19）、同秒比較方向（c30fd81）、樹的 HEAD→DELETE 視窗
（ADR 005 §6a）。其中一件在設計審查就攔下、兩件出在 Go 端，但它們全在同一個
機制裡。這個機制存在的唯一原因是 write-once 的樹物件沒辦法「再放一份」來表示
自己還活著，只好用旁路訊號（touch/ 覆寫、mtime 比較）。樹進 pack 之後，樹跟
chunk 走同一套已有 200 案例 proptest 的 pack 生命週期；touch/、樹的 gc 標記、
樹的 .r1、commit gate 逐樹 HEAD 全部消失。

## 保留與改動的分界

| 部分 | 決定 | 理由 |
| --- | --- | --- |
| 金鑰階層、AAD、隨機 nonce、CBOR 十條規則、FastCDC、pack 排版與 trailer | 保留，逐 byte 不動 | 有跨語言向量與長時 fuzz，沒有 bug 史 |
| Tree/Entry 明文結構、mk 聯集與存在矩陣、roots、stats 口徑 | 保留 | `tree_id` 與 `chunk_id` 是同一個 keyed BLAKE3（kist-crypto/src/lib.rs:356-362），tree-canonical 向量原樣存活；Windows 路徑 bug 是 root 切分問題，不是聯集的問題 |
| snapshot 唯一 commit point、client 命名空間、conditional put、snapshot .r1 | 保留 | 整個安全論證掛在它上面 |
| pack 兩階段 GC：無資訊 gc/ 標記、後端 mtime 當年齡、grace、活躍 client 規則、young-object 規則、刪前 HEAD | 保留 | 專案裡唯一有行為證據的部分（gc_race、兩位 reviewer 的探針）；此區 bug 史只有一次無害的精度不一致（ADR 005 §11） |
| index blob、supersedes、>64 合併、本地快取 | 保留 | 幽靈 pack 已靠「只在存在的 pack 間選正本」修掉；替代方案輸在 phantom 順序論證 |
| PutIfAbsent 作為後端原語 | 保留 | S3、GCS、MinIO、OpenSSH 都有原子建立；拿掉是為了遷就 rclone，反轉 PLAN 的優先序 |
| config 單一物件 | 保留，補 keys/ 命令 | 規格已有 `keys/<slot>`，只缺命令；拆成獨立 slot 物件與 settings 世代物件被審查者打回（見「拒絕的方案」） |
| **trees/、touch/、trees/*.r1、樹的 gc 標記、樹的 self-heal 覆寫、commit gate 逐樹 HEAD** | **移除，樹進 metadata pack** | P1 四件、三個生命週期、樹的寫放大、backup 角色需要覆寫 |

## 決定 1：目錄樹搬進 pack（v4 唯一的格式改動）

### 機制

- 新命名空間 `mpacks/<hex>`：排版、trailer、index、gc 標記、parity 都與 `packs/`
  完全相同，只是分開放。分開的三個好處：index 全丟時只要掃幾顆小物件就能列
  目錄；本機後端可以只給 metadata pack 寫 .r1；metadata pack 可以維持小顆。
  `IndexPack` 多一個種類欄位（0 資料、1 metadata），與命名空間不符讀端拒絕；
  不另設 AAD 常數。
- tree segment 的封裝與 chunk 一模一樣：`alg byte ‖ 規範 CBOR`，用 chunk key 封，
  AAD 是 tree ID，允許 zstd。ID 仍是明文的 keyed BLAKE3，壓縮前算（不重蹈
  ADR 002 §3）。`Snapshot.roots[].tree` 與 `Entry.tree` 的值完全不變。
- 格式內已有先例：超過 `MAX_INLINE_CHUNKS` 的 `ChunkList` 本來就是當資料放進
  pack 的（format.md §8）。
- 重用規則只剩一句：**沿用的樹＝index 查得到、且持有者 pack 沒被標記的樹；
  持有者被標記，就把該樹重新寫進新的 mpack，跟 chunk 一樣。** 這句話取代整個
  touch/ 機制。
- 讀取端不過濾被標記的 pack，只有去重才排除；已 commit 的 snapshot 可以合法地
  只剩一個被標記的持有者。NotFound **或 Corrupt** 都重載 index 再試（樹現在會因
  repack 搬家）。
- 重用時的驗證：這一輪沒有經 parent 走訪讀過的重用樹段，讀出來驗（解密、重算
  ID、解析），驗不過就當 miss 重寫。這是 v3「AlreadyExists → read_tree_once →
  覆寫 heal」（backup.rs:1296-1312）的無覆寫版本。
- backup 角色從此沒有任何覆寫 Put。

### GC 時間線（V3-GC-5 那個當初差點刪活資料的案例）

1. D0：backup#1 把樹 T 寫進 mpack M1，commit S1；之後 forget S1。
2. D1：prune 標記 M1（標記時間＝後端 mtime）。
3. D5：backup#2 開始，先列 gc/ 看到 M1 被標記；目錄沒變所以還是 T，查 index 時
   排除 M1 → miss → 把 T 重寫進新的 mpack M2；commit gate 確認 T 解析到 M2
   （存在、未標記）；commit S2。
4. 之後的 prune：T 的正本是 M2，M1 不活、標記夠老、client 的 S2 在標記之後 →
   刪 M1。

全程沒有比較任何樹物件的 mtime、沒有 touch、沒有同秒規則。「先重用再標記」
與 repack 搬家的案例跟 chunk 完全相同，gc_race 已涵蓋。

### 抗勒索收益

查證確認 AWS S3 自 2024-11 起可用 bucket policy 的 `s3:if-none-match` 條件鍵，
強制所有 PutObject 帶 `If-None-Match`，覆寫在伺服器端被拒。完整寫法：backup
主體只在 `packs/`、`mpacks/`、`parity/`、`indexes/`、`snapshots/` 前綴下有建立
權限，對 `config`、`keys/`、`gc/` 明確 deny。v3 要用它得替 touch/ 與樹 heal 開
前綴例外；v4 不需要任何例外。

要吃到這個收益還要修一個規格與程式不符：pack 上傳（backup.rs:1740）與 index
blob（repo.rs:467）目前是普通 `put`，不是 format.md §1 寫的 PutIfAbsent；Go 端
反而早就是。

### 必須寫進規格的條件（審查者附加）

1. tree segment 要有 **byte 上限**，作為格式常數而不是建議值。chunk payload 以
   `chunker.max` 封頂，解壓上限與 pack 緩衝都靠它；tree segment 現在只限
   `MAX_NODES_PER_TREE` 個節點，xattr 沒有上限。讀端對 mpack 內項目的解壓上限
   取兩者較大者。
2. 本機單碟的冗餘要明定：`replicas=1` 時 mpacks 寫 `.r1`，沿用 snapshot .r1 的
   成組規則。**這推翻 format.md §7 與 ADR 016 §7「pack 不做副本，parity 已涵蓋」
   的決定**，理由：parity 是選配、修不了整顆物件遺失（16 個資料 shard 都在 pack
   本體裡），而且 repack 不寫 parity sidecar（prune.rs 只刪不產）——這是 v3 既有
   的洞，一併補。
3. 保留 index blob。「trailer 就是 index」的提案被指出丟掉了 chunk map 的第二份
   副本（rebuild-index 的依據）。
4. 保留 young-object 規則。它是 commit gate「自己的 pack 免驗」（backup.rs:592
   `own_packs` 的例外）的依據，不是效能規則。
5. prune 的持有者集合要規範化：Rust 讀 index blob（prune.rs `load_index_blobs`），
   規格沒有寫死。已移除的 Go 實作是列 `packs/` 並逐顆讀 trailer，兩者曾用不同的
   真相源；把 Rust 的做法寫進規格並掛向量。
6. 讀端「同一 ID 保留全部持有者、依正本順序逐一嘗試」是本地快取紀錄格式的
   改動（ADR 011 的固定長度紀錄），要列進成本並重跑記憶體 bench。
7. 失去的保護要寫明：v3 每次備份都會讀回並驗證沿用的樹；v4 與 chunk 相同，靠
   `check --read-data` 與上面的重用驗證。

### 成本（要重做的驗證）

- pack 與 cbor 兩個 fuzz target 重新播種重跑；chunker、parity 不動。
- golden 多數只差版本 byte；index 的 golden 因種類欄位要重錄，index 程式要動。
- gc_race harness 把樹納入持有者模型後重跑 200 案；順便補它現在的缺口：第二個
  pending plan 會被略過（gc_race.rs:241）、沒有「execute 中途中斷」的操作，重疊
  prune 只有「plan → 完整 prune → execute」這一種形狀被跑過。
- 樹讀寫路徑（backup/restore/check/prune/mount）改寫。v3 的凍結 fixture repo
  改當反向向量（v4 讀取端必須拒絕），另凍結一份 v4 的（見 ADR 018）。
- 記憶體 bench（ADR 011 A/B、prune）與 MinIO、SFTP 整合重跑。
- 不動：chunker 邊界向量、parity golden、金鑰向量、CBOR 規則。若保留 `kist/v3/*`
  派生字串，金鑰向量連 hex 都不用重錄。

順帶發現：format.md §18 的向量登記表大多**尚未生成**——程式碼只引用 V3-GC-5、
V3-KEYS-1、V3-TREE-1；跨語言實際存在的是 tree-canonical.hex、chunker 邊界、
金鑰向量與 Go 的 golden。這代表「重來＝歸零重驗」高估了成本，也代表目前的
跨語言保證比規格寫的薄。不論改不改，向量都該做成真實的資料檔。

### 殘餘風險

- 單一 mpack 遺失的爆炸半徑比單一 trees/ 物件大（一顆裝很多目錄段）。
- 目錄列舉現在也依賴 index；退路是掃 mpacks/ 的 trailer 與 rebuild-index，要進
  規格並測。
- 每次有變更的 backup 至少多寫一顆小 mpack，需要合併政策。
- 所有速度數字 UNVERIFIED：README 的 S3 數字是 v1 時代（commit 09f6f82）量的，
  v3 之後沒重量過。

## 決定 2：GC 規則補釘（同一次 v4，不新增任何物件）

- 後端 mtime 仍是時鐘，但只剩 pack 的四處：標記年齡對 grace、young-object 對
  grace、活躍 client 對 inactive_after（三處小時級以上）；刪前 HEAD「重寫過就
  復活」（prune.rs:650）保留，那一處仍是秒級，截秒規則對它維持規範。
- 活躍 client 規則在標記時間等於最新 snapshot 時間時是「擋住」（prune.rs:439
  `mark.modified >= m`），刪除要**嚴格大於**；這個方向要釘成向量，同秒方向正是
  P1 的一類。
- prune 主機時鐘超前後端會讓每個標記看起來更老（資料遺失方向），目前沒有東西
  會抓。補自檢：寫下第一個標記後 HEAD 它，時間差超過 clock_skew 就中止整輪。
- 一次只能一個 prune 變硬規則（維護主機檔案鎖）；刪 pack 前再讀一次它的 gc
  標記，不在了代表別的 prune 已復活它 → 跳過。
- prune 順序規範化：先讀完全部 index blob，再列 packs/ 與 mpacks/；反過來會把
  剛上傳、剛索引的 pack 當 phantom 丟掉。
- 刪除順序改成 .r1 先、parity、主體、標記；crash 才不會製造永久的假孤兒警報。
- 壞掉的 index blob 目前讓所有 client 停擺且 rebuild-index 清不掉（只寫新 blob
  不刪舊的）：backup 與 restore 改寬鬆跳過並記錄，prune 維持嚴格拒絕，補一個
  維護命令刪掉解不開的 blob。

## 決定 3：不改格式就能先做的債

- restore 每個 chunk 一次 range GET（restore.rs:477）：同 pack 內相鄰 chunk 合併
  成一次讀，解密後仍按檔案順序組裝（不要用 raw_len 算 pwrite 位移）。
- `key add` / `key list` / `key remove`，對既有的 `keys/<slot>`。remove 必須先用
  其他槽解鎖，刪完再列一次，空了就用記憶體裡的 master 重寫一槽並大聲警告；
  `key rotate` = 加新槽、驗證、**刪掉其他全部**（只刪舊槽會留下攻擊者用 backup
  主機密碼加的合法槽）。開 repo 時 config 的槽開不了才列 keys/，要限制掃描順序
  與 KDF 參數上限，搭配對 keys/ 的 deny，避免只有建立權限的攻擊者塞爆。
- `read_snapshot` 只在 NotFound 退到 .r1（repo.rs:613），`read_tree` 在 Corrupt
  也會退（repo.rs:346-347）：對齊成後者。
- pack 與 index blob 改 PutIfAbsent；format.md §15 列出真實的覆寫點（v4：backup
  角色零；維護：config、`check --repair` 同名重寫）。
- ADR 014 表格的 O_EXCL 列要改：查證者實測 rclone v1.75.0 是**靜默忽略**
  O_EXCL，不是回 OpUnsupported；`rclone://` 維持明確降級模式不變。
- canonical rank 函數單一來源（**已做**，ADR 019 C1，與這段改寫同一個 commit）：
  規格 §10「未標記優先，其次名稱最小」在 Rust 內部手寫了兩份（add_pack_ranked
  與 prune 的正本選擇），改成共用 index.rs 的 `canonical_rank`。而且已實測誤拒：
  commit gate 用不看標記的 `load_index` 重新解析，會解析到被標記、已過 grace 的
  舊 pack 而拒絕 commit（ADR 019 的真 prune repack 探針：修正前 11/32 回誤拒，
  離線 client 擋住下一輪 prune 時重跑仍失敗；gate 改用有 rank 的
  `load_index_for_backup` 之後 0/32），回歸測試是 commit_gate_rank.rs。這條與
  格式無關，不必等 v4；決定 1 的 GC 時間線第 3 步「commit gate 確認 T 解析到 M2」
  也以它為前提。原本附帶的「加向量」不在 C1 範圍，未做；快取「新的贏」的合併
  規則要不要改另案。

## Go 參考實作（已決定：2026-09-27 移除）

git 歷史稽核與決定見 ADR 018。對本 ADR 的影響：v4 只需改一份實作；格式
相容性由凍結的 fixture repo 把關，v3 那份在升 v4 時改當反向向量。

## 拒絕的方案

| 誘惑 | 為什麼不 |
| --- | --- |
| 用自己寫進標記的時間戳、MAC、sealed plan、epoch 取代後端 mtime | backup 主機持有 master key（要封 chunk），被入侵後可偽造任何有效標記；v3 免疫的原因正是後端蓋時間（ADR 005 §1）。每一版初稿都被找到 fatal 級時間線（取樣時間與寫入時間相差數小時吃掉安全邊際、同一個 now 兼任兩種用途、epoch 在重疊 prune 下失效）。樹進 pack 後 mtime 只剩四處，換整套不成比例 |
| 拿掉 PutIfAbsent、改無條件 put、snapshot 加隨機尾碼、樹加 salt | 放棄 S3 上最強的抗勒索硬化（bucket policy 強制 create-only），只為 rclone；salt 規則照字面會讓每次 backup 重傳全部樹 |
| config 拆成獨立 `keys/<random>` 物件 ＋ `settings/<gen>` 世代物件 | config 今天本來就只寫一次（repo.rs:217 `put_if_absent_verified`）；只有建立權限的攻擊者可以塞爆槽數或用極大 KDF 參數讓 restore 機器開不了；刪最後一槽＝全部資料永久不可讀；settings 帶 grace 是可偽造的資料遺失槓桿 |
| 拿掉 index blob（trailer 即 index）或拿掉 supersedes | 丟掉 chunk map 第二份；phantom 順序論證失效；快取重建規則被弱化；prune 讀本地快取變成未驗證的安全輸入 |
| 整份 snapshot 單一 manifest 取代目錄 Merkle 樹；sorted packs；bloom filter | 失去子樹零 I/O 重用與 ADR 011 的串流；sorted packs 毀掉 restore 的檔案順序局部性 |
| Entry 改成 core ＋ 不透明 per-source blob | 矩陣沒有 bug 史，純口味；會作廢 tree-canonical 向量 |
| 砍 Web UI、mount、rclone；停 windows-gnu | 產品範圍，沒有 bug 或 P-id 支持 |
| lease / heartbeat 物件 | crash 的 backup 永久擋住標記：要嘛靠時鐘過期、要嘛靠覆寫心跳，等於 touch/ 再來一次 |
| master key 輪替（重加密全部資料） | 密碼輪替只換 KEK wrap；master 外洩＝新 repo，寫成接受的限制 |

## 依賴的外部事實（查證 agent 對到上游文件或原始碼並引文）

| 事實 | 狀態 |
| --- | --- |
| S3 PutObject 支援 `If-None-Match: *`（2024-08-20）、`If-Match`（2024-11-25） | 已查核 |
| S3 bucket policy 可用 `s3:if-none-match` / `s3:if-match` 條件鍵強制條件寫入（2024-11-25） | 已查核；推翻三個提案「IAM 擋不住覆寫」的前提 |
| S3 條件 DELETE（`If-Match`，2025-09-16）；object_store 0.14.x 未暴露（trait 的 `delete` 無選項） | 已查核；樹進 pack 後不需要 |
| S3 的 Last-Modified 由伺服器設定、client 改不了；SFTP 的 SETSTAT 可設 mtime | 已查核；SFTP 靠 gc/ 目錄擁有權緩解是部署假設 |
| S3（2020-12 起）與 MinIO 的 LIST 讀後寫一致 | 已查核；MinIO 只在 xfs/zfs/btrfs 上保證 |
| rclone serve sftp v1.75.0：hardlink 回 OpUnsupported，O_EXCL 被靜默忽略 | 已查核（實測） |
| Kopia 目錄列表存在 `q` 前綴 metadata pack；restic 的 tree blob 與資料同 pack 格式；Borg 的 item stream 走同一切塊 | 已查核；樹進 pack 有先例 |
| Kopia 的 blob GC 以儲存端時間戳為準並要求兩輪；Duplicacy 靠 rename 兩階段；restic prune 要獨佔鎖 | 已查核 |
| kist 的 pack 上傳是單一 PUT 而非 multipart（影響 bucket policy 是否要 multipart 豁免） | 從原始碼看走 object_store 的 `put`（kist-backend lib.rs:281-283）；UNVERIFIED 未實測 |

## 待負責人決定

1. 是否採納 v4「樹進 pack」；若採納，先做決定 3 的債，再開 v4。
2. ~~Go 的定位~~：已決定，2026-09-27 移除（ADR 018）。
3. 派生字串留 `kist/v3/*`（省重錄金鑰向量）或全面改 `v4`（一致但要重錄四行 hex）。
