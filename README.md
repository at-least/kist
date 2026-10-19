# kist

去重、加密、可多台機器共用 repo 的備份工具（Rust）。

> 目前狀態：**格式 v4（2026-10-11，[ADR 026](docs/decisions/026-v4-format.md)；實作七段全部完成，
> 2026-10-18）**。`init` / `backup` / `snapshots` / `restore` / `check` / `forget` / `prune` / `rebuild` /
> `settings` / `key` / `remove-unknown-packs` 可用；多台機器可同時備份到同一個 repo，GC 不需要鎖（世代柵欄：
> 只有建立下一個世代成功的 prune 能刪東西，刪除只看 prune 自己驗證過的資料，任何物件都不覆寫；規格
> [docs/format-v4.md](docs/format-v4.md)）。v4 不讀 v3 的 repo、沒有遷移（系統還沒上線，負責人決定）。
> 備份來源可以是本機路徑，也可以是 `sftp://` / `s3://` 的**遠端來源**——client 當轉運：讀遠端、本機切塊
> 加密後上傳，金鑰不出機器（[ADR 016](docs/decisions/016-v3-remote-sources.md)）。儲存後端：本機、S3（含
> MinIO）、SFTP（[ADR 013](docs/decisions/013-sftp-backend.md)）、rclone 橋接
> （[ADR 014](docs/decisions/014-rclone-bridge.md)；能 backup、restore、forget，不能 prune）。
> 另有：唯讀 FUSE 掛載（[ADR 015](docs/decisions/015-mount-fuse.md)）、選配 Reed-Solomon parity
> （[ADR 009](docs/decisions/009-m5-parity.md)）、設定檔／排程／webhook（`kist run`）、`--json`、
> Prometheus metrics 與 Web UI（`kist serve`）。v4 的凍結 fixture 在 `crates/kist-core/tests/fixtures/v4/`
> （原本的 Go 參考實作已移除，見 [ADR 018](docs/decisions/018-remove-go-reference.md)；拿掉 Go 之後的
> Rust 端盤點與修正清單見 [ADR 019](docs/decisions/019-rust-after-go.md)）。
> 已接受的限制：Windows VSS（不在路線圖上，見 [PLAN.md](PLAN.md)）。

## 建置

```sh
cargo build --release
./target/release/kist --help
```

## 使用

```sh
export KIST_REPO=/path/to/repo          # 或每個命令加 --repo
export KIST_PASSWORD=...                # 或 --password-file，或互動輸入

kist init                               # 建立 repo
kist init --grace 48h                   # 建 repo 時就定 settings（之後調低 grace 要排空一個舊的，見「空間回收」）
kist backup ~/Documents ~/Photos        # 備份，產生一個 snapshot
kist backup ~ --exclude node_modules --exclude '*.tmp'   # 排除規則，見「排除規則」一節
kist backup sftp://nas.local/srv/data   # 遠端來源也能備：讀遠端、本機切塊加密後上傳
kist snapshots                          # 列出 snapshot
kist restore latest /tmp/out            # 還原到 /tmp/out/<原本的絕對路徑>
kist restore latest /tmp/out --include ~/Documents/report.pdf   # 部分還原，見「部分還原」一節
kist check                              # 檢查一致性（不下載資料）
kist check --read-data                  # 下載並驗證每個 pack 的每一項
kist check --repair                     # 補回遺失的副本（只建立、不覆寫；維護角色）
kist forget --keep-daily 7 --keep-weekly 4 --keep-monthly 12   # 依保留政策刪 snapshot
kist forget latest                      # 或指定 snapshot（id、時間戳前綴、latest）
kist forget latest --retire             # 每台機器每組路徑最新的一份是下一次 backup 的 parent，要刪得明說
kist prune                              # 回收空間（每跑一輪寫一個世代；見「空間回收」）
kist forget --keep-last 10 --prune      # 一次做完
kist settings                           # 看 repo 的 settings（grace、replicas、pack 大小）
kist settings --grace 48h               # 改 settings：寫一個新世代，不刪任何東西
kist rebuild                            # 最新世代的 map 分片遺失或壞掉時重建（不刪任何東西）
kist key add                            # 多一個密碼；`key list` / `key remove <slot>` 管理槽
```

結束碼：0 成功；1 失敗；3 完成但有項目被略過（backup）、還原失敗或 `--include` 沒選到東西（restore）、
刪不掉或刪除沒做完（prune）——請看警告。

`restore` 先把每個檔寫進同目錄的隱藏暫存檔 `.kist-restore-*`，完整寫好才 rename 成正式名：還原失敗
時目標處原有的檔不動（以前會被刪掉），擋路的 symlink、目錄或特殊檔回報錯誤、不動它；再還原到先前
還原出的唯讀目錄（例如 0555）時，會暫時加上擁有者寫入權，最後套回記錄的 mode。覆寫大檔時新舊兩份
會同時佔空間；Ctrl-C 中斷的 restore 可能留下 `.kist-restore-*`，可以直接刪掉。暫存檔建成只有擁有者可
讀寫（0600），記錄的 mode 最後才套上：metadata（例如 xattr）套不上時內容照樣還原、回報錯誤，檔案停在
0600；沒記錄 mode 的條目（s3／generic 來源）覆寫既有檔時沿用原檔的權限（再經 umask），新檔照舊是
umask 預設。
這類本機擋路（含目錄位置上擋著的 symlink 或非目錄）與 xattr 設不上去的錯誤，訊息不再說「object …
is corrupt」，改成「<路徑>: a symlink is in the way of a restored file」之類（`restore --json` 的
`errors` 字串同樣改變）；結束碼不變。

`restore` 在目標之下逐層開著目錄往下寫（unix；Windows 不變）：還原途中某一層被換成 symlink，寫入仍落在
原本開著的那個目錄，不會跟著 symlink 寫到目標之外（以前會）。只能進入、不能讀的目錄（例如 0311；以 root
備份、以一般使用者還原時會遇到）在 Linux 照樣還原得進去，子項目與硬連結都保得住；再還原到已經存在的這種
目錄時，只有它自己的 metadata 套不上、回報 Permission denied（以前也是）。Linux 以外的 unix 沒有這條
退路（依程式；沒在 macOS 上跑過）：目標本身要可讀，否則整個 restore 回報 Permission denied；snapshot 裡
的這種目錄，名字落在其中的硬連結會退成各自獨立的複製（只記 warn），再還原到已經存在的這種目錄時，它
回報 Permission denied、子項目被跳過。

以 root（euid 0）執行 `restore` 時，記錄了 uid/gid 的條目（posix 來源）會先設回記錄的擁有者、再套
mode（以前從不 chown：別人的 4755 檔會還原成 root 擁有的 setuid 檔）。擁有者設不回去的條目回報錯誤
（結束碼 3），mode 照套但去掉 setuid／setgid；沒記錄 uid/gid 的條目（遠端來源）同樣去掉這兩個位。
非 root 還原不變：檔案屬於還原者本人，mode 照記錄的值。

