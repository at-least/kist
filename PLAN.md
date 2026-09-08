# 專案：kist 的 Go 參考實作

> 這份文件給 Claude Code 讀。請先讀完整份再開始。
> 專案名 `kist`（蘇格蘭語「箱子、寶箱」），module path `github.com/<owner>/kist`。

## 定位（2026-09-05 起）
**本 repo 是參考實作，不是產品。** 產品是 `kist-rs`（Rust）；本實作的職責是
交叉驗證格式設計的可行性、抓產品的 bug、提供第二個視角的實作證據。
格式以兩邊共用的 `docs/format.md`（v2）為權威，本實作**跟隨不領導**：
任何格式改動由產品端發起，這邊同步實作、golden 與 `internal/interop`
的跨語言向量。測試與互通驗證照常維持綠燈。

## 技術選型（固定，不要改）
- Go 1.22+，單一 binary，禁止 cgo。
- CLI：`spf13/cobra`；設定檔：TOML（`BurntSushi/toml`）。
- Chunking：FastCDC 自殖實作（gear 表取自 fastcdc-go v0.2.0、digest 釘死；
  參數存 config），min 512 KiB / avg 2 MiB / max 8 MiB——此實作是 v2 的
  格式基準，Rust 端逐 byte 移植。
- Hash：BLAKE3（`lukechampine.com/blake3`），keyed 模式當 chunk ID。
- 壓縮：zstd（`klauspost/compress/zstd`），預設 level 3；先做不可壓縮偵測。
- 加密：XChaCha20-Poly1305（`golang.org/x/crypto/chacha20poly1305`）；KDF：Argon2id（`golang.org/x/crypto/argon2`）。
- 序列化：CBOR（`fxamacker/cbor/v2`），所有 metadata 都帶 `version` 欄位。
- 後端：介面抽象，實作順序 local → S3（`aws-sdk-go-v2`）→ SFTP（`pkg/sftp`）。
- 糾錯碼（後期）：`klauspost/reedsolomon`。
- 測試：標準 `testing` + `testing/fstest`；fuzz 用 Go 原生 `FuzzXxx`；
  `internal/interop` 持有跨語言 conformance 向量（與 kist-rs 共用）。

## 儲存格式（v2 — 權威規格見 `docs/format.md`）
repo 是一個 key-value 命名空間，所有物件不可變、以內容 hash 命名：

```
config                      repo 參數、加密 key 封裝（KEK 包 master key）
keys/<id>                   額外 key slot（多密碼 / 還原 key）
packs/<hash>                pack file：[encrypted chunk]* + encrypted trailer index + trailer length(8B) + magic
indexes/<hash>              index blob：chunkID → (packID, offset, length)，是快取，可從 packs 重建
trees/<hash>                目錄物件（keyed hash of **明文 CBOR**；未變動子樹整棵重用）
snapshots/<client>/<ts>     快照指標：root tree、時間（ns）、host、paths、parent、統計
gc/<objectID>               無資訊待刪標記（固定 8 bytes；時間看後端 mtime 截秒）
parity/<packID>             選配 Reed-Solomon sidecar（他端忽略）
```

- pack 目標 64 MiB，寫滿或 backup 結束時 flush。
- chunk ID 與 tree ID = keyed BLAKE3(hash key, 明文)；pack/index 以密文 hash 命名。
- 每個 chunk 獨立 AEAD，nonce 隨機 24B，AAD 帶 chunkID。
- snapshot 是 commit point：所有 pack 與 index 上傳完成後才寫 snapshot。
- Key 階層：password → Argon2id（64 MiB/t3/p4）→ KEK → 解開 master key →
  BLAKE3 DeriveKey（`kist/v2/{hash,chunk,meta,index}`）。master 的 AAD 綁
  repo_id 與 chunker 參數。**沒有 nonce key**：全部隨機 nonce。
- sealed 物件無 header：`nonce24‖ct‖tag16`，AAD 依角色。CBOR 規範編碼 =
  規格釘死的欄位表順序（`docs/format.md` §4），不做 key 排序。

## 無鎖 GC（two-phase, grace period；詳見 format.md §13）
1. `prune` 找出不再被任何 snapshot「解析到」的 pack（正本 = 未標記優先、
   其次名稱最小；間接清單要解開算資料 chunk），寫 `gc/<名稱>` **無資訊**
   標記，**不刪**。標記年齡 = 後端修改時間（截秒）。
2. backup 嚴格 Put-only：被標記的 pack 不拿來去重（chunk 重寫），**絕不刪
   標記**；commit 前驗證引用 pack 與自己寫的 tree，BackupTooLong 拒絕。
