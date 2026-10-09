# ADR 022：只還原一個子目錄、restore 的 dry run

日期：2026-10-10　狀態：已採用

## 背景

ADR 020 的部分還原保留原本的版面：檔案一律落在 `<目標>/<原本的路徑>`。常見的另一種需求是「把這個目錄還原
到那裡」——restic 寫成 `restore latest:/home/me/docs --target /tmp/docs`。另一個缺口是部分還原前想先看會寫
哪些東西。這份補上 `SNAPSHOT:PATH`、`--dry-run`，以及 restore 的樣式檔。格式不變。

## 決定

### 1. `SNAPSHOT:PATH`：目錄的內容直接放進目標

- `RestoreOptions.subtree` 是 snapshot 裡一個目錄的段（`subtree_components` 切，與樣式同一套路徑空間，`/`
  開頭；`.`／`..` 報錯）。空的＝整份還原的版面，所以舊的行為就是 subtree 為空的特例。
- 每個 root 的落點由一個函式算（`place_root`，真的還原與 dry run 共用）：
  - subtree 是 root 落點或它的上層 → 落點是去掉 subtree 之後的部分（幾個 root 的共同上層也行）；
  - subtree 在 root 裡面 → 沿 tree 走到那個目錄，它的內容放進目標本身；
  - 兩者都不是 → 這個 root 跟這次還原無關。
  沒有任何 root 跟 subtree 有關 → 結束碼 1「不在這個 snapshot 裡」；路徑上遇到的不是目錄 → 「不是目錄」。
- 樣式照樣比對 snapshot 裡的完整路徑，不是相對於 subtree（restic 的文件說它的樣式在這種寫法下相對於
  子目錄；kist 維持 ADR 020「一套路徑空間」，backup、restore、mount 寫的路徑都同一種）。
- 目標本身不套那個目錄的 metadata：目標是使用者自己的路徑，與整份還原時目標本身不被動一致。

### 2. `--dry-run`：與真的還原共用落點與選取的另一條走訪

- `Repository::plan_restore` 走同樣的 `place_root` 與 `Select::visit`，但不碰檔案系統（連目標都不建），
  把每個會被還原的節點交給 callback（目錄在它的內容之前）；回傳的 `RestoreSummary` 與真的還原同形狀。
- 沒有把 dry run 做成真的還原裡的一個旗標：那條路徑每一步都在建目錄、開 handle、寫暫存檔，加旗標等於每
  一步都要記得「這次不寫」，漏一處就是 dry run 寫了東西。另一條只讀 tree 的走訪，結構上就不可能寫入；
  代價是兩條走訪要保持一致，由測試把關。
- 一致性的證據：測試對八組選取（整份、單檔、目錄、不錨定、只有 exclude、include＋exclude、沒選到的
  include、subtree）各跑一次 dry run 與真的還原，比對計數、`unmatched_includes` 與路徑集合，逐一相同。
  負向對照：讓 dry run 把走過的目錄都計數（不管有沒有建）→ 這個測試失敗（6 對 2）。
- 只讀 tree、不讀檔案內容：壞掉的 chunk、目標處擋著的東西這類錯誤，dry run 看不到。
- CLI：文字輸出一行一個 `dir`／`file`／`link` 加路徑，最後一行 `would restore …`；`--json` 只輸出計數，
  不列路徑（大樹的路徑清單不適合塞進一個 JSON），並多一個 `dry_run` 欄位（真的還原時是 false）。

### 3. restore 的樣式檔

`--include-file`、`--exclude-file`，與 `backup --exclude-file` 共用同一個讀檔函式（一行一條、去前後空白、
空行與 `#` 開頭略過、錯誤點名 `檔案:行號`）。

## 後果

- 公開介面：`RestoreOptions.subtree`、`restore::subtree_components`、`Repository::plan_restore`、`Planned`。
- `restore --json` 多一個 `dry_run` 欄位。
- 沒量測：dry run 在大 snapshot 上的時間（它讀的 tree 與真的還原一樣多）。
