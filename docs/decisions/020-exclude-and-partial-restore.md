# ADR 020：排除規則與部分還原

日期：2026-10-10　狀態：已採用

## 背景

2026-10-09 與 restic 0.19.1 做功能比對，日常使用最先撞到的兩個缺口：backup 沒有任何排除機制
（只能列路徑），restore 只能整份還原（單一檔案要走 `kist mount` 再拷貝）。負責人指示補上這兩項。

範圍：

- backup：`--exclude PATTERN`、`--exclude-file FILE`、設定檔 `[backup] exclude = [...]`。
- restore：`--include PATTERN`、`--exclude PATTERN`。

格式不變：kist-format 與 `docs/format.md` 都沒動，snapshot 不記錄用過的樣式。被排除的東西只是
不在樹裡。

## 決定

### 1. 一套路徑空間：節點在 snapshot 裡的路徑

樣式比對的對象＝root 定位的切段（`fsmeta::locator_components`）接上 tree 裡一層層的節點名，
也就是整份還原時節點在目標底下的位置。restore 的落點、`kist mount` 的 `<client>/<ts>/` 之下、
backup 的排除，用的都是這同一套切段（mount 的 `corefs.rs` 與 restore 呼叫同一個函式）。

- 本機來源：backup 會 canonicalize，所以是 **canonical 絕對路徑**（symlink 已解開），與
  `kist snapshots` 的 PATHS 欄位相同。
- 遠端來源：去掉 `scheme://`。`sftp://nas/srv/data` 底下的 `x` 是 `/nas/srv/data/x`；錨定樣式要從
  落點寫起，不是遠端主機上的路徑（`source_backup.rs` 的 `mem://bucket/data` 案例釘住）。
- Windows：本機 root 的定位是 `C:\…` 一整段，錨定樣式比對不到。不錨定的樣式照常比對。沒在 Windows
  上跑過（UNVERIFIED，同 PLAN「已接受的限制」）。

### 2. 樣式語法

gitignore／restic 風格的子集，規則全在 `crates/kist-core/src/filter.rs` 的模組文件：

