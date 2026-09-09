# 010 — Parity：明文 sidecar 與以名字為證的修復

- 狀態：已接受（M5）
- 日期：2026-09-05
- 相關：`internal/parity/`、`internal/pack/writer.go`、`internal/repo/check.go`、[docs/format.md §13](../format.md)

## 問題

`PLAN.md` M5：Reed-Solomon 可選開啟。格式 v1 凍結了——parity 進不了 pack 本身。

## 決策

### 1. Sidecar，不進 pack

`parity/<packID>`，對密封後的整個 pack 算。pack 位元組一個都不動，v1 的 golden 全部不變。

### 2. 開關只在寫入端，repo 裡沒有狀態

`backup --parity M` / `[repository] parity`。config 是 v1 的明文 CBOR、strict decoder、也是唯一允許改寫的物件——不為這個加欄位。混合的機群（有的 client 開、有的不開）不需要協調；`check` 回報覆蓋率，缺 parity 不是問題。

### 3. 明文，而且這是性質不是偷懶

- 密文上的 RS 是偽隨機位元組的線性組合，不洩漏。
- shard hash 是密文的 hash。
- 修復的正確性證明是 `CiphertextID(重建結果) == packID`。名字就是 checksum。

結論：偽造或損壞的 parity 只能讓修復失敗，不能讓它錯誤地成功（`TestForgedParityCannotRepairWrongly` 連「hash 剛好對上損壞 shard」的情況都試了）。副產品：不持密碼的 scrub 工具可以在儲存端修 pack。

### 4. 固定幾何：K=16，M ∈ 1..8，預設建議 2

固定 K 讓開銷就是 M/16、reader 不需要協商。`shard_len = ceil(pack_size/16)`，最後一個 shard 補零。parse 先驗完所有界限再配置記憶體（`FuzzParse` 守著）。

一個真 bug：下界原本寫成 `pack_size > shard_len × 15`，178 bytes 的 pack（shard_len 12，12×15=180）被判「header 自相矛盾」，`check --repair` 回報 parity corrupt。CLI 測試抓到的；改成 `ceil(pack_size/16) == shard_len`，`TestEncodeAndParseAtAwkwardSizes` 掃過邊界附近的尺寸。

### 5. 寫入：pack 上傳成功之後、spool 還在時算

`pack.Writer.Finish` 上傳完再從 spool 算 parity、`PutIfAbsent`。parity 寫失敗是**警告**不是備份失敗：資料已經安全，缺的是冗餘。

### 6. 修復用 `Put`，而且這是 `Put` 第一個正式的呼叫者

Delete + PutIfAbsent 會留一個沒有 pack 的窗，Object Lock 底下 delete 也不會真的刪。`Put` 在三個後端都是原子替換（rename-over、PutObject、posix-rename），在版本控制 bucket 上是新版本。`backend.Backend.Put` 的註解和 format.md 不變條件 2 都改成點名兩個例外：config 改寫、以及寫回經 hash 證明與名字相符的 pack。寫完再 `VerifyAll`。

### 7. 檢測不新增：`VerifyAll` 本來就把整個 pack hash 回名字

trailer 讀不出來、chunk AEAD 失敗、整體 hash 不對——全部是既有的失敗點；`--repair` 只是在那些點上多試一次。

## 測試

parity 套件：決定性 golden、≤M 修回逐位元組相同（body、同 shard 兩處、trailer、首尾、截斷、多出來）、>M 拒絕、偽造拒絕、header 矛盾拒絕、fuzz。repo：`check --repair` 修回 + 再 check 乾淨 + restore；太多損壞 / 沒 parity / 偽造 / parity 物件損壞四種都不動 pack；prune 連帶刪、孤兒清。S3：backup 憑證能寫 parity 不能刪；lock bucket 上 repair 成功並讀回相同。

## 沒做的

串流 RS（64 MiB pack 整個進記憶體 + 8 MiB parity，對 1 GiB 的上限沒有壓力；量到再說）。tree / snapshot / index blob 的 parity。
