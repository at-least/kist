# 專案：一個現代化的開源備份工具（Go）

> 這份文件給 Claude Code 讀。請先讀完整份再開始，每個階段完成後回報並等我確認再進下一階段。
> 專案名 `kist`（蘇格蘭語「箱子、寶箱」），module path `github.com/<owner>/kist`。

## 定位
去重、加密、可多台機器共用 repo 的備份工具，對象是 object storage（S3 相容）優先，其次本機與 SFTP。
設計目標依序：資料安全性 > 還原可靠性 > 抗勒索 > 效能 > 功能數量。

競品參考：restic（穩定但有鎖、記憶體重）、Kopia（pack + 無鎖，最接近我們）、Duplicacy（無鎖 GC 但一 chunk 一檔）。
我們要的是 Kopia 的儲存效率 + Duplicacy 的無鎖 GC + 原生抗勒索設計。

## 技術選型（固定，不要改）
- Go 1.22+，單一 binary，禁止 cgo。
- CLI：`spf13/cobra`；設定檔：TOML（`BurntSushi/toml`）。
- Chunking：FastCDC（`github.com/jotfs/fastcdc-go`，或自行實作），min 512 KiB / avg 2 MiB / max 8 MiB。
- Hash：BLAKE3（`lukechampine.com/blake3`），keyed 模式當 chunk ID。
- 壓縮：zstd（`klauspost/compress/zstd`），預設 level 3；先做不可壓縮偵測。
- 加密：XChaCha20-Poly1305（`golang.org/x/crypto/chacha20poly1305`）；KDF：Argon2id（`golang.org/x/crypto/argon2`）。
- 序列化：CBOR（`fxamacker/cbor/v2`），所有 metadata 都帶 `version` 欄位。
- 後端：介面抽象，實作順序 local → S3（`aws-sdk-go-v2`）→ SFTP（`pkg/sftp`）。
- 糾錯碼（後期）：`klauspost/reedsolomon`。
- 測試：標準 `testing` + `testing/fstest`；fuzz 用 Go 原生 `FuzzXxx`。

## 儲存格式（v1）
repo 是一個 key-value 命名空間，所有物件不可變、以內容 hash 命名：

```
config                      repo 參數、加密 key 封裝（KEK 包 master key）
keys/<id>                   額外 key slot（多密碼 / 還原 key）
packs/<hash>                pack file：[encrypted chunk]* + encrypted trailer index + trailer length(8B) + magic
indexes/<hash>              index blob：chunkID → (packID, offset, length)，是快取，可從 packs 重建
trees/<hash>                目錄物件（content-addressed，未變動的子樹整棵重用）
snapshots/<clientID>/<ts>   快照指標：root tree hash、時間、host、paths、統計
gc/<packID>                 待刪標記（timestamp + 由誰標記）
```

- pack 目標 64 MiB，寫滿或 backup 結束時 flush。
- chunk ID = keyed BLAKE3(master-derived hash key, plaintext)。
- 每個 chunk 獨立 AEAD，nonce 隨機 24B，AAD 帶 chunkID。
- snapshot 是 commit point：所有 pack 與 index 上傳完成後才寫 snapshot。
- Key 階層：password → Argon2id → KEK → 解開 master key → HKDF 派生 chunk key / hash key / index key。

## 無鎖 GC（two-phase, grace period）
1. `prune` 找出不再被任何 snapshot 引用的 pack，寫 `gc/<packID>` 標記，**不刪**。
2. 任何 client 在 backup 時若引用到被標記的 pack，刪除該標記（復活）。
3. `prune` 第二次執行時，標記超過 grace period（預設 72h）且所有已知 client 在標記後都有新 snapshot 的 pack 才真正刪除。
4. snapshot 寫入用後端的 conditional write（S3 `If-None-Match: *`）確保不覆蓋。

## 抗勒索設計
- backup 只需要 Put 權限；List/Delete 給 maintenance 角色。
- 支援 S3 Object Lock；`prune` 對受鎖物件直接略過並回報。

## 里程碑

