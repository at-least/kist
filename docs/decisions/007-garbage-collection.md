# 007 — 垃圾回收：標記、grace、登記與復活

- 狀態：已接受（M3）
- 日期：2026-09-04
- 相關：`internal/repo/gc.go`、`internal/repo/forget.go`、`internal/repo/backup.go`、[docs/format.md §12](../format.md)

## 問題

沒有鎖。任何時刻都可能有 client 在備份，而備份的去重靠的是一份**開始時載入**的 index：它看到 chunk X 在 pack P 裡，就不傳 X，最後寫一個引用 X 的 snapshot。prune 要刪的正是「沒有 snapshot 引用」的 pack。兩件事之間的空窗——X 已被跳過、snapshot 還沒落地——是所有危險的來源。`PLAN.md` 的驗收條件只有一句：刻意競態下永遠不會刪到活的 chunk。

## 決策

### 1. `forget` 只刪 snapshot 物件，不需要 grace

snapshot 是葉子：沒有東西引用它，刪掉不會讓任何東西懸空。retention 規則（`--keep-last/hourly/daily/weekly/monthly/yearly/within`）按 UTC 分桶、每個 client 各算各的——一台很少備份的機器不該因為另一台很常備份而被清光。沒給規則也沒指名 → `ErrNothingToForget`：「全部忘掉」必須打字，不能是預設。

它引用過的 tree 與 index blob **留著**。tree 沒有時間戳也沒有標記，套不上 grace；它們很小（100k 檔 ≈ 1012 個）。留給之後的 `gc/tree-<id>` 或世代機制，M3 不做。

### 2. 活性以「解析」定義，不是「持有」

`index.AddPack` 原本是先到先贏，贏家取決於 blob 順序或 pack 順序，`LoadAll` 與 `Rebuild` 可能對同一個 chunk 給出不同的 pack。對 prune 這是致命的：兩輪之間一次 `rebuild-index` 就能把活的和死的對調。改成 **packID 最小者贏**，對映變成 pack 集合的純函數。順帶解決了並發備份產生的重複 pack：兩個 pack 持有同一批 chunk，一個活、一個死，死的那個被正常回收。`TestDuplicateChunkResolvesToTheSmallestPackID` 正反序各餵一次，要求每個 chunk 的答案相同。

### 3. 兩階段，中間隔一個 grace

第一輪標記（`gc/<packID>`，`PutIfAbsent`，**從不刷新**——刷新等於 grace 永遠到不了），下一輪等 grace 過了才刪。grace 預設 72h：一個週末的維護工作停擺不該變成競態。

### 4. 清掃的第四個條件：client 條件

只靠 grace 擋不住跑超過 72h 的備份（一台新機器的第一次全量備份正是最可能的那個）。所以刪一個 pack 還要：**每個還在等的 client，最後活動都晚於 標記時間 + clock-skew**。

論證：一個在標記**之前**開始的備份，跳過了 X，snapshot 還沒落地。它的 client 的最新 snapshot（或登記時間）一定早於標記——同一 client 同一時間只跑一個備份——所以 hold。一個在標記**之後**開始的備份，開頭就列了 `gc/`，會看到標記（見 6）。

「還在等的 client」= 最後活動距今不超過 `--forget-clients-after`（預設 10 × grace）。一個 30 天沒動靜的 client 被假設沒有備份在跑；真有的話那次備份已經跑了 30 天。這是文件化的限制，不是漏洞。

### 5. `clients/<clientID>` 登記：讓 prune 知道要等誰

advisor 原本的定義是「有 snapshot 的 client」。那漏掉了一種：**第一次備份還沒完成的新 client**。它沒有 snapshot，prune 不知道它存在，grace 一過就把它正在依賴的 pack 刪掉。備份開頭先 `PutIfAbsent clients/<id>`（`{first_seen}`），不更新——它是下界，之後的活動由 snapshot 說。最後活動 = max(登記, 最新 snapshot)。

backup 憑證能寫 `clients/*`，所以那裡會有垃圾。垃圾**回報、不中止、不刪**：一個誠實 client 損壞的登記是它第一次備份期間唯一的保護。代價是持有 backup 憑證的人可以塞假登記把 prune 拖住 `--forget-clients-after`——延遲，不是破壞，在威脅模型之內。

### 6. 復活 = 重傳 + 刪標記，不是只刪標記

備份看到 chunk X 在被標記的 pack 裡，把 X **當成不存在**重傳一份，並刪掉標記（一個 pack 一次）。只刪標記有一個關不上的窗：prune 讀到標記還在 → client 刪標記 → prune 刪 pack。兩個無條件操作沒有任何順序能關掉它。重傳之後不管競態怎麼走，總有一個持有 X 的 pack 活著。重傳的成本可以忽略——那是本來就要被刪的資料。

`TestPruneRaceRevivalDuringSweep`（情境 E）抓到一個真 bug：`revive` 把 pack 從「被標記」集合裡拿掉，於是同一個 pack 的**第二個** chunk 被跳過。修法：被標記的集合整輪不動，只用另一個集合記「標記已刪過」。沒有這個測試就沒有這個 bug。

