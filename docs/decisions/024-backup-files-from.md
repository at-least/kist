# ADR 024：從清單讀來源路徑（`backup --files-from`）

日期：2026-10-10　狀態：已採用（切法由 ADR 025 修訂：原本說「照 restic」，實際有出入）

## 背景

restic 有三種從檔案讀來源路徑的旗標（文件原文）：`--files-from` 是「一行一個 *pattern*」，去掉前後空白、
略過空行與 `#` 開頭的行，每個 pattern 讀進來時展開；`--files-from-verbatim`「不展開特殊字元、不去空白、
不略過 `#`」，只略過空行；`--files-from-raw` 以 NUL 結尾。三種都把 `-` 當標準輸入。

## 決定

- 名稱、切法與 `-` 照 restic，都只在 CLI（設定檔的 `[backup] paths` 本來就是清單）。**ADR 025 修訂**：
  審查時拿 restic 0.19.1 對同一批檔案實跑，切法其實不同——verbatim 保留 CRLF 的 `\r`、raw 照收空名字與少了
  結尾 NUL 的清單、開頭的 UTF-8 BOM 沒去掉。現在的規則與刻意的差異見 ADR 025 與 README。
- **都不展開萬用字元**。kist 的萬用字元語法（ADR 020）是比對 snapshot 裡的路徑，不是在本機檔案系統上
  展開；另做一套檔案系統 glob 是第二種語法，容易和排除樣式混淆。列了 `*` 的行照字面找，不存在就像命令列
  給了不存在的路徑一樣以結束碼 1 結束——不會默默少備。
- 讀的是 bytes：unix 上名字可以不是 UTF-8；`--files-from-raw` 的名字裡可以有換行。非 unix 要求 UTF-8。
- 清單讀完一個路徑都沒有：結束碼 1（「no source paths」），不寫空的 snapshot。

## 驗證

CLI 測試：一般清單的去空白與註解（含 CRLF）並與命令列路徑合併、verbatim 保留結尾空白、raw 從標準輸入讀
含換行的名字、空清單與萬用字元行都是結束碼 1 且不留 snapshot。先紅後綠（旗標不存在時 clap 報
「unexpected argument」）。
