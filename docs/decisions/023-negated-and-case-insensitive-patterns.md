# ADR 023：反向樣式與不分大小寫的樣式

日期：2026-10-10　狀態：已採用

## 背景

ADR 020 的樣式不收 `!`（直接報錯），也只有區分大小寫的比對。restic 兩者都有：文件說 `!` 開頭的樣式
「比對到先前被一般樣式比對到的檔案」時把它加回來，「與 gitignore 相似，限制也相同：目錄一旦被排除，
就不能把裡面的檔案加回來」，例子是 `$HOME/*` 加上 `!$HOME/Documents`；`--iexclude` 是「同 `--exclude`
但不分大小寫」。這份補上兩者。格式不變。

## 決定

### 1. 反向樣式：一個節點上最後一個比對到的樣式決定

- 樣式依加入的順序編號。走到一個節點時，比對到它的樣式裡編號最大的那條決定：不是反向的＝比對到，反向的
  ＝取消（`FilterState::decision`，沒有樣式比對到時是 `None`）。
- 沒有樣式比對到的節點怎麼算，由用的地方決定：
  - backup 的排除、restore 的排除：不排除。被排除的目錄整棵不走，所以「目錄被排除、裡面再加回來」不會
    發生——就是 restic 文件寫的那個限制。
  - restore 的 include：沿用上一層（選到的目錄底下全部選到；`!` 取消的目錄底下全部不選）。
- 只看比對到「這個節點」的樣式，不看比對到上層的：與 gitignore 相同。restic 的實作在這種邊角上是否一樣
  沒有查證（UNVERIFIED）。
- 剪枝：反向樣式只會取消，所以「底下還可能比對到」（`may_match_below`）只看不是反向的樣式。proptest 的
  剪枝性質跟著改成：決定是「比對到」的節點，它的每個真前綴都還活著。
- 「沒選到東西」的 include 檢查（ADR 020）不看反向樣式。
- 順序來自 CLI：先是區分大小寫的（旗標的值、再來樣式檔的行），然後是不分大小寫的。clap 分開收集不同
  旗標的值，所以 `--exclude` 與 `--exclude-file` 交錯寫在命令列上也是這個順序（README 寫明）。

### 2. 不分大小寫

- 每條樣式自己帶旗標（`PathFilter::and_ignoring_case`），所以兩種可以混在同一組、共用編號與順序。
- 比對：字元兩邊的小寫相同就算（`char::to_lowercase`，一對多的對應也照比）；`[...]` 看字元本身與它的大小寫
  形式；不成 UTF-8 的 byte 不折。不分大小寫的樣式沒有逐 byte 的字面段，一律走萬用段的比對。

### 3. 驗證

- 單元測試：反向的順序規則、只有反向樣式時不剪枝、不分大小寫的字面／清單／非 ASCII／byte。
- proptest：樣式隨機帶 `!` 與不分大小寫，與參考實作比對每個前綴的「比對到哪些樣式」與「決定」，並驗剪枝
  性質。開發途中它抓到一個真 bug：不錨定的反向樣式在解析時沒有去掉 `!`（`!keep.tmp` 變成字面的
  `!keep.tmp`）。
- restore 的 include 改成「沒有樣式比對到就沿用上一層」之後，把舊規則放回去，反向 include 的測試就失敗
  （docs/old 被還原）。

## 後果

- 以前直接報錯的 `!xxx` 現在是反向樣式；字面的 `!` 開頭要寫 `\!`（以前就是這樣寫）。
- 公開介面：`PathFilter::and`、`and_ignoring_case`、`is_negated`，`FilterState::decision`；
  `FilterState::is_match` 改成「決定是比對到」。
- 新旗標：backup `--iexclude`、`--iexclude-file`；restore `--iinclude`、`--iinclude-file`、`--iexclude`、
  `--iexclude-file`；設定檔 `[backup] iexclude`。