- `/` 開頭從頂端比起，否則任何深度都比（`*.tmp`、`node_modules`、`build/out`）。
- 以 `/` 分段，每段恰好一個元件：`*`、`?`、`[abc]`／`[a-z]`／`[!a]`／`[^a]`、`\` 跳脫。整段只有
  `**` 是零或多個元件。
- `?` 與清單比對**一個字元**（CJK 字也是一個）；名稱不成 UTF-8 的地方，一個 byte 算一個字元。
- 區分大小寫；結尾的一個 `/` 去掉（目錄與同名檔案一起比對）。

直接報錯的寫法，每一條都是「不報錯的話會默默比對不到東西」：空樣式、`/`、`//`（空段）、`.`／`..`
段（snapshot 的路徑沒有這種段；`./build` 這種相對寫法最常見）、夾在字裡的 `**`、沒關上的 `[`、
反向的區間、結尾的 `\`、開頭的 `!`（反向樣式不支援，字面的 `!` 寫 `\!`）。大括號是字面字元。

### 3. 不用 globset，自寫逐段比對

假設與證據：

| 假設 | 來源 |
| --- | --- |
| globset 0.4.20 的 `?` 吃一個 byte：`**/?.txt` 對 `/a/ä.txt` 回 false | 量測（scratchpad probe，結束碼 1 的那一條） |
| globset 對 `a**b`、`**a`、`a/**b` 照收不報錯 | 量測（同一個 probe） |
| globset 對換行、不成 UTF-8 的 bytes 比對正確 | 量測（同一個 probe） |
| restore 剪枝需要「這個目錄底下還可能比對到嗎」的逐段狀態，globset 只做整條路徑比對 | 讀 globset 的 API |
| 檔名常有非 ASCII（CJK） | 猜測（負責人用中文；既有測試資料 `make_source` 本來就有中文檔名） |

做法：每條樣式編成一串段（`**`、字面段、萬用段），走訪時記 NFA 狀態（每條樣式比到第幾段），子項目
往下走一步。字面段逐 byte 比；萬用段以「記住最後一個 `*`」回溯，時間是 O(token 數 × 名稱長度)，
不配置記憶體。

驗證：

- 單元測試覆蓋每條規則與每種報錯。
- proptest 2000 案：與天真的遞迴參考實作（另一套 UTF-8 解碼）對每個前綴比對結果；另驗證「整條比對到
  時每個前綴的狀態都還活著」——restore 依它剪枝，這條保證剪不掉會被選到的東西。
- 負向對照：植入「`**` 不比零段」與「`*` 回溯一個 byte」兩個 bug，proptest 都抓到。第二個原本漏抓，
  補上含多 byte 字元的反向清單（`[!中]`）後才抓到；兩個縮小後的種子留在
  `crates/kist-core/proptest-regressions/filter.txt`。

### 4. backup：指名的來源不排除

- 樣式只管來源底下的東西：root 本身與它的上層都不比對（同 restic 0.19 #5767）。單一檔案來源也一樣。
- 被排除的目錄算一個 `excluded`，底下不再走；排除先於深度上限檢查（被排除的過深目錄不算錯誤）。
- 跳過名字不影響 parent 串流（只要求查詢名稱遞增），快速路徑照舊：帶同一組排除的第二次 backup
  0 新 chunk、每個檔案都走快速路徑（`backup_exclude.rs`）。
- 設定檔的樣式在 `Config::validate` 就編譯：寫錯時 `kist run`／`kist serve` 起不來，不會等到排程
  時間才失敗。CLI 寫錯的樣式在碰 repo 之前以結束碼 1 結束，樣式檔裡的錯誤點名 `檔案:行號`。

### 5. restore：選取、剪枝、延後建立目錄

- 每個節點有一個位置：include 與 exclude 兩個狀態，加上「自己或上層被 include 比對到」。exclude 優先。
- restore 的樣式**也比對 root 本身與它的上層**。理由：restore 的 root 不是使用者在這個命令指名的，
  只是 snapshot 裡的資料；`--include /home/me` 選到 `/home/me/docs` 這個 root、`--exclude /srv`
  跳過 `/srv/data` 這個 root，都要靠它。restic 在這點上是否相同：UNVERIFIED（沒有查證也沒有
  實測）。代價是與 backup 不對稱：root 是 `/tmp/data` 時，`--exclude tmp` 會排除整個 root（README
  寫明，`restore_select.rs` 釘住）。
- 延後建立目錄：restore 的每一層目錄改成「待建」（`PendingDir`，第一個 commit 只做這個重構、行為
  不變），第一次要往裡面寫東西時才沿路建上來。沒被選到、但底下也許有東西被選到的目錄先不建；最後
  都沒建的不計數、不套 metadata。被建出來的上層照記錄套 mode 與時間、計進 `dirs`——它們是 snapshot
  裡的目錄，只是沒有全部內容。
- 剪枝：include 狀態全死（底下不可能被選到）的子樹連 tree 都不讀，root 也一樣（落點不建、root tree
  不讀）。證據：刪掉 photos 子樹的 tree 物件，整份還原報 1 個錯，只 include docs 的還原 0 個錯；把
  剪枝拿掉時這個測試變紅。不錨定的 include（`*.pdf`）每一層都可能比對到，照樣要走整棵樹。
- 沒選到東西的 include → `RestoreSummary::unmatched_includes`，CLI 印警告、結束碼 3，選到的照樣
  還原。定義是「沒比對到任何走得到、沒被排除的節點」：比對到的全被排除也算（被排除的子樹不會走進去，
  裡面的比對無從算起，所以規則只能是這樣才前後一致）。
- 硬連結：表只記真的還原出來的名字。第一個名字沒被選到時，後面的名字還原成內容相同的獨立檔案。

## 沒做

比對 restic 時列過、這次刻意不做的：`--exclude-caches`、`--exclude-if-present`、`--one-file-system`、
`--exclude-larger-than`、不分大小寫的 `--iexclude`、`--files-from`；restore 的 `--dry-run`、
`snap:子目錄` 寫法、`--include-file`／`--exclude-file`；反向樣式（`!`）與大括號。

## 後果

- JSON：`backup --json` 的 `report` 多 `excluded`，`restore --json` 多 `unmatched_includes`。都是新增
  欄位，既有欄位不變。
- 結束碼：restore 多一種 3（沒選到東西的 include）。
- `BackupOptions` 多一個必填欄位 `exclude`（程式內嵌的呼叫端要補 `PathFilter::default()`）；
  `RestoreOptions` 多 `include`／`exclude`，`Default` 仍是整份還原。
- 沒量測：大樹上逐段比對的 CPU 成本、剪枝在 S3 repo 上省下的時間。每個節點每條樣式做一次比對；
  v3 的每個 tree 是一個物件，所以每少讀一個 tree 就少一個請求。