密碼檔（`--password-file` 與設定檔的 `password_file` 同一份規則）：只取第一行、去掉結尾的 `\r`，
空的算錯誤。

> **行為變更**：以前 CLI 的 `--password-file` 不去 `\r`。密碼檔第一行結尾是孤立的 `\r`（後面沒有
> `\n`）時，以前用這個檔 `kist init` 建的 repo，密碼其實帶著那個 `\r`；現在同一個檔讀出來不含
> `\r`，會回「wrong password」。這種 repo 改用 `KIST_PASSWORD` 給出含 `\r` 的密碼（bash／zsh：
> `KIST_PASSWORD=$'你的密碼\r'`）；改密碼檔沒有用，任何密碼檔都讀不出結尾的 `\r`。
> `--password-file` 優先於 `KIST_PASSWORD`，而它也吃環境變數 `KIST_PASSWORD_FILE`：有匯出的話先
> `unset KIST_PASSWORD_FILE`。`kist run` 一直是這個讀法，不受影響。

### 從清單讀來源路徑（`backup --files-from`）

```sh
kist backup --files-from ~/backup-list.txt               # 一行一個；去前後空白，空行與 # 開頭的行略過
kist backup --files-from-verbatim list.txt               # 一行就是一個路徑，照原樣
find ~/proj -name '*.db' -print0 | kist backup --files-from-raw -   # NUL 分隔，從標準輸入讀
```

旗標名稱同 restic，但**不展開萬用字元**：列的路徑照字面找，不存在就以結束碼 1 結束、不留 snapshot。
可以和命令列的路徑一起給；讀完一個路徑都沒有也是結束碼 1。切法：

- 兩種文字清單：開頭的 UTF-8 BOM 去掉，行尾的 `\r`（CRLF）去掉，verbatim 也是（同 restic）。restic 只對
  具名檔案去 BOM、標準輸入不去；kist 兩邊都去。UTF-16 的清單（restic 會轉碼）不支援，請存成 UTF-8。
- raw：每個名字都要以 NUL 結尾、不能是空的，否則結束碼 1（同 restic）。
- `-`（標準輸入）整個命令只能給一次（清單與樣式檔合計）；給兩次以結束碼 1 結束，不會讓後面那個默默讀到
  空的。

### 排除規則（`backup --exclude`）

```sh
kist backup ~ --exclude node_modules --exclude '*.tmp' --exclude "$HOME/.cache"
kist backup ~ --exclude-file ~/.config/kist/exclude    # 一行一條；空行與 # 開頭的行略過
```

被排除的檔案與目錄不進 snapshot（目錄整棵不走），數量印在 backup 的摘要（`excluded: N`）與
`--json` 的 `report.excluded`。設定檔寫成 `[backup] exclude = ["node_modules", "*.tmp"]`。樣式檔
的行與 `--files-from` 的文字清單同樣切（BOM、CRLF），去掉前後空白；`-` 是標準輸入。

樣式比對的是節點在 snapshot 裡的路徑，也就是整份還原時它在目標底下的位置：

- 本機來源是 **canonical 絕對路徑**（symlink 已解開，與 `kist snapshots` 的 PATHS 欄位相同）。
  遠端來源只去掉 `scheme://`，第一段是 URL 裡的 `[user@]host[:port]` 原樣：`sftp://nas/srv/data`
  底下的 `x` 是 `/nas/srv/data/x`，`sftp://me@nas:2222/srv/data` 底下的是 `/me@nas:2222/srv/data/x`。