提交 snapshot 前再列一次 `gc/`：這次備份參照到的 pack 若在中途被標記，刪標記。此時它不可能已經被刪——標記比這次備份年輕，而這個 client（在標記之前就登記了）hold 著清掃。拿不到 `gc/*` Delete 權限時只警告不失敗：`TestPruneRaceMarkDuringBackupWithoutUnmarkPermission` 證明 prune 自己的重算是最後一道防線。

### 7. 標記比 pack 活得久；index 重寫在標記刪除之前

清掃刪 pack、**留標記**，下一輪才刪「pack 已不存在」的標記。否則一個 client 可能在「pack 沒了、標記也沒了」的那一瞬間列 `gc/`，然後繼續相信一份還指著 P 的 index。

advisor 找到一個 crash 洞：清掃刪了 P、還沒重寫 index 就死了。下一輪原本會直接刪孤兒標記，而 index blob 還指著 P——client 載入 blob、找不到標記、跳過上傳。修法：這輪開頭重新載入 index，若它指到任何不存在的 pack 就重寫，**然後**才刪孤兒標記。順帶把「管理員手動弄丟一個 pack」也修好了。`TestPruneRewritesTheIndexAfterACrashedSweep` 先紅後綠。

client 端配合：**先列 `gc/`，再重載 index**。prune 是先重寫 index 再刪標記，所以一個錯過標記的 client 一定是在刪除之後才列的，它之後才重載的 index 不可能指著那個 pack。

### 8. 所有列舉在一輪開頭、順序固定

`gc/` → `indexes/` → `clients/` → `snapshots/` → trailer → 走 snapshot。之後所有決定都對著**同一組** snapshot 做。在列舉之後才落地的 snapshot，要嘛來自標記之後開始的備份（看過標記，安全），要嘛它的 client 在列舉裡的最後活動早於標記（hold）。這依賴後端的 read-after-write 一致性，M4 的 SFTP 後端也必須提供。

### 9. 時鐘偏差：一個容許值，不是一個假設

client 條件拿 client 時鐘蓋的章跟 pruner 時鐘蓋的章比。一台快 δ 的 client，備份在標記前 δ 之內開始、跑過 grace，看起來像「標記之後才活動」。`--clock-skew`（預設 1h）：活動必須晚於 標記 + skew 才算。默默失效的假設換成一個有預設值的旋鈕。

### 10. Object Lock：回報，不對抗

實測（MinIO，`mc mb --with-lock`）：版本控制 bucket 上不帶 version 的 `DeleteObject` 成功（delete marker），之後 Get 404、List 不列出，位元組留著。kist **不刪版本**——那是 bucket 擁有者 lifecycle policy 的事。`backend.Delete` 事後用 `ListObjectVersions` 看一眼，有版本留著就回 `ErrLocked`。`prune` 對它：計入 `locked`、不計回收位元組，其餘跟刪掉的 pack 一樣——index 停止指向它（它已經讀不到了），標記留到下一輪。維護用的刪除（舊 index blob、標記、垃圾）把 `ErrLocked` 當成功。

### 11. 不健康的 repo 不能 prune

任何 snapshot、tree、chunk 解析不到，或任何一個 pack 的 trailer 讀不出來（**包括沒人引用的 pack**），一律中止：「跑 `check`」。在壞掉的 repo 上 prune 是壞掉變成消失的方式。限制：一個沒人引用但 trailer 損壞的 pack 會擋住 prune，得手動移除。

## 測試

五個刻意競態情境用兩個 hook（`backupHooks.afterMarks`、`pruneHooks.beforeDelete`）在同一個 process 裡把 prune 插進 backup 的特定點，時鐘由測試控制，全部在 `-race` 下跑，每個情境結束都 `check --read-data` + 逐位元組還原：

| | 情境 | 結果 |
| --- | --- | --- |
| A | 備份在標記前開始、snapshot 在標記後落地 | pack 保留；提交時的復活把標記刪了 |
| B | grace 到期時備份還在跑 | hold（client 條件）；落地後 pack 活 |
| C | 標記後才開始的備份 | 重傳 + 刪標記；重複的那份下一輪回收 |
| D | 兩個 client 並發產生重複 pack、忘掉一個 snapshot | 只回收非正典的那份；再備份 0 新 chunk。MinIO 上也跑 |
| E | 標記後的備份剛好落在「決定刪」和「刪」之間 | 重傳保住資料；抓到 revive-once bug |

另加：拿不到 unmark 權限、sweep crash 後重寫 index、垃圾登記、時鐘偏差、Object Lock。

## 沒做的

- 忘掉的 snapshot 留下的 tree 與 index blob 不回收。
- `prune` 不能只跑一個階段；兩階段永遠在同一次呼叫裡，第一次標記、grace 後的下一次刪。
- AWS S3 上的 Object Lock 與 `s3:if-none-match`：**UNVERIFIED**。
