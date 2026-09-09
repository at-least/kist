# ADR 009：M5 parity——Reed-Solomon sidecar 的 Rust 寫入端與 repair

日期：2026-09-06

## 背景

格式（`docs/format.md` §14）裡 parity sidecar 從 Go v1 就定義好了，但只有
Go 實作（encode/repair），而且程式碼把物件版本寫成 `v:1`、規格寫 `v:2`，
兩邊沒對上（parity 是讀取端忽略的選配 sidecar，所以從未爆掉）。Rust 端
（產品本体）完全沒有 parity。

## 決策

1. **規格為權威**：Go 的 `parity.Version` 1→2 對齊 format.md，golden 向量
   重生；Rust 直接以 `v:2` 實作。
2. **RS 函式庫**：Rust 用選型文件指定的 `reed-solomon-erasure`（Backblaze
   JavaReedSolomon 的移植，純 Rust）。跨語言矩陣相容性**先做 PoC 再實作**：
   Go（klauspost/reedsolomon）與 Rust 對同一 pack 的 16+m 片做編碼，雙向
   「一邊寫、另一邊抹掉 2 片重建」，結果逐 byte 相同——兩者都是
   GF(2⁸)/0x11d 上的 Vandermonde→systematic 構造，跨實作修復可行。
3. **goldens 即跨語言向量**：Rust 的測試直接消費 Go 的 golden
   （`tests/testdata/parity-golden.txt`），pack 由同一 BLAKE3 XOF 流重現；
   Rust 的 encode 輸出與 Go 的 golden 物件**逐 byte 相同**。
4. **語意照抄 Go**：sidecar 用 `PutIfAbsent`（內容確定性，已存在 = 成功）；
   同位寫失敗只警告、不讓 backup 失敗（pack 已安全，缺的只是冗餘）；
   repair 的結果只在重算 BLAKE3 等於 pack 名稱時接受，所以偽造 parity
   永遠不會「修錯」，只會修不成。
5. **prune 清掃**：刪 pack 時 sidecar 一起刪（m=8 時 sidecar 是 pack 的一半
   大），並清掃之前死掉的 run 留下的孤兒 sidecar（與 Go gc.go 一致）。
6. **Object Lock**：`check --repair` 的重寫是整個系統唯一覆寫既有物件的
   動作；受鎖物件存不回去時回報為 unrepairable，不假裝成功。

## 影響

- `kist backup --parity N`（0..=8）與設定檔 `[backup] parity`。
- `kist check --repair`（隱含 `--read-data`）。
- 安全性假設不變：parity 是明文，但只讓修復失敗、不能修錯、不洩漏。

## 驗證

- Go `internal/parity` 全套 + golden 重生。
- Rust `kist-format` parity 測試（golden、awkward sizes、≤m 修復、>m 安全
  失敗、偽造 parity、proptest）。
- Rust `kist-core` E2E：`--parity 2` backup → 破壞 pack → check 回報 →
  `check --repair` 修復 → 還原逐 byte 相同；沒 parity 時只回報；
  3 片損壞 m=2 安全失敗；prune 連 sidecar 一起刪。
- 兩邊全套測試綠（Rust workspace 47 targets、Go 15 packages）。