3. 第二次 prune：標記超過 grace 且活躍 client（snapshot 推得，30 天 +
   clock_skew）在標記後有新 snapshot 才刪；刪前先寫 supersedes 全部舊
   blob 的新 index；孤兒標記只清「物件在各 namespace 都不存在」者。
4. repack：活 chunk 搬新 pack，舊 pack 留在 index 並標記，走同一兩階段。

## 抗勒索設計
- backup 只需要 Put 權限；List/Delete 給 maintenance 角色。
- 支援 S3 Object Lock；`prune` 對受鎖物件直接略過並回報。

## 里程碑

### M0 骨架（第 1 天）【完成】
- `go mod init`、cobra 根命令、`internal/` 分層：`backend`、`crypto`、`chunker`、`pack`、`index`、`tree`、`snapshot`、`repo`、`cmd`。
- Makefile：`build`、`test`、`lint`（golangci-lint）、`fuzz`。
- CI：GitHub Actions 跑 test + lint 在 linux/macos/windows。
- 驗收：`kist-go version` 可執行，CI 綠燈。

### M1 格式定案（第 1–2 週）【完成；v1 已被 v2（M6）取代】
- Backend 介面：`Put/Get/List/Delete/Stat`，加 `PutIfAbsent`；local 實作。
- crypto：key 階層、AEAD 封裝、`repo init` 產生 config。
- chunker + pack writer/reader + trailer index。
- tree/snapshot 物件與 CBOR schema。
- 命令：`init`、`backup <path>`、`snapshots`、`restore <snapshot> <target>`、`check`。
- 驗收：對 10 萬檔 / 10 GiB 測試集 backup 後 restore，byte-for-byte 相同；第二次 backup 幾乎不寫新 pack；`check` 能偵測人為破壞的 pack。
- **格式在此里程碑後凍結，之後只能透過 version 演進。**

### M2 S3 後端 + 無鎖並發（第 3–4 週）【完成】
- S3 backend（含 MinIO 相容），conditional write。
- 兩台 client 同時 backup 到同一 repo 的整合測試（用 MinIO 容器）。
- index 本地 cache，`rebuild-index` 命令。
- 驗收：並發測試無資料損毀；Put 權限-only 的 IAM policy 可完成 backup。

### M3 GC（第 5 週）【完成】
- `forget`（依 retention policy 標記 snapshot）、`prune` 兩階段。
- 針對「prune 進行中另一 client 同時 backup」寫刻意競態測試。
- 驗收：刻意競態下永遠不會刪到活的 chunk。

### M4 可用性（第 6–7 週）【完成】
- SFTP 後端；`mount`（FUSE，先 Linux/macOS）。
- 宣告式設定檔 + 排程（內建 cron 語法）、retention、通知（webhook）。
- JSON 輸出（`--json`）、Prometheus metrics endpoint。
- Windows VSS / Linux LVM 快照整合（可選）。

### M5 硬化（第 8 週起）【完成】

fuzz、parity、mount、race test 均已落地；剩餘硬化項隨產品端排程。

### M6 v2 統一格式（2026-09-05）【完成】

- 與 kist-rs 統一為單一 v2 規格（docs/format.md；ADR 011）。
- 金鑰改 BLAKE3 DeriveKey、tree 扁平 entry + 明文 hash 命名、pack 頭尾
  magic + BE、index supersedes、無資訊 GC 標記、Put-only backup。
- internal/interop 跨語言向量（切塊邊界、金鑰、tree CBOR）。
- 驗收：雙向 backup/restore/check 逐 byte 相同、雙向 0-new-chunk 去重。
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

## M7：v3 格式移植（2026-09-09）【完成】

對齊 kist-rs 的 v3 權威規格（`docs/format.md` 為其逐 byte 拷貝，由
kist-rs 的 CI interop job 檢查同步）：Snapshot.Roots 取代合成根、
Entry.mk metadata 聯集（etag/vern）、wrapped master 攜帶 Invariants、
config 加 min_reader/replicas、樹 write-once＋touch 復活、.r1 副本、
有效 index blob > 64 強制合併、快速路徑分級。interop 向量對齊 Rust
端錄製的 v3（金鑰逐 byte、tree canonical CBOR、parity golden）。
gc race 測試在 touch 語意下重跑，補 V3-GC-5 時間線釘死測試（標記後
重用的樹經 touch 復活）。移植中修掉兩個真 bug：writeTree 對「剛寫入
但帶過期標記」的主體樹補 touch（commit gate 誤拒）；backupRoots 檔案
來源的 err 遮蔽。

**已知未移植（跟隨產品端排程）**：遠端「來源」介面（kist-rs 的
`kist-backend::source`，`kist backup sftp://…`／`s3://…`）——本 repo 目前
只能當 repo 端；格式層已完全支援遠端來源（Entry.mk 聯集、etag/vern、
快速路徑分級），移植 Source 介面時不需要再動格式。
