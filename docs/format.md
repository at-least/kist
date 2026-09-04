# kist 儲存格式

> **狀態：未定案。** M0 只建立骨架，格式在 M1 結束時凍結，之後只能透過每個物件的 `version` 欄位演進。
> 這份文件在 M1 過程中隨著實作長出來，凍結時就是規範本身。

## 目前確定的部分

以下取自 `PLAN.md`，是 M1 的設計輸入，尚未有實作與 golden file 佐證。

repo 是一個 key-value 命名空間，所有物件不可變、以內容 hash 命名：

| Key                         | 內容 |
| --------------------------- | ---- |
| `config`                    | repo 參數、加密 key 封裝（KEK 包 master key） |
| `keys/<id>`                 | 額外 key slot（多密碼 / 還原 key） |
| `packs/<hash>`              | pack file：`[encrypted chunk]*` + encrypted trailer index + trailer length(8B) + magic |
| `indexes/<hash>`            | index blob：chunkID → (packID, offset, length)，是快取，可從 packs 重建 |
| `trees/<hash>`              | 目錄物件（content-addressed，未變動的子樹整棵重用） |
| `snapshots/<clientID>/<ts>` | 快照指標：root tree hash、時間、host、paths、統計 |
| `gc/<packID>`               | 待刪標記（timestamp + 由誰標記） |

不變條件：

- pack 目標 64 MiB，寫滿或 backup 結束時 flush。
- chunk ID = keyed BLAKE3(master-derived hash key, plaintext)。
- 每個 chunk 獨立 AEAD（XChaCha20-Poly1305），nonce 隨機 24 B，AAD 帶 chunkID。
- snapshot 是 commit point：所有 pack 與 index 上傳完成後才寫 snapshot。
- Key 階層：password → Argon2id → KEK → 解開 master key → HKDF 派生 chunk key / hash key / index key。
- 所有 metadata 以 CBOR 序列化，且都帶 `version` 欄位。

## M0 沒有回答的問題

M1 動工前需要定案的項目列在給使用者的 M0 回報中（trailer 的實際欄位佈局、index blob 的產生粒度、`clientID` 的來源、local backend 上 `PutIfAbsent` 的原子性、tree 記哪些 metadata 等）。定案後回填到本文件，並各自留下一份 ADR。

## 相關決策

- [001 — 專案骨架、module path 與工具鏈基準](decisions/001-project-skeleton.md)