### M0 骨架（第 1 天）
- `go mod init`、cobra 根命令、`internal/` 分層：`backend`、`crypto`、`chunker`、`pack`、`index`、`tree`、`snapshot`、`repo`、`cmd`。
- Makefile：`build`、`test`、`lint`（golangci-lint）、`fuzz`。
- CI：GitHub Actions 跑 test + lint 在 linux/macos/windows。
- 驗收：`kist version` 可執行，CI 綠燈。

### M1 本機格式定案（第 1–2 週）
- Backend 介面：`Put/Get/List/Delete/Stat`，加 `PutIfAbsent`；local 實作。
- crypto：key 階層、AEAD 封裝、`repo init` 產生 config。
- chunker + pack writer/reader + trailer index。
- tree/snapshot 物件與 CBOR schema。
- 命令：`init`、`backup <path>`、`snapshots`、`restore <snapshot> <target>`、`check`。
- 驗收：對 10 萬檔 / 10 GiB 測試集 backup 後 restore，byte-for-byte 相同；第二次 backup 幾乎不寫新 pack；`check` 能偵測人為破壞的 pack。
- **格式在此里程碑後凍結，之後只能透過 version 演進。**

### M2 S3 後端 + 無鎖並發（第 3–4 週）
- S3 backend（含 MinIO 相容），conditional write。
- 兩台 client 同時 backup 到同一 repo 的整合測試（用 MinIO 容器）。
- index 本地 cache，`rebuild-index` 命令。
- 驗收：並發測試無資料損毀；Put 權限-only 的 IAM policy 可完成 backup。

### M3 GC（第 5 週）
- `forget`（依 retention policy 標記 snapshot）、`prune` 兩階段。
- 針對「prune 進行中另一 client 同時 backup」寫刻意競態測試。
- 驗收：刻意競態下永遠不會刪到活的 chunk。

### M4 可用性（第 6–7 週）
- SFTP 後端；`mount`（FUSE，先 Linux/macOS）。
- 宣告式設定檔 + 排程（內建 cron 語法）、retention、通知（webhook）。
- JSON 輸出（`--json`）、Prometheus metrics endpoint。
- Windows VSS / Linux LVM 快照整合（可選）。

### M5 硬化（第 8 週起）
- Reed-Solomon 可選開啟。
- 對 pack parser、CBOR decoder、chunker 全部加 fuzz test，連續跑 24h。
- 記憶體 profile：100 萬檔 repo 的 backup 峰值記憶體 < 1 GiB。
- 文件與 release 流程（goreleaser）。

## 工程規範
- 每個 package 都有 `doc.go` 說明職責；對外介面有 godoc。
- 錯誤一律 `fmt.Errorf("...: %w", err)` 包裝，不吞錯。
- **CI 強制 `go vet` + staticcheck + errcheck**（含 `check-blank`，`_ = err` 也算吞錯）。
  Go 讓忽略 error 只要打兩個字，這三個是唯一會攔下它的東西。要吞必須寫
  `//nolint:errcheck // 理由`，把「我知道我在做什麼」變成 code review 看得到的一行。
- **所有測試都跑 `-race`。** `-race` 需要 cgo，跟「出貨 binary 禁止 cgo」不衝突：
  `make test` 用 `CGO_ENABLED=0`（跟出貨一致），`make test-race` 用 `CGO_ENABLED=1`
  且只產生測試 binary。CI 兩種都跑，三個 OS 都跑。Go 的 data race 是靜默的
  記憶體損毀，對備份工具而言那等於靜默的資料損毀。
- **`exhaustive` linter 檢查 switch 窮舉。** 加一個 `NodeType`、一個壓縮演算法、
  一個後端錯誤種類而漏掉某個 switch，Go 只會安靜地走 default 或什麼都不做。
  格式演進靠的就是加列舉值，所以這個檢查直接對應「格式可以演進」這個目標。
- 任何寫入 repo 的操作都要先寫測試再實作。
- 格式相關程式碼變更必須附上 `testdata/` 的 golden files。
- 不做的事：不支援非加密 repo、不做 GUI、不自己實作加密原語。
- 每階段結束產出 `docs/format.md` 與 `docs/decisions/NNN-*.md`（ADR），記錄為什麼這樣設計。

## 開始
先做 M0，完成後列出你對 M1 格式的疑問，等我回覆再動手。