- `/` 開頭的樣式從路徑頂端比起；其他樣式在每一層都比對（`*.tmp`、`node_modules`、`build/out`）。
- 以 `/` 分段，每段比對恰好一個路徑元件：`*` 任意多個字元、`?` 一個字元（CJK 字也是一個）、
  `[abc]`／`[a-z]`／`[!a]` 一個字元、`\` 讓下一個字元照字面。整段只有 `**` 時比對零或多個元件
  （`/home/me/**/build`）。區分大小寫；結尾的 `/` 會被去掉。
- 指名的來源本身不會被排除（`kist backup ~/proj --exclude proj` 照樣備份 `~/proj`）。包在另一個指名來源
  裡的也一樣：它併進外層的 root，但排除條件不排除它；它的上層被排除時，只沿著通往它的路徑備份。
- `!` 開頭是反向樣式（同 restic 文件描述的 gitignore 行為）：樣式依序編號，一個路徑上**最後**一個比對到
  它的樣式決定它排不排除。`kist backup ~ --exclude "$HOME/*" --exclude "!$HOME/Documents"` 只備
  Documents。被排除的目錄整棵不走，裡面的東西加不回來。字面的 `!` 寫成 `\!`。編號的順序：`--exclude` 的
  值、`--exclude-file` 的行，然後是 `--iexclude` 的值、`--iexclude-file` 的行。
- `--iexclude`／`--iexclude-file`（設定檔 `iexclude`）：同上，但不分大小寫（CJK 以外有大小寫的字也折，
  不成 UTF-8 的 byte 不折）。
- 大括號是字面字元（`{a,b}` 不展開）。
- 樣式寫錯（例如沒關上的 `[`、夾在字裡的 `**`、`.`／`..` 段）時，backup 在碰 repo 之前就以結束碼 1
  結束；樣式檔裡的錯誤點名 `檔案:行號`。設定檔裡寫錯，`kist run`／`kist serve` 起不來。

樣式以外的排除條件（設定檔的對應鍵寫在括號裡）：

```sh
kist backup ~ --exclude-caches                 # 有 CACHEDIR.TAG 的快取目錄（exclude_caches = true）
kist backup ~ --exclude-if-present .nobackup   # 有 .nobackup 的目錄（exclude_if_present = [".nobackup"]）
kist backup ~ --exclude-larger-than 2G         # 大於 2 GiB 的檔案（exclude_larger_than = "2G"）
kist backup / --one-file-system                # 不進入別的檔案系統（one_file_system = true）
```

- 記號檔：目錄裡有 `NAME` 時，除了記號本身，目錄的內容都不備份，目錄本身照備。寫成 `NAME:HEADER` 時，
  記號必須是開頭為 HEADER 的檔案。`--exclude-caches` 就是 `CACHEDIR.TAG` 加上規格的簽章
  （<https://bford.info/cachedir/>；cargo 的 target 目錄與 `~/.cargo/registry` 都有這個檔）。沒有 HEADER
  的記號只看在不在，讀不了也算數。每個目錄每個記號查一次：本機是一次 lstat，有 HEADER 時再讀開頭；遠端
  來源是多列一次那個目錄。
- 大小：`k`／`m`／`g`／`t` 以 1024 為底、不分大小寫；只排除比它大的，剛好等於的照備。
- `--one-file-system`：裝置號碼和它所屬來源不同的目錄（掛載點，**也包括 btrfs 的子卷**）照備成空目錄，
  底下不走。`/home` 是另一個檔案系統或子卷時，`kist backup / --one-file-system` 不含 `/home` 的內容，
  要一起備就把 `/home` 也列成來源。只適用 unix 的本機來源（遠端來源與 Windows 直接報錯）。
- 這些條件和樣式一樣不排除指名的來源本身，被排除的都計進 `excluded`（掛載點、被清空內容的目錄裡每一項各算一個）。

### 部分還原（`restore --include`／`--exclude`）

```sh
kist restore latest /tmp/out --include /home/me/docs/report.pdf   # 只還原這個檔
kist restore latest /tmp/out --include /home/me/docs              # 整個目錄
kist restore latest /tmp/out --include '*.pdf' --exclude /home/me/old
kist restore latest:/home/me/docs /tmp/docs                       # 目錄的內容直接放進 /tmp/docs
kist restore --dry-run latest /tmp/out --include '*.pdf'          # 只列出會寫什麼
```

- 樣式規則與 `backup --exclude` 相同，比對的是節點整份還原時在目標底下的路徑（也是 `kist mount`
  裡 `<client>/<timestamp>/` 之下看到的路徑）。檔案照樣落在 `<target>/<原本的路徑>`。
- `--include`：只還原比對到的節點與它們底下的一切。通往它們的上層目錄照樣建立、套記錄的 mode 與
  時間（也計進 `dirs`）；沒被選到的分支一個目錄也不建。錨定的 include 只讀它路徑上的 tree，其他子樹的
  tree 不讀；不錨定的 include（`*.pdf`）要走過整棵樹才找得到。
- `--exclude`：不還原比對到的節點（目錄整棵不走）。與 `--include` 同時比對到時排除優先。
- 與 backup 不同，restore 的樣式也比對 root 本身與它的上層：root 是 `/tmp/data` 的 snapshot，
  `--exclude tmp` 會排除整個 root。只想排除 root 底下名為 `tmp` 的目錄，寫 `/tmp/data/**/tmp`。
- 沒有選到任何東西的 include 樣式印警告、結束碼 3，`--json` 的 `unmatched_includes` 列出它們；其他選到
  的照樣還原。一條樣式要涵蓋某個被還原的節點（比對到它或它的上層）才算選到東西：比對不到、比對到的全被
  排除、或全被後面的 `!` 取消，都算沒選到。還原有錯誤時（例如某個目錄建不起來），失敗處底下沒有走到，
  警告會註明這一點，不說成 snapshot 裡沒有。
- 只有 `!` 樣式的 include 什麼都選不到，以結束碼 1 結束。
- 硬連結的第一個名字沒被選到時，後面的名字還原成內容相同的獨立檔案。
- `SNAPSHOT:PATH`（同 restic）：PATH 是 snapshot 裡的一個目錄（某個 root、root 裡的目錄，或幾個 root
  共同的上層），它的內容直接放進目標，不重建上面的路徑；目標本身不套那個目錄的 metadata。樣式照樣比對
  snapshot 裡的完整路徑。PATH 不在 snapshot 裡或不是目錄時，以結束碼 1 結束，什麼都不建；PATH 所在的
  root 被樣式排除時也照樣檢查。
- root 的落點（`<target>/<原本的路徑>`）建不起來時（例如那裡擋著 symlink），整個還原以結束碼 1 結束，
  即使 include 只選到更深的節點。
- `--dry-run`：列出會寫的目錄、檔案、symlink（`dir`／`file`／`link` 加上目標底下的路徑），目標連建都
  不建；計數與「沒選到東西」的警告和真的還原相同。只讀 tree、不讀檔案內容，所以壞掉的 chunk、目標處
  擋著的東西這類錯誤只有真的還原才看得到。
- `--include-file`／`--exclude-file`：樣式一行一條，格式同 `backup --exclude-file`（`-` 是標準輸入）。不分
  大小寫的版本是 `--iinclude`、`--iexclude` 與 `--iinclude-file`、`--iexclude-file`。一條樣式都沒有的
  include 樣式檔（空的、只有註解）以結束碼 1 結束：照讀的話 include 是空的，等於整份還原。
- include 也可以用 `!` 從選到的目錄裡取消一部分（`--include /home/me/docs --include '!/home/me/docs/old'`）；
  「沒選到東西」的檢查不看反向樣式。

### `--json`

`backup` / `snapshots` / `restore` / `check` / `forget` / `prune` / `rebuild` / `settings` / `key list` /
`run` 都支援 `--json`：結果以 JSON 印到 stdout（錯誤照舊在 stderr、結束碼不變），給腳本和監控消費。

- `backup` 印 `snapshot`（寫下的 snapshot 的 key）、`parent`、`generation`（backup 開始時的世代）、`stats`、
  `report`（過程計數）與 `roots`（每個來源的 `path` 與 `type`：`file` / `dir` / `symlink`）。`snapshots`
  每筆有 `key`、`client`、`timestamp`、`time`、`paths`、`roots`、`parent`、`stats`。
- snapshot 的 `paths` 原本是 OS bytes，JSON 裡以 UTF-8 呈現，非 UTF-8 的位元組會換成 U+FFFD。
- `forget` 的輸出帶 `dry_run` 欄位——「removed」是已刪還是會刪，看這個；`prune` 沒有 dry run，印的是
  這一輪的報告（寫出的世代、標記／復活／待刪／已刪的數量、repack、刪不掉的、沒做完的原因、警告）。
- `backup` 的 `report.excluded`：被排除樣式比對到的項目數；`restore` 的 `unmatched_includes`：沒選到
  東西的 include 樣式（定義見上面的部分還原），`dry_run`：是不是 `--dry-run`（是的話計數是「會還原」的）。
- `run --once --json` 印出 JobOutcome 陣列；常駐模式下每件工作結束印一行 JSON（NDJSON）。

### 設定檔與排程

```toml
# /etc/kist/backup.toml —— backup 主機：只需要 Put/Get/List 權限
repo = "s3://my-bucket/backups/laptop"
password_file = "/etc/kist/password"      # 只能用檔案
timezone = "local"                        # 排程用的時區，或 "utc"

[backup]
paths = ["/home", "/etc"]                 # 或只有一個 sftp:// 或 s3:// URL（見「遠端來源」）
schedule = "0 2 * * *"                    # cron（5 欄；前面可多加一欄秒）
exclude = ["node_modules", "*.tmp"]       # 選填；規則見「排除規則」
# trust_mtime = true                      # 選填，只有 sftp:// 來源；同 --trust-mtime
# reread_every = "7d"                     # 選填，要 trust_mtime；同 --reread-every

[notify]
webhook_url = "https://example.com/hook"  # 每件工作結束 POST 一個 JSON
on = ["failure", "incomplete"]            # 也可以加 "success"
```

```toml
# /etc/kist/maintenance.toml —— 另一台維護主機：需要 Delete 權限
repo = "s3://my-bucket/backups/laptop"
password_file = "/etc/kist/password"
[forget]
keep_daily = 7
keep_weekly = 4
keep_monthly = 12
schedule = "0 4 * * *"
[prune]
schedule = "0 5 * * *"
repack_below = 50                         # 選填；同 `prune --repack-below`
read_data = false                         # 選填；同 `prune --read-data`（每輪整份讀資料 pack，慢）
```

```sh
kist run --config /etc/kist/backup.toml          # 常駐，照排程跑（Ctrl-C 等目前工作做完再結束）
kist run --config /etc/kist/backup.toml --once   # 每件工作各跑一次就結束（給外部 cron / systemd timer）
kist serve --config /etc/kist/backup.toml        # 同 run，另外開 HTTP 端口：/metrics 與 Web UI
```

> 排程錯過不補跑：daemon 沒在跑、機器休眠或停機跨過了排程時間的話，那次就跳過，
> 下一輪是排程的**下一次**時間（不是醒來後立刻補跑）。常休眠的機器請用外部
> cron / systemd timer 配 `run --once`。對應的訊號是 `kist_job_last_success_timestamp_seconds`
> 多久沒更新。

`[forget]` / `[prune]` 刻意跟 `[backup]` 分開放：backup 主機的憑證不該有 Delete 權限（抗勒索）。

grace（prune 標記之後等多久才刪，見「空間回收（GC）」一節）不在設定檔裡：它存在 repo 的 settings 裡
（`kist settings --grace`，預設 72h），backup 與 prune 自動用同一個值，分在兩台主機也不必對齊。backup 必須在
grace 減一小時內做完，超過就不寫 snapshot、以錯誤結束（重跑即可，已上傳的資料會沿用）。v3 的
`[backup] gc_grace`、`[prune] grace`、`clock_skew`、`inactive_after` 不再接受：寫了設定就載入失敗，`kist run`／
`kist serve` 不會啟動。`cache_dir` 沒寫時用使用者的快取目錄（同 CLI）。

### Prometheus metrics（`kist serve`）

`kist serve` = `kist run`（照排程跑工作）+ 一個 HTTP 端口（`--http`，預設 `127.0.0.1:9898`）：

- `GET /metrics`：OpenMetrics 文字格式，給 Prometheus 抓。
- `GET /healthz`：活著沒。

`/metrics` **沒有認證**，預設只綁 loopback；內容含 repo 位置、機器名稱與備份排程——
不要暴露到不受信任的網路。主要指標：

| 指標 | 意義 |
| --- | --- |
| `kist_job_runs_total{job,status}` | 每種工作完成次數（success / incomplete / failure） |
| `kist_job_last_success_timestamp_seconds{job}` | 上次**完全成功**的 Unix 時間（持久化在 `cache_dir/jobstate-<repo hash>-<job>.json`，重啟後種回；incomplete 不算成功） |
| `kist_job_last_duration_seconds{job}` | 上次執行時長 |
| `kist_backup_files` / `kist_backup_bytes` | 最後一次 snapshot 的檔案數 / 總 bytes |
| `kist_backup_bytes_new` / `kist_backup_chunks_new` | 最後一次 backup 上傳的量 |
| `kist_backup_skipped_items` | 最後一次 backup 略過的項目數 |
| `kist_prune_deleted_bytes_total` | prune 自 daemon 啟動以來刪掉的 bytes |
| `kist_prune_marked_objects` / `kist_prune_live_packs` | 最後一次 prune 標記（condemned）的 pack 數 / 結束時 live 的 pack 數 |

告警請用「多久沒看到成功」（例如 `time() - kist_job_last_success_timestamp_seconds{job="backup"} > 90000`），
不要用「有沒有成功過」；incomplete（有項目被略過/刪不掉）不算成功，所以要另外對
`increase(kist_job_runs_total{status="incomplete"}[24h]) > 0` 告警，或用 webhook 即時通知 failure / incomplete。

### Web UI（`kist serve`）

`kist serve` 同時提供一個網頁：開 `http://127.0.0.1:9898/` 就是目前狀態——

- **Status**：正在跑的工作與即時進度（phase、檔案/目錄數、bytes、目前處理的路徑）、
  排隊中的工作、各工作的排程與下次執行時間、最近的執行紀錄；每 2 秒自動更新。
- **Snapshots**：按 Refresh 列出 repo 裡的 snapshot（每次列都要重新開 repo，約一秒）。
- **Run backup now**：手動觸發一次 backup。UI 唯一的寫入操作——forget / prune 刻意不給按。

是否開放這顆按鈕由 `[serve]` 決定：

```toml
[serve]
password_file = "/etc/kist/ui-password"           # UI 密碼（跟 repo 密碼是不同的秘密）
allowed_hosts = ["backup.example.internal:9898"]  # 額外允許的 Host header
```

- 沒設 `password_file`：UI 唯讀（不用登入），按鈕換成停用說明。
- 設了：整個 UI（含靜態檔）要 HTTP Basic auth，帳號任意、只比密碼。

安全規則（細節與理由見 [ADR 007](docs/decisions/007-m4-web-ui.md)）：

- `Host` header 不在白名單 → 421。白名單 = 綁定位址與幾種 loopback 寫法（自動允許）
  加上 `allowed_hosts`；逐字比對，防 DNS rebinding。
- 改狀態的請求只接受 htmx 送出的（`HX-Request: true`、非 `cross-site`），否則 403。
- `/metrics`、`/healthz` 不在這些規則內（見上節；既有的 scrape 設定不受影響）。

UI 密碼走明文 HTTP：預設只綁 loopback，要遠端存取請放在有 TLS 的 reverse proxy 後面。

### 空間回收（GC）

`forget` 只刪 snapshot；`prune` 才回收資料。v4 的 prune 是**世代鏈**（[docs/format-v4.md §11](docs/format-v4.md)、
[ADR 026](docs/decisions/026-v4-format.md)）：每跑一輪，prune 讀全部 snapshot 與每個 pack 的 trailer，算出一份
新的 map，以「只在不存在時建立」寫下一個世代的 head；寫得成的那一輪才能刪東西，兩個 prune 重疊時輸的那個
什麼都不刪。沒人引用的 pack 先被**標記**（condemned），等它的標記在後端時間與寫入者時間**兩個時鐘上**都超過
grace（預設 72 小時）、再經過刪前驗證，下一輪才真的刪；中間任何一台機器重新用到它，標記就撤銷。所以
`prune` 可以跟 backup 同時跑、可以排程每天跑，不需要鎖；剛 forget 完馬上 prune 不會釋放空間，那是設計——
沒人用的資料最多約兩個 grace 加 prune 的間隔才刪。

- grace 存在 repo 裡（`kist settings --grace`，至少 2h），不是命令列或設定檔的選項。每個 backup 必須在 grace
  減一小時內做完，否則不寫 snapshot、以錯誤結束（重跑即可）。調高立刻生效；調低要「排空」一個舊的 grace
  才完全生效（期間新的標記仍以舊的 grace 計）。新 repo 要用較短的 grace，在 `kist init --grace` 給（`--replicas`、
  `--pack-target`、`--mpack-target` 也可以在 init 給）：init 之後再調低就是排空，72 小時內的標記都照 72 小時算。
- `prune` 的選項：`--repack-below`（預設 50%）：活資料比例低於它的舊 pack 會被重新打包；損壞而 parity 修得回
  的 pack 一律重新打包，壞的那份之後照常刪掉。`--read-data`：另外把每個活的資料 pack 整份讀回、逐一驗證它持有
  的 chunk（慢：下載整個 repo 的資料），壞而 parity 修得回的重新打包、修不回的從記錄移除並回報（它若是唯一的
  一份，下一輪 prune 會拒絕，直到 forget 掉需要它的 snapshot）；時間用掉一半就停、下次接著讀。平常的 prune 只
  讀它要用的 metadata 與要刪的 pack，稠密的活資料 pack 裡的位元損壞要靠它修；`check --read-data` 回報
  `repairable` 之後接著跑它。沒有 `--dry-run`。
- 刪除只看 prune 自己讀過、驗證過的東西：備份主機寫的提示（delta）只用來加速去重，左右不了刪什麼。
- 刪不掉的物件（S3 Object Lock、權限）會回報並留到下一輪再試；一輪超過時限也會停下來、下一輪接著做。
  兩種都是結束碼 3。
- 同一台機器同時只能跑一個 backup（client id 檔旁邊有鎖），也一次只能跑一個維護命令（prune、rebuild、
  settings、remove-unknown-packs、key remove；鎖檔在 repo 的本地快取目錄裡）。
- 最新世代的 map 分片遺失或壞掉時用 `kist rebuild`：重建一份 map、不刪任何東西，之後一段時間（hold）不刪。
- 較新版本的 kist 寫的、這一版看不懂的 pack，prune 不使用也不刪（每輪回報）；確定沒有較新的 kist 在用這個
  repo 時，`kist remove-unknown-packs` 把它們標成待刪，之後照常兩階段刪除。
- `rclone://` 的後端不能跑 prune、rebuild、settings、remove-unknown-packs 與 `check --repair`（條件建立不是
  原子的，柵欄靠不住）；backup、restore、forget、key 照常。
- bucket 有 versioning 時，真的釋放空間還需要 lifecycle 規則清掉舊版本。

本地 index 快取放在使用者快取目錄（Linux：`~/.cache/kist/`），可用 `--cache-dir` /
`KIST_CACHE_DIR` 指定、`--no-cache` 關閉。`check` 與維護命令永遠不用快取。

### S3 / MinIO

```sh
export KIST_REPO=s3://my-bucket/backups/laptop
export AWS_ACCESS_KEY_ID=... AWS_SECRET_ACCESS_KEY=... AWS_DEFAULT_REGION=us-east-1
# MinIO 或其他自架的 S3 相容服務：
export AWS_ENDPOINT=http://minio.local:9000 AWS_ALLOW_HTTP=true
kist init
```

v4 不覆寫任何物件，`config` 也只在 `init` 時寫一次：bucket policy 可以對備份角色只開「新增」。backup 需要的
權限是 `PutObject`（建議以 `s3:if-none-match` 強制只在不存在時建立）、`GetObject`、`ListBucket`（不需要
`DeleteObject`）；每台備份主機的物件都在自己的前綴下（`packs/<client>/`、`mpacks/<client>/`、
`snapshots/<client>/`……），可以讓每台主機的憑證只能寫自己的前綴、讀全部。寫 `config`、`keys/`、`maint/` 與
`DeleteObject` 只給 `init` 與維護角色的憑證（`prune`、`forget`、`key remove` 要刪；`rebuild`、`settings`、
`remove-unknown-packs` 要寫 `maint/`；`check --repair` 要寫遺失的副本），建議用另一組憑證在別台機器跑。建議的
policy 見 [docs/format-v4.md §1](docs/format-v4.md)。

每台機器第一次 backup 時會產生一個 client id（`~/.local/share/kist/client-id`，
可用 `--client-id-file` 或 `KIST_CLIENT_ID_FILE` 指定）。

### SFTP

```sh
export KIST_REPO=sftp://user@backup.example.com/srv/backups/laptop
# 認證：SSH agent → key 檔 → 密碼，擇一：
export KIST_SFTP_KEY=~/.ssh/id_ed25519            # 可再加 KIST_SFTP_KEY_PASSPHRASE
export KIST_SFTP_PASSWORD=...                     # 或 SSH_AUTH_SOCK 的 agent
kist init
```

Host key **嚴格驗證**：只接受 `~/.ssh/known_hosts`（或 `KIST_SFTP_KNOWN_HOSTS`）裡有的
key，不做 TOFU——先用 `ssh user@host` 連一次把 key 記進 known_hosts。伺服器必須支援
`hardlink@openssh.com` 與 `posix-rename@openssh.com`（OpenSSH 的 sftp-server 都有；
rclone 的 `serve sftp` 會在連線後明確拒絕，見下節）。

### 遠端來源（`kist backup sftp://…`、`s3://…`）

```sh
kist backup sftp://me@nas.local/srv/data           # 連線設定同 SFTP repo：KIST_SFTP_KEY／PASSWORD／KNOWN_HOSTS、agent
kist backup s3://bucket/photos                     # AWS_* 同 S3 repo（AWS_ENDPOINT 給 MinIO）
kist backup sftp://me@nas.local/srv/data --trust-mtime   # 選用：大小與 mtime 沒變的檔案不讀（見下）
```

一次 backup 一個遠端來源（URL 必須是唯一的路徑）；設定檔的 `[backup] paths` 也可以是一個 URL。client 是轉運：讀遠端、
本機切塊加密，再寫進 repo（repo 也可以是遠端的，就是 remote-to-remote）。讀取：一個目錄裡最多同時讀 17 個檔案
（不大於 1 MiB 的才先開始讀），SFTP 的一個檔案同時有 2 MiB 的 READ 在等；記憶體不跟著檔案大小長。SFTP 來源不記符號連結：
連到檔案的跟過去讀目標的內容（大小與 mtime 也記目標的），連到目錄的與斷掉的記成略過的項目（backup 以 3 結束）。

沒變的檔案：`s3://` 以 etag 與大小判斷，沒變的物件不讀。`sftp://` 預設**每次整份讀**（去重讓 repo 不多存，但每個
byte 都要經過網路）：SFTP 給的只有大小與 mtime，兩個都是寫檔案的人設得了的（`touch -d`、`cp -p`）。`--trust-mtime`
（設定檔 `trust_mtime = true`）改成信任它們：大小與 mtime 都和上一次相同、mtime 那一秒早於上一次 backup 開始的
檔案沿用上一次的內容。代價是內容變了、大小與 mtime 都沒變的檔案要到下一次整份讀才備份到；伺服器的時鐘比這台慢
多少，同一秒裡又改的判斷就弱多少。所以這台機器上一次整份讀這個來源超過 `--reread-every`（預設 7d）、沒有紀錄（第一次、
`--no-cache`、換了機器）或 parent 比那次舊，就整份讀一次；摘要的最後一行印出這次是哪一種與原因。整份讀的紀錄在本機
快取裡，不在 repo 裡。

大的 SFTP 樹也可以把 kist 裝在 SFTP 那台機器上，直接備份它的本機路徑：posix 的快速路徑（大小、mtime、ctime、inode，
kernel 維護）不必整份讀，也不經過轉運；代價是那台機器要有 repo 的密碼。

### 瀏覽 snapshot：`kist mount`（Linux/macOS）

```sh
kist mount --repo $KIST_REPO /mnt/snapshots   # 目標目錄需已存在且為空
ls /mnt/snapshots/<client id>/<timestamp>/    # 備份的樹，唯讀
cp /mnt/snapshots/<client>/<ts>/tmp/…/report.pdf .
# Ctrl-C（或 SIGTERM）卸載
```

隨機讀直接定位 chunk（不解前面資料），適合翻大檔、比對、拷貝少數檔案；整目錄
還原請用 `restore`（較快）。檔案進 kernel page cache，翻過一次第二次就快。
掛載時有檔案開著 → 卸載 EBUSY。細節與限制見
[ADR 015](docs/decisions/015-mount-fuse.md)。

### rclone 橋接（任何 rclone 認得的遠端）

```sh
rclone config                                     # 設定好遠端，例如 gdrive
export KIST_REPO=rclone://gdrive/backups/laptop   # 遠端留空 = 本機目錄：rclone:///srv/backups
kist init
```

kist 自己 spawn `rclone serve sftp --stdio <remote>:<path>`（restic 同款做法）：不開
TCP port、不用 known_hosts，rclone 的設定就是全部。`KIST_RCLONE_BIN` 可指定 rclone
路徑。**語意比 `sftp://` 寬鬆**：rclone 不實做 hardlink 與 O_EXCL 建檔，`put_if_absent`
退化成「先 stat 再 posix-rename」；kist 用它的 key 都是「同 key 必同內容」或一次性寫入
（init 另有讀回驗證擋雙重 init），風險分析見
[ADR 014](docs/decisions/014-rclone-bridge.md)。`sftp://` 維持嚴格語意不變。v4 的維護命令（prune、rebuild、
settings、remove-unknown-packs、`check --repair`）在 `rclone://` 上直接拒絕：世代柵欄要靠原子的條件建立；
backup、restore、forget、key 照常。

## 開發

四道關卡（與 `ci-rust.yml` 同款；該 workflow 還含 MinIO 整合測試與 fuzz smoke，
但只開 `workflow_dispatch` 手動觸發，避免吃配額——本機 gate 是平時的準則）：

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo deny check          # 需先 cargo install --locked cargo-deny
```

S3 整合測試預設略過；起一個 MinIO 容器並設環境變數就會跑（見 `tests/README.md`）。

格式相容性：v4 的 fixture repo（`crates/kist-core/tests/fixtures/v4/repo`，提交在 git 裡、**永不重生**；
`crates/kist-core/tests/v4_fixture.rs` 驗它打得開、check 乾淨、還原得出來、每個物件解碼再編碼逐 byte 相同、同樣的
來源不寫新 chunk、parity 與 `.r1` 修得回）。golden 與向量（`crates/kist-format/tests/golden_v4.rs`、
`scripts/v4-vectors`）會隨程式重錄，fixture 不會；它只在格式升版時換（見
[ADR 018](docs/decisions/018-remove-go-reference.md)）。`fuzz/` 的四個 target 對準 v4 的讀取端（`fuzz/README.md`）。

## Workspace 結構

| crate | 職責 |
| --- | --- |
| `kist-format` | on-disk 格式：結構定義、CBOR 序列化、golden files（改動需負責人確認）；`v4` 模組是現行格式，根下只剩版本無關的 cbor、ids、chunker、parity |
| `kist-crypto` | 金鑰階層（Argon2id → KEK → master key → 派生子金鑰）與 XChaCha20-Poly1305 封裝 |
| `kist-chunker` | 內容定義切塊（FastCDC，512 KiB / 2 MiB / 8 MiB） |
| `kist-backend` | 儲存後端（本機、S3、SFTP、rclone 橋接）與遠端來源（Source 抽象）|
| `kist-core` | backup / restore / check / forget / prune / rebuild / settings / key 流程（`kist_core::v4`）與本地快取 |
| `kist-app` | 應用層：設定檔、排程、工作執行、通知、Prometheus metrics 與 Web UI |
| `kist-mount` | 唯讀 FUSE 掛載（`kist mount`） |
| `kist-cli` | `kist` 執行檔（clap） |

格式規格：[docs/format-v4.md](docs/format-v4.md)（v3 的 `docs/format.md` 已刪，`git show 0b3d425:docs/format.md`
取回）。設計決策：[docs/decisions/](docs/decisions/)。

## 驗收數據（v4，2026-10-18／19）

同一台機器（Ryzen 7 7700、NVMe/ext4）、與 v3 時代同一份資料集（100 000 個檔案、9.75 GiB，一半亂數一半可壓縮，
`tests/acceptance/gen.py`）、release build。M1 與 M2 的表是 2026-10-19 以 `b48e007`（沒變的 backup 不再每個檔案換一次
執行緒、每個條目只 lstat 一次之後）重跑的，log 是 `tests/acceptance/*-v4-2026-10-19.log`；10-18 以 `2e6b20b` 跑的
`*-v4-2026-10-18.log` 也留著。M3 的 GC 是 10-18 以 `9acbafb` 跑的。100 萬檔的記憶體 bench 在 `bench/memory/README.md`：
backup、prune、check、restore 在兩個 100 萬檔的測試集上都在 512 MiB 門檻內（2026-10-19，`b48e007`；prune 與單一平面
目錄的 restore 原本超過，那天修掉，見該處與 ADR 026「實作第七段之二」），最高的是 `check --read-data`（441 MiB）。對照欄是下一節 v3 時代的數字。

### M1（本機目錄後端）

| 項目 | v4 | v3（2026-09-05） |
| --- | --- | --- |
| 第一次 backup | 33 s；77 個 pack、2 個 mpack 物件（repo 4.90 GiB）。10-18 是 57 s；這次來源在 page cache 裡（前面在同一份資料上量過幾次），差距不歸因於程式 | 54 s；77 個 pack |
| 第二次 backup（內容未變） | **1.1 s**；**0 個新 pack、0 個新 chunk、0 個新 mpack**（100 000 個檔案全走快速路徑，1 102 個目錄全沿用 parent 的子樹）。10-18 是 2.8 s：每個檔案一次執行緒的來回與兩次 lstat（ADR 026「實作第七段之二」） | 0.8 s |
| restore | 31 s；`diff -r` **完全相同** | 41 s |
| `check` | 0.7 s | 0.3 s |
| `check --read-data` | 17.4 s | 17 s |
| 人為翻轉 pack 中一個 bit | `check --read-data` 以非 0 結束並指名該 pack | 同 |
| 峰值 RSS（backup 期間） | 192 MiB | 186 MiB |

### M2（S3 / MinIO）

repo 放在本機 Docker 裡的 MinIO（`minio/minio:RELEASE.2025-09-07`，HTTP），腳本 `tests/acceptance/run_s3.py`；
請求數由容器內的 `mc admin trace --json` 數、依 API 分類，含 LIST（10-18 記的 9／74／6 漏數了 LIST：trace 裡 LIST 的
path 是 bucket 本身、repo 的前綴在 query，10-19 修正）。

| 項目 | v4 | v3 |
| --- | --- | --- |
| 第一次 backup | 30 s；77 個 pack、1 個 mpack 物件（S3 上 mpack 預設不寫 `.r1`） | 157 s |
| 第二次 backup（內容未變，pack 還只在 delta 裡） | 1.1 s；0 個新 pack；**13 個 S3 請求**（6 GET、2 HEAD、4 LIST、1 PUT） | 35 s（每次重新 put 1 102 個 tree） |
| restore | 88 s；`diff -r` **完全相同** | 213 s |
| `check` | 0.5 s | 2 s |
| `check --read-data` | 9.5 s | 28 s |
| `prune` 第一輪（收錄 78 個 pack 進 map，不刪東西） | 0.7 s；之後 `check` 0.5 s | `rebuild-index` 0.4 s |
| 收錄進 map 之後、內容未變的 backup | 新的一代第一次 76 個請求（70 GET：那一代的 66 片 map 分片第一次進本地快取，加每次都有的 config、金鑰槽、head、parent snapshot；1 HEAD、4 LIST、1 PUT）；快取有了之後 **10 個**（4 GET、1 HEAD、4 LIST、1 PUT） | — |
| 人為翻轉 pack 中一個 bit（透過 `mc` 上傳） | `check --read-data` 以非 0 結束並指名該 pack | 同 |
| 峰值 RSS（backup 期間） | 188 MiB | 242 MiB |

內容未變的 backup 的請求數與目錄數無關；新的一代的一次性成本上限是 256 片分片加 head。4 個 LIST 是 B1 的 head 與
delta、找 parent 的自己的 snapshot、B4 的 head。restore 對 S3 仍比本機慢（88 s 對 31 s；10-19 的前一次跑是 99 s），差距
的原因沒有 profile（UNVERIFIED）。

### Remote to remote（遠端來源寫進 S3 repo，2026-10-19）

同一份資料集放進本機 Docker 裡的 SFTP（OpenSSH 10.3p1）與 MinIO，各自以 `kist backup sftp://…`、`kist backup s3://…`
備份進 MinIO 上的一個 S3 repo（腳本 `tests/acceptance/run_r2r.py`，log `tests/acceptance/r2r-acceptance-2026-10-19.log`，
`7cf9e2d`）。兩個容器都在本機、網路延遲幾乎是零：真實網路上的時間沒有量。

| 項目 | SFTP 來源 | S3 來源 |
| --- | --- | --- |
| 第一次 backup | 50.5 s，峰值 RSS 275 MiB | 42.7 s，231 MiB；來源 100 000 個 GET、1 102 個 LIST |
| 沒變的第二次 | 45.8 s：預設每次整份重讀（去重，0 個新 pack） | 17.7 s：etag 與大小，全部沿用；1 102 個 LIST |
| 沒變的，`--trust-mtime` | **2.9 s**：大小與 mtime，全部沿用 | — |
| restore 到本機 | 86.5 s，`diff -r` **完全相同** | 79.5 s，`diff -r` **完全相同** |
| 沒變的 backup 對 repo 的請求 | 13 | 13 |

平面 100 萬項（記憶體 bench 的 B 集）的第一次 backup 在 512 MiB 的門檻內：SFTP 398 MiB、S3 465 MiB（本機路徑 394 MiB；
`bench/memory/run-remote.sh`，數字是門檻跑法；見 ADR 026「實作第七段之三」）。S3 的那一個離門檻最近。

### M3（GC，真實時鐘）

同一份資料集、本機目錄後端，腳本 `tests/acceptance/run_gc.py`，log `tests/acceptance/m3-acceptance-gc-v4-2026-10-18.log`
（binary 是 `9acbafb`）。流程：backup → 刪掉一成的目錄再 backup → forget 舊 snapshot → prune（收錄）→ 等一個 grace
（設 2 小時，v4 的最低值）→ prune 兩輪（第一輪只把「現在的 T」推到現在，第二輪標記與 repack）→ 再等一個 grace →
prune 兩輪（第二輪 tombstone 並刪除）→ 再一輪（沒事可做）→ `check --read-data` → restore 與 `diff -r`。

| 項目 | v4 | v3（`prune --grace 0s`，2026-09-05） |
| --- | --- | --- |
| backup 1／backup 2（少一成） | 44 s／2 s | — |
| prune 1（收錄 77 個 pack 與 2 個 mpack 進 map） | 1 s | — |
| grace 之後的第一輪（只推進 T） | 0 s，0 個標記 | — |
| 第二輪（標記 0 個 pack、repack 8 個） | 1 s | 3.3 s（標記 112 個 tree + 8 個 pack） |
| 再一個 grace 之後的第一輪 | 0 s，不刪 | — |
| 第二輪（tombstone 並刪 8 個 pack、521 MiB） | 0 s | 1.1 s（刪 120 個物件、520 MiB） |
| 之後一輪 | 0 s，無事可做 | 1.1 s |
| repo 大小 | 4.90 GiB → 4.93 GiB（標記後，沒刪）→ **4.42 GiB** | 4.90 → 4.41 GiB |
| `check --read-data` 之後 | 8 s，無錯誤 | 26 s |
| restore 最新 snapshot | 17 s，`diff -r` **完全相同** | 57 s |
| 峰值 RSS（整個流程） | 230 MiB | 233 MiB |

這是第三次跑。第一次的腳本每個階段只跑一輪 prune、在標記那裡斷言失敗——v4 的時間只比 head（§11.1），一輪的「現在的 T」
是它讀到的最新 head 的時間，等過一個 grace 之後第一輪只是寫一個把 T 推到現在的 head。第二次斷言 `condemned > 0` 又失敗：
這份資料集刪掉的目錄的 pack 都還剩幾個跨目錄相同的小檔的 chunk，走的是 repack（舊 pack 一樣標成 condemned，數在
`repacked`）；同一次跑也看到 init 之後 `settings --grace 2h` 開始了 72 小時的排空，期間的標記照 72 小時算、刪除階段
永遠到不了——所以加了 `kist init --grace`。兩次錯的都是腳本，不是 GC；但「新 repo 想用短的 grace」是 kist 的用法缺口。
grace 設 2 小時只是為了驗收能在一個晚上走完；實際使用請保留預設 72 小時（每天一輪 prune 時，從沒人用到刪掉推算約 8 天）。

## 驗收數據（v2／v3 時代，2026-09-05）

以下三節的數字量的是 2026-09-05 的 v2／v3 實作（`rebuild-index`、tree 物件、`prune --grace` 都已不存在），留作對照。

### M1 驗收數據

2026-09-05，commit `26637f0`（審查修正後），同一台機器（Ryzen 7 7700、NVMe/btrfs，
本機目錄後端），release build。資料集：100 000 個檔案、9.75 GiB（一半亂數、一半可壓縮），
腳本與完整 log 在 `tests/acceptance/`。

| 項目 | 結果 |
| --- | --- |
| 第一次 backup | 54 s；寫 77 個 pack（repo 4.90 GiB，可壓縮的那一半被壓掉了） |
| 第二次 backup（內容未變） | 0.8 s；**0 個新 pack、0 個新 chunk**（含重新 put 1 102 個 tree） |
| restore | 41 s；`diff -r` 與原始資料**完全相同** |
| `check` | 0.3 s |
| `check --read-data` | 17 s |
| 人為翻轉 pack 中一個 bit | `check --read-data` 以非 0 結束並指名該 pack |
| 峰值 RSS（backup 期間，`ru_maxrss`） | **186 MiB**（審查修正前 253 MiB） |
| 512 MiB 單一大檔的 RSS 成長（reviewer 的 probe） | 22 MiB（修正前 736 MiB） |

### M2 驗收數據（S3 / MinIO）

2026-09-05，commit `09f6f82`，同一台機器、同一份資料集，repo 放在本機 Docker 裡的 MinIO
（`minio/minio:RELEASE.2025-09-07`，HTTP、無 TLS），腳本 `tests/acceptance/run_s3.py`，
完整 log `tests/acceptance/m2-acceptance-s3-2026-09-05.log`。

| 項目 | 結果 | 對照本機後端 |
| --- | --- | --- |
| 第一次 backup | 157 s；77 個 pack（4.90 GiB） | 54 s |
| 第二次 backup（內容未變） | 35 s；**0 個新 pack、0 個新 chunk** | 0.8 s |
| restore | 213 s；`diff -r` **完全相同** | 41 s |
| `check` | 2 s | 0.3 s |
| `check --read-data` | 28 s | 17 s |
| `rebuild-index`（77 個 pack，只讀 trailer） | 0.4 s；再 `check` 通過 | — |
| 人為翻轉 pack 中一個 bit（透過 `mc` 上傳） | `check --read-data` 以非 0 結束並指名該 pack | 同 |
| 峰值 RSS（backup 期間） | **242 MiB** | 186 MiB |

第二次 backup 與 restore 對 S3 明顯慢：前者每次仍 put 全部 1 102 個 tree 並逐個讀 parent 的 tree，
後者對每個 chunk 各發一次 range GET（71 040 次）。
UNVERIFIED（未做 profile，只是從請求數推測）。兩者都列在 M3/M4 待辦（ADR 004「沒做」）。

### M3 驗收數據（GC）

2026-09-05，commit `0d0e107`（兩輪審查修正後），本機目錄後端，同一份資料集，腳本 `tests/acceptance/run_gc.py`，
完整 log `tests/acceptance/m3-acceptance-gc-2026-09-05.log`。流程：backup → 刪掉一成的目錄再 backup →
forget 舊 snapshot → `prune --grace 0s`（標記 + repack）→ backup → prune（刪）→ 再兩輪 backup + prune 收尾
→ `check --read-data` → restore 與 `diff -r`。

| 項目 | 結果 |
| --- | --- |
| prune 1（標記 112 個 tree + 8 個被 repack 的 pack，搬 18 MiB 活資料到 1 個新 pack） | 3.3 s |
| 標記後的 backup（內容未變） | 5 s；**0 個新 chunk**（不會把被標記 pack 裡的資料重傳） |
| prune 2（刪 120 個物件、520 MiB） | 1.1 s |
| prune 3–4（清掉被取代的 index blob，之後無事可做） | 1.1 s 各 |
| repo 大小 | 4.90 GiB → **4.41 GiB**（刪掉的一成資料全部回收） |
| `check --read-data` 之後 | 26 s，無錯誤、無警告 |
| restore 最新 snapshot | 57 s，`diff -r` **完全相同** |
| 峰值 RSS（整個流程） | 233 MiB |
| 競態 proptest（`tests/gc_race.rs`） | 24 案例進 `cargo test`；200 案例本機跑（見 ADR 005 §10） |

grace 設 0 只是為了驗收能在幾分鐘內走完；實際使用請保留預設 72 h。

## Clean build 時間

機器：AMD Ryzen 7 7700（8C/16T）、Linux 7.2.0、rustc 1.98.1、cargo 1.98.1。
`cargo clean` 之後、相依套件已在本機 cargo 快取中（不含下載時間）：

| 階段 | `cargo build` | `cargo build --release` |
| --- | --- | --- |
| M0 骨架（只有 clap） | 3.9 s | 3.5 s |
| M1（完整相依） | 8.7 s | 15.8 s |

（當時 Rust 的 workflow 只開手動觸發、至今未實際執行過；量測皆在本機。）

## 授權

MIT OR Apache-2.0（你可以任選其一）。見 [LICENSE-MIT](LICENSE-MIT) 與
[LICENSE-APACHE](LICENSE-APACHE)。
