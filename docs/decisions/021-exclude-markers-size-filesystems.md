# ADR 021：排除規則（二）——記號檔、檔案大小、檔案系統邊界

日期：2026-10-10　狀態：已採用

## 背景

ADR 020 完成路徑樣式的排除與部分還原後，「沒做」清單裡最常用的是 restic 的 `--exclude-caches`、
`--exclude-if-present`、`--exclude-larger-than`、`--one-file-system`。負責人說「繼續」，這份補上它們。
格式不變。

## 決定

### 1. 排除規則收成一個 `Exclusions`

`BackupOptions.exclude` 從 `PathFilter` 改成 `Exclusions { patterns, if_present, larger_than,
one_file_system }`，`Default` 仍是什麼都不排除。理由：每多一個條件就要改十幾處直接寫出
`BackupOptions` 的測試與內嵌呼叫端，收成一個欄位之後新條件不必再動它們。設定檔的 `[backup]` 由
`BackupSection::exclusions()` 一次編譯（載入時就驗）。

### 2. 記號檔（`--exclude-if-present NAME[:HEADER]`、`--exclude-caches`）

- 語意同 restic 與 CACHEDIR.TAG 規格：目錄裡有記號時，除了記號本身，目錄的內容都不備份，目錄本身照備
  （還原出來看得到「這裡刻意沒備」）。有 HEADER 時記號必須是開頭為 HEADER 的一般檔案；沒有 HEADER 時任何
  種類都算（目錄、symlink 也算）。`--exclude-caches` 就是 `CACHEDIR.TAG` 加上規格的 43 bytes 簽章。
- 判斷要在處理子項目之前做，但目錄的列舉是惰性串流、名字排序後記號不一定在前面，也不能整份緩衝
  （ADR 011 的記憶體門檻）。所以 `Source` 加一個 `probe(rel, limit)`：沒有 → `None`；一般檔 → 開頭最多
  `limit` bytes；其他種類 → 空的。本機來源以 lstat（不跟隨）＋只對一般檔 open 覆寫——FIFO 不 open，否則
  沒有寫端時會一直等。預設實作列上一層目錄找同名條目：遠端來源每個目錄每個記號多一次列舉（沒量測成本）。
- `LocalSource::read` 把 NotFound 也包成字串錯誤，所以不能拿 read 的失敗當「不存在」——這是另加 `probe`
  而不是用 `read` 的原因。
- 判斷失敗（例如記號讀不到）記警告、當作沒有：那個目錄照常整個備份。方向是「多備」，不會少備，所以不算
  backup 的錯誤（結束碼不變）。
- 指名的來源裡有記號也照規則：記號管的是目錄內容，不是來源本身。

### 3. 檔案大小（`--exclude-larger-than SIZE`）

- 大小取自來源的列舉（本機是列舉時的 lstat）。只排除比上限大的檔案，剛好等於的照備；指名的檔案來源不排除。
- `SIZE`：位元組數，或加 `k`／`m`／`g`／`t`（1024 為底、不分大小寫）。`1MB`、`1MiB`、小數一律報錯——與
  `parse_duration` 同樣嚴格，免得「以為設了 1 MB、其實解析成別的」。

### 4. 檔案系統邊界（`--one-file-system`）

- 每個本機目錄來源記下自己的裝置號碼；走訪時裝置號碼不同的目錄照備成空目錄（metadata 照記），不往下走；
  這樣的檔案（單一檔案的 bind mount）不備份。都計進 `excluded`，掛載點算一個。
- btrfs 的子卷各有自己的裝置號碼（量測：在 `/tmp` 的 btrfs 上建子卷，父目錄 88、子卷 163），所以子卷也是
  邊界。
- 遠端來源沒有裝置號碼、非 unix 的 capture 一律給 0：兩者都在開始前回錯，不默默整個走進去。
- 驗證：CLI 測試在 user＋mount namespace 裡把 tmpfs 掛到來源底下再 backup，不加旗標的對照組證明掛載真的
  生效；不准 unprivileged namespace 的環境印出原因後略過（這台機器上有跑到）。另以實際的 binary 對 btrfs
  子卷手動跑過一次：加旗標時子卷是空目錄、excluded 1、files 1；不加時 files 2。

## 沒做

`--iexclude`（不分大小寫）、`--files-from`、restore 的 `--dry-run` 與 `snap:子目錄`、restore 的樣式檔、
反向樣式與大括號：見 PLAN M11 的後續。

## 後果

- 公開介面：`BackupOptions.exclude` 的型別變成 `Exclusions`；新增 `Marker`、`Source::probe`（有預設實作，
  既有的 `Source` 實作不必改）。
- 設定檔 `[backup]` 新增 `exclude_if_present`、`exclude_caches`、`exclude_larger_than`、`one_file_system`。
- 沒量測：記號判斷在遠端來源上的請求成本、大樹上的額外 lstat 成本。
