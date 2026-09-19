# 專案：kist — 一個現代化的開源備份工具（Rust）

> 這份文件給 Claude Code 讀。請先讀完整份再開始，每個階段完成後回報並等我確認再進下一階段。
> 專案名 `kist`（蘇格蘭語「箱子、寶箱」），crate 名 `kist`，repo `github.com/<owner>/kist`。
> 我（專案負責人）不熟 Rust。請在每個階段結束時用白話解釋你做了什麼、為什麼這樣設計、我要怎麼驗證；程式碼要保守、直白，不要炫技。

## 定位
去重、加密、可多台機器共用 repo 的備份工具，對象是 object storage（S3 相容）優先，其次本機與 SFTP。
設計目標依序：資料安全性 > 還原可靠性 > 抗勒索 > 效能 > 功能數量。

**本 repo（kist-rs）是正式產品與格式標竿**；`github.com/at-least/kist`（Go）是
參考實作，只用来交叉驗證格式與抓 bug，不單獨演進。兩邊共用同一份
`docs/format.md`（v2，權威規格）與跨語言 conformance 向量；任何格式改動
都要同時改兩邊、golden、向量與 E2E。設計取捨以產品（Rust）為優先，不為
遷就 Go 選次級方案（2026-09-05 的 CBOR 欄位順序修訂就是這個原則的執行）。

競品參考：restic（穩定但有鎖、記憶體重）、Kopia（pack + 無鎖，最接近我們）、Duplicacy（無鎖 GC 但一 chunk 一檔）。
我們要的是 Kopia 的儲存效率 + Duplicacy 的無鎖 GC + 原生抗勒索設計 + Rust 帶來的低記憶體與安全性。

## 技術選型（固定，不要改）
- Rust stable（edition 2021），`cargo`，單一 binary，禁止 `unsafe`（`#![forbid(unsafe_code)]`；若真的需要，先跟我討論）。
- async runtime：`tokio`；CPU 密集段（chunk、hash、壓縮、加密）用 `rayon` 或 `tokio::task::spawn_blocking`，不要在 async task 裡做重計算。
- CLI：`clap`（derive）；設定檔：TOML（`toml` + `serde`）。
- 錯誤處理：library crate 用 `thiserror`，binary 用 `anyhow`；禁止 `unwrap()`/`expect()` 出現在非測試程式碼。
- Chunking：FastCDC（**自殖實作**，gear 表取自 fastcdc-go v0.2.0 並以
  digest 釘死——邊界函式是凍結格式，不依賴上游；參數存 config，CLI
  `init --chunker-min/avg/max` 可調），min 512 KiB / avg 2 MiB / max 8 MiB。
- Hash：`blake3`，keyed 模式當 chunk ID，`derive_key` 做 key 派生。
- 壓縮：`zstd`，預設 level 3；先做不可壓縮偵測。
- 加密：`chacha20poly1305`（XChaCha20-Poly1305）；KDF：`argon2`（Argon2id）；隨機：`rand` + OS RNG。全部來自 RustCrypto，不自己實作原語。
- 序列化：CBOR（`ciborium` + `serde`），所有 metadata 都帶 `version` 欄位。
  規範編碼 = 規格釘死的欄位表順序（`docs/format.md` §4；不做 key 排序）。
- 後端：`object_store` crate 統一抽象（local / S3 / GCS / Azure 一次涵蓋），SFTP 後期另做。
- 糾錯碼（後期）：`reed-solomon-erasure`。
- 日誌：`tracing`；測試：內建 `#[test]` + `proptest` 做屬性測試 + `cargo-fuzz` 做 fuzz。
- 記憶體：index 用本地磁碟快取（sorted table，`pread` 逐筆讀——不用 mmap，
  所有 crate `forbid(unsafe_code)`）；tree 物件 streaming 處理。

## 儲存格式（v2 — 與 Go 參考實作統一；權威規格見 `docs/format.md`）
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
- sealed 物件無 header：`nonce24‖ct‖tag16`，AAD 依角色（tree=自 ID、
  snapshot=完整 key、trailer/index=角色常數）。
- 所有格式結構定義集中在 `kist-format` crate，其他 crate 只能透過它讀寫。

## 無鎖 GC（two-phase, grace period；詳見 format.md §13）
1. `prune` 找出不再被任何 snapshot「解析到」的 pack（正本 = 未標記優先、
   其次名稱最小），寫 `gc/<名稱>` **無資訊**標記（pack/tree/index 共用命名
   空間），**不刪**。標記的年齡 = 後端修改時間（一律截秒）。
2. backup 嚴格 Put-only：被標記的 pack **不拿來去重**（chunk 重寫一份），
   **絕不刪標記**；commit 前驗證引用與 BackupTooLong 閘門。
3. 第二次 prune：標記超過 grace（預設 72h）且每個活躍 client（30 天內有
   snapshot，含 clock_skew 容差）在標記後都有新 snapshot 才刪；刪 pack 前
   先寫 supersedes 全部舊 blob 的新 index。
4. repack：活 chunk 搬新 pack，舊 pack 留在 index 並標記，走同一兩階段。
5. snapshot 寫入用 conditional put（`PutMode::Create`）確保不覆蓋。

## 抗勒索設計
- backup 只需要 Put 權限；List/Delete 給 maintenance 角色。
- 支援 S3 Object Lock；`prune` 對受鎖物件直接略過並回報。

## Workspace 結構
```
kist/
  Cargo.toml            workspace
  crates/
    kist-format/        所有 on-disk 結構、序列化、版本（最重要，改動需我確認）
    kist-crypto/        key 階層、AEAD 封裝
    kist-chunker/       FastCDC 封裝
    kist-backend/       object_store 封裝 + conditional put + 本地 index cache
    kist-core/          backup / restore / check / prune 邏輯
    kist-cli/           clap 命令列，產出 `kist` binary
  fuzz/                 cargo-fuzz targets
  tests/                跨 crate 整合測試（含 MinIO 容器）
  docs/format.md        格式規格（與程式碼同步更新）
  docs/decisions/       ADR
```

## 里程碑

### M0 骨架（第 1–2 天）【完成】
- workspace 建立、`kist version` 可執行。
- CI（GitHub Actions）：`cargo fmt --check`、`cargo clippy -D warnings`、`cargo test`、`cargo deny check`（授權與漏洞），linux/macos/windows 三平台，開 sccache 或 cargo cache 縮短編譯。
- 驗收：CI 綠燈，clean build 時間記錄在 README。

### M1 格式定案（第 2–4 週）【完成；v1 已被 M6 的 v2 取代】
- `kist-format`：所有結構 + CBOR 序列化 + golden files。
- `kist-crypto`：key 階層、AEAD、`repo init`。
- chunker + pack writer/reader + trailer index。
- tree/snapshot 物件，目錄樹 streaming 寫入。
- 命令：`init`、`backup <path>`、`snapshots`、`restore <snapshot> <target>`、`check`（含 `--read-data`）。
- 驗收：對 10 萬檔 / 10 GiB 測試集 backup 後 restore，用 `diff -r` 或 `rsync -c` 這類獨立工具驗證 byte-for-byte 相同；第二次 backup 幾乎不寫新 pack；`check` 能偵測人為破壞的 pack；峰值記憶體記錄下來。
- **格式在此里程碑後凍結，之後只能透過 version 演進。**

### M2 S3 後端 + 無鎖並發（第 5–6 週）【完成】
- S3（含 MinIO 相容）via `object_store`，conditional put。
- 兩台 client 同時 backup 到同一 repo 的整合測試（MinIO 容器）。
- index 本地 mmap cache，`rebuild-index` 命令。
- 驗收：並發測試無資料損毀；Put 權限-only 的 IAM policy 可完成 backup。

### M3 GC（第 7–8 週）【完成】
- `forget`（retention policy）、`prune` 兩階段、repack、inactive client 規則。
- 針對「prune 進行中另一 client 同時 backup」寫刻意競態測試，用 `proptest` 產生交錯順序。
- 驗收：刻意競態下永遠不會刪到活的 chunk。**到這裡可以開始自己使用。**

### M4 可用性（第 9–13 週）【大部分完成】
- 宣告式設定檔 + 排程（內建 cron 語法）、retention、通知（webhook）✓
- `--json` 輸出、Prometheus metrics ✓
- Web UI：`axum` + `rust-embed`，htmx + server-rendered 模板（**maud**，
  ADR 007）✓
- SFTP 後端 ✗（Go 參考實作有 `pkg/sftp` 版可對照）
- `mount`（`fuser`，Linux/macOS）✗（同上）
- Windows VSS（`windows` crate）✗ 延後，可選

### M6 v2 統一格式（2026-09-05）【完成】
- 與 Go 參考實作統一為單一 v2 規格：欄位表順序 CBOR、tree 以明文 keyed
  hash 命名、無 envelope header、BLAKE3 DeriveKey 子金鑰、FastCDC 以 Go
  實作為準移植、扁平 tree entry（分段/間接/硬連結/xattr/絕對路徑根）、
  supersedes index、無資訊 GC 標記、Put-only backup。
- 共用跨語言 conformance 向量（切塊邊界、金鑰、tree CBOR）進兩邊 testdata。
- 產品缺口補齊：index 合併「未標記優先」、解碼後版本檢查、trailer 一致性
  檢查、backup 記錄 xattr、CLI chunker 參數。
- 驗收：雙向 backup/restore/check 逐 byte 相同、雙向 0-new-chunk 去重、
  跨實作 prune 後仍互通；ADR 008。

### M5 硬化（第 14 週起）【進行中】
- Reed-Solomon 可選開啟【完成 2026-09-06】：`--parity`/`[backup] parity`
  寫 sidecar、`check --repair`、prune 清掃；RS 矩陣與 Go 逐 byte 相容
  （雙向 PoC + 共用 golden），ADR 009。
- `cargo-fuzz`（基礎建設【完成 2026-09-06】，`fuzz/`：pack、cbor、
  chunker、parity 四 target + seed corpus；斷言有效性經負向對照驗證）；
  長跑【完成 2026-09-06】：每 target 1h 序列全數 OK（pack 4.6M exec、
  cbor 21.5M、chunker 7.4M、parity 2.8M，零 crash、零 artifact、seeds
  乾淨）；首輪 cbor 在 35 分鐘假性 OOM——ASan allocator 頁保留讓累計
  RSS 失真（live heap 僅 36MB、無 ASan 對照峰值 62Mb），看門狗改
  `-rss_limit_mb=0 -malloc_limit_mb=2048` 並負向對照驗證（ADR 010 §6）。
  96h 全量（每 target 24h）腳本就緒：`nohup sh fuzz/longrun.sh 86400 &`，
  擇時執行。長跑驅動內建 nice 19 讓路【2026-09-10】（`KIST_FUZZ_NICE`
  覆寫、0 關閉；同核心爭搶實測 nice19 僅佔 0.6%，跨 systemd scope 同效，
  協議見 fuzz/README.md）。**96h 全量已執行【2026-09-13 → 09-17，fail=0】**：
  四個 target 依序各 24h 全數 OK、零 crash、`fuzz/artifacts/` 四個目錄皆空、
  `fuzz/seeds` 未被寫入。逐 target 執行量（掛鐘 86401s）與 corpus 成長：
  pack 149.3M exec（corpus → 50M）、cbor 7 033.0M exec（→ 200M）、
  chunker 143.1M exec（→ 732K）、parity 72.9M exec（→ 6.9M）；全程 nice 19
  讓路未影響日常操作。完整 log：`~/kist-fuzz-96h.log`（本次 log 移出 /tmp，
  避免 4 天期間被清）。
- 記憶體目標：100 萬檔 repo 的 backup 峰值 < 512 MiB【完成 2026-09-06】：
  量測（dhat 歸因）找出每檔 16 MiB zeroed 切塊緩衝、每檔 1 MiB BufReader、
  pack 緩衝 Vec 倍增到 128 MiB 級、CBOR 走 Value 中繼、平面大目錄把 parent
  entries 整份讀成 HashMap 等根因；修後 A 集（1000 目錄）373–431 MiB、
  B 集（單一平面 1M 檔）442–524 MiB，`MemoryMax=512M` 硬門檻 A 3/3、
  B 4/4 通過。方法與根因清單：`bench/memory/`、ADR 011。
  restore 71 MiB、prune 550 MiB（非門檻記錄；prune 留給之後）。
- release binary【完成 2026-09-06，ADR 012】：版本升 0.1.0；`./dist.sh` 從
  本機 cross 產出 linux-musl ×2 / macOS ×2 / windows-gnu 五個 target 的
  壓縮檔 + SHA256SUMS（zig + cargo-zigbuild；musl 兩個有煙霧全流程，
  windows/macOS 僅建置驗證）。順手修掉 xattr 讓 Windows 編得過，並清掉
  `cargo clippy --all-targets -D warnings` 的 13 個既有 error。

## 工程規範
- `#![forbid(unsafe_code)]`、`#![deny(clippy::unwrap_used, clippy::expect_used)]` 在所有 lib crate。
- 每個 crate 有 `lib.rs` 頂端文件說明職責；公開 API 有 doc comment。
- 任何寫入 repo 的操作都要先寫測試再實作。
- `kist-format` 的變更必須附 golden files 更新與 `docs/format.md` 更新。
- 不做的事：不支援非加密 repo、不做原生 GUI、不自己實作加密原語。
- 每階段結束產出 `docs/decisions/NNN-*.md`（ADR），用白話解釋為什麼這樣設計。
- 每階段結束開一個新的 review session：只給格式文件和程式碼，請 Claude Code 以審查者身分找 bug 與設計漏洞，重點是 GC、加密、pack 解析。

## 給 Claude Code 的工作方式
- 每個 session 只做一件事，先寫測試再寫實作。
- 每個階段完成就把該階段直接 commit（不用先問）；回報照舊，開始下一階段前仍等確認。
- 遇到 borrow checker 或 lifetime 問題，優先用簡單的做法（clone、`Arc`）而不是複雜的 lifetime 標註，效能問題之後再處理。
- 每次回報用三段：做了什麼、我該怎麼驗證（給我可直接執行的指令）、還有什麼疑問。

## 開始
先做 M0，完成後列出你對 M1 格式的疑問，等我回覆再動手。

## 下一步（產品路線，依優先序）
1. **M5 硬化**：cargo-fuzz 四 target（pack、cbor、chunker、parity）已建
   並過煙霧【2026-09-06】；1h×4 長跑全數 OK【2026-09-06】。大 repo 記憶體
   目標達成【2026-09-06，ADR 011；advisor 簽核：以 MemoryMax 硬門檻為準】。
   prune 記憶體同樣達成【2026-09-06：1.05 GiB → 230 MiB，walk_tree 鏈式持有
   與 ADR 005 §5 三份結構合一，見 ADR 011 prune 節】。
   release binary 完成【2026-09-06，ADR 012：`./dist.sh` 產五個 target，
   版本 0.1.0】。M5 至此全數完成。
2. **M4 尾巴**：SFTP 後端【完成 2026-09-07，ADR 013：russh + openssh-sftp-client，
   host key 嚴格驗證、hardlink/posix-rename 原子寫入，合約測試與 CLI 端到端全過】、
   rclone 橋接【完成 2026-09-07，ADR 014：`rclone://` stdio 橋接（kist spawn
   `rclone serve sftp --stdio`），寬鬆條件寫入 opt-in、config 讀回驗證擋雙重 init；
   實測 rclone 不實做 hardlink/O_EXCL，`sftp://` 維持嚴格並給明確錯誤】、
   `mount`【完成 2026-09-07，ADR 015：`crates/kist-mount`（fuser 0.18），
   唯讀 `<client>/<ts>/<樹>`、index `raw_len` 隨機讀、volatile/immutable TTL、
   子程序 E2E 與 CLI 手動驗證全過】。**M4 全數完成。**
3. xattr 的 restore 套用【完成 2026-09-07：fsmeta::apply_xattrs（只套 user.*，
   惡意 repo 不能指揮特權 namespace），在 times/mode **之前**套（唯讀 mode 的
   檔案才設得進 xattr）；symlink 不套（Linux 不能對 symlink 設 user.*，且
   xattr::set 會跟隨連結）；失敗記節點錯誤，其他檔案繼續。往返測試含
   0o444 排序案例與 symlink 案例】。
4. 回報上游【完成 2026-09-07：openssh-rust/openssh-sftp-client#183，
   https://github.com/openssh-rust/openssh-sftp-client/issues/183 】。
5. CI 把跨語言 interop（Go 測試 + 共用向量）納入 pipeline。【完成 2026-09-07：
   workflow 加手動觸發的 interop job（維持「只手動觸發、不吃配額」政策）；
   Go interop 測試本機跑過。】

以上路線圖全部完成【2026-09-07】。完成審計【2026-09-07】：本機 gate 全綠——
`cargo fmt --check`、`clippy --all-targets -D warnings`、`cargo test --workspace`
（56 個測試執行檔零失敗）、`cargo deny check` 四項 ok（修復 RUSTSEC-2024-0384
紅燈後，見已接受的限制）、MinIO S3 整合測試（contract + s3 共七案）全過。

6. **gate 全量重驗在收斂後的 monorepo 上重跑【2026-09-13】**：抓到一個
   間歇失敗——`backup_gc_rules::marked_pack_is_not_used_for_dedup`
   （5 次跑失敗 4 次）。根因：`ChunkIndex::add_pack_ranked` 的合併比較把
   既有位置的「是否被標記」寫死成 `false`，被標記的舊位置一旦先進
   overlay 就永遠贏過後到的未標記位置，backup 於是反覆重寫那些 chunk
   （違反 §10「與載入順序無關」與 M3 驗收「標記後 0 新 chunk」）；是否
   踩中取決於 blob 載入順序與每次 backup 都不同的隨機 pack 名稱。修法
   與 prune 的正本選擇同一個全序（兩側都算 `(marked, pack)`，見
   `prune.rs` 的 rank），新增決定性單元測試釘死兩種加入順序。修後：
   單元測試綠、整合測試 20/20、`cargo test --workspace` 59 執行檔零
   失敗、MinIO S3 整合全過（contract 3 + s3 4 + s3_end_to_end 3，容器
   測畢已清）、gc_race 200 案例通過
   （317.9s）、fmt/clippy/deny 綠；Go `make verify`（build/vet/lint/
   test/-race，含 interop）綠。

## M7：format v3 ＋ 遠端來源（2026-09-08 起）【完成 2026-09-09】

【2026-09-09 二次重排】本專案改為 **monorepo**：Rust（產品）在 repo 根，
Go（參考實作）在 `go/` 子目錄；單一遠端 at-least/kist。歷史經 git
subtree 合併保留（merge commit 5a67394 + 重排 commit）。位置慣例：
`docs/format.md`（根）= v3 權威規格；`go/docs/format.md` = Go 端拷貝
（CI 逐 byte 檢查）。Go 的開發與驗證都在 `go/` 內進行
（build/vet/lint/test）。全部完成項與實機 E2E 證據見
下方各條。

新輸入兩個：**遠端來源需求**（`kist backup sftp://…`/`s3://…`，client 當
轉運、金鑰不出機器）與 v1/v2 教訓清單。設計立場：v2 核心全保留（有 PoC
與 200-case proptest 背書），變更外科手術式九項，每項對應教訓或需求
（ADR 016；規格草案 `docs/format-v3-draft.md`，定案後逐 byte 取代
`docs/format.md` 成唯一權威副本）。

- 設計草案＋advisor 檢查點【完成 2026-09-08，commit 7e626db】：advisor
  抓出 touch 用 PutIfAbsent 的**資料遺失級競態**（第二次 backup 重用已
  標記樹時 mtime 不刷新 → prune 誤刪 → snapshot 懸掛），修為覆寫式 Put
  ＋commit 檢查收窄，時間線釘成向量 V3-GC-5。
- kist-rs 全面遷移【完成 2026-09-08，commit f256642】：roots 取代合成根、
  stats 只留資料事實（過程計數移 `BackupReport`）、Entry metadata 聯集
  （mk＋etag/vern，posix uid/gid 必填）、Invariants 入 wrapped 密文、
  min_reader、touch 復活＋驗證式自愈、`.r1` 副本（GC 成組、孤兒不自動
  刪）、index 壓縮觸發（>64）、restore/mount 的 roots 映射與檔案來源
  落點。gate：fmt/clippy -D warnings/270 測試零失敗/deny 四項 ok。
  過程中修掉：snapshot 列表未排除 `.r1`、commit 檢查誤比 write-once 樹的
  mtime、檔案來源被丟棄。
- 遠端來源（Source 抽象）【完成 2026-09-09，commit 9785c86】：
  `kist-backend::source`（Local/ObjectStore-backed SFTP/S3，async 串流
  橋接 blocking Read）；walker 走 Source；CLI `kist backup sftp://…`／
  `s3://…`；FakeSource 測試 7 案（etag 快速路徑、sftp 不重用、檔案來源
  落點等）。replicas 預設依後端（本機 1、遠端 0），InitOptions 可覆寫。
- kist-go 鏡射 v3【完成 2026-09-09，kist-go commit e0b7c63】：全部結構、
  金鑰（wrapped Invariants）、touch 復活、.r1 副本、壓縮觸發、快速路徑
  分級、遠端來源介面；interop 向量對齊 Rust 端錄製的 v3（金鑰逐 byte、
  tree canonical CBOR 611 bytes、parity golden）。gate：build/vet/
  golangci-lint 0 issues/CGO=0 全套件 ok（mount 兩案為沙箱 fusermount
  權限限制）。移植中修掉兩個真 bug：writeTree 對「剛寫入但帶過期標記」
  的主體樹補 touch（commit gate 誤拒）；backupRoots 檔案來源 err 遮蔽。
- gc race 測試在 touch 語意下重跑【完成 2026-09-09】：兩邊各補 V3-GC-5
  時間線釘死測試（標記後重用的樹經 touch 復活、snapshot 完整可還原）；
  Go 側 race 測試期望全面更新為「樹也參與標記/持有/刪除」。
  200-case 深度重跑【完成 2026-09-09】：
  `PROPTEST_CASES=200 cargo test -p kist-core --test gc_race`——proptest
  全綠零失敗，313.64s（v2 的 200-case 同基準 381s）。
- 規格正式化【完成 2026-09-09，commit c8ad826】：`docs/format.md` 升格
  為 v3 權威規格（單一副本）；kist-rs CI interop job 檢查 kist-go 拷貝
  逐 byte 一致（commit 22b3411）。
- 跨語言雙向 E2E【完成 2026-09-09】：Go init/backup → Rust check
  --read-data ✓ + restore 逐 byte 相同 ✓；Rust 對 Go repo 再備份
  0 新 chunk（跨實作去重 100%）✓；Rust init/backup → Go check ✓ +
  restore 逐 byte 相同 ✓；Rust prune → Go check ✓；Go forget+prune →
  Rust check ✓ + 最終還原逐 byte 相同 ✓。
- fuzz seeds 隨 v3 重生成【完成 2026-09-09，commit 8fb2450】：cbor 五結構、
  pack 排版、parity golden 全部由 v3 golden 機器產生；4 targets 煙霧全過。
- 大 repo 記憶體重審【完成 2026-09-09】：重審**抓到回歸**——SourceItem
  整批物化讓 100 萬條目平鋪目錄的清單多 ~150-250 MiB，B 集首備頂到
  512 MiB 硬門檻被殺（v2 可存活）。修法：`Source::list` 改惰性迭代器
  （本機來源只常駐排序後名稱 ~45 MiB，metadata 逐條 lstat；commit
  72530c0）。重測結果（MemoryMax=512M 硬門檻，release，N≥1）：
  A 集首/次備份 ex-file-slab 峰值 283 MiB（優於 v2 的 373–431）、
  B 集 450 MiB（在 v2 的 442–524 帶內）、prune ×3 ~230 MiB——全部
  通過硬門檻不被殺。
- SFTP/S3 來源的實機 E2E【完成 2026-09-09】：docker 的 MinIO（:19000）
  與 OpenSSH SFTP（:19222）容器實測——S3 來源備份→還原逐 byte、etag
  變更偵測（變更→3 新 chunk、無變更→全 reuse）；SFTP 來源備份→還原
  （落點含 user@host 元件）、無快速路徑語意（重讀但去重吸收）、
  check --read-data 乾淨。實機測試抓到並修掉四案：list 的 block_on
  在 async 執行緒 panic、Url 指向本機路徑時 posix 重取無路徑、
  SFTP 來源雙重前綴（列出 0 條目）、S3 檔案來源的 store 前綴處理
  （裸 bucket + root 拼接 + rel="" 根讀取契約；三種來源形狀——檔案/
  目錄/含子目錄——實機全數驗證）。
- Go 端 Source 介面移植【完成 2026-09-09，kist-go commit d6a1bdb：
  `internal/source`（Source 介面、Local/Memory/SFTP/S3、OpenSource）；
  詳見 go/PLAN.md「遠端「來源」介面」節】。
- mount 測試修復【2026-09-09】：`TestMountServesTheSnapshot` 連 snapshot
  根葉節點也比對來源 dir 的 mode。v3 Root 只帶 {path, tree}，root 自身
  的 mode 格式上不存在，兩端都刻意讓葉節點呈現唯讀合成目錄 0555
  （Rust `root_leaf_entry` 的 `0o040_555` 同形）；測試改為對 "." 釘
  合成形狀、其餘條目維持逐 byte 比對。此前沙箱 fusermount 不可用時
  mount 測試提前失敗，這條斷言未被走到。修後 gate 全綠：Go build/vet/
  CGO=0 全套件/-race 全套件；Rust fmt/`clippy -D warnings`/
  `test --workspace`/deny 四項 ok；根 `docs/format.md` 與
  `go/docs/format.md` 逐 byte 一致。
- CI interop job 修復【2026-09-09】：`ci-rust.yml` 的
  `go test ./internal/interop/...` 沒帶 `working-directory: go`——M8
  重排後根目錄已無 go.mod，這個跨語言 gate 在 CI 上必失敗；補上
  step 層級設定（format.md 的 cmp 步驟用根相對路徑，維持在根），
  同檔三處 rust/ 時代的過時註解一併更新。本機鏡射驗證：
  `cd go && go test ./internal/interop/...` ok、兩份 format.md
  逐 byte 一致。
- 完成 gate 全量重驗【2026-09-09】：本機補齊 CI 同款 lint 工具後——
  golangci-lint（v2.13.2）`run ./...` 0 issues、actionlint 三個
  workflow 乾淨、MinIO S3 整合測試重跑（contract 3 + s3 4，七案全過，
  測畢容器已清）。至此完成審計的每一個 gate（Go build/vet/CGO=0/
  -race/lint、Rust fmt/clippy/test --workspace/deny、MinIO 整合、
  format.md 同步）皆在本 session 以實機重驗綠燈。

## 已接受的限制（非待辦）

- **Windows VSS 與 Windows 路徑語意驗證**：裁示為已接受的限制，自路線圖移除
  【2026-09-07】。VSS 是 Windows COM API、路徑語意（UTF-16、保留名稱、結尾
  點/空白）只能在真 Windows 上驗——Linux 開發機上無法實作亦無法驗證，不寫
  測不了的 stub。**若未來取得 Windows 環境**，從這裡重啟：先在 Windows 上跑
  既有測試套件確認編譯與基本行為，再實作 VSS（`backup` 前取 shadow copy）與
  路徑語意驗證；格式層（`kist-format`）已平台中性，不需改動。

- **cargo-deny ignore RUSTSEC-2024-0384（`instant` unmaintained）**【2026-09-07】：
  `instant` 是 reed-solomon-erasure 6.0.0 → parking_lot 0.11 的傳遞依賴，
  6.0.0 即最新版、上游無升級路徑；advisory 屬「停止維護」而非已知漏洞，
  instant 本身只是 std::time shim（無 unsafe），實際風險為零。不走「關
  default features 進 no_std」的路——那會改變格式關鍵的 parity 路徑的錯誤
  型別與鎖行為（ADR 009 已與 Go 逐 byte 驗證），為消一條資訊性提示重驗不
  值得（advisor 簽核）。**移除條件**：reed-solomon-erasure 發新版脫離
  parking_lot 0.11、更換 RS crate，或 instant 日後出現真正的漏洞 advisory。

- **CI interop job 首次觸發前需設 `GO_REF_TOKEN`**【2026-09-07 記錄】：
  at-least/kist 是 private repo，同 repo 的 github.token 讀不到它；第一次
  dispatch interop job 前要在 repo secrets 放一顆可讀該 repo 的 PAT
  （見 ci.yml interop job 內註解）。未設則該 job 預期失敗，**不影響其餘
  gate**，不要誤判為 regression。另：本機開發環境未設定 git remote，
  commit 均只存在本地，push 需先由負責人加上遠端。
  【2026-09-12 更新：monorepo（M8）之後 interop job 移到 `ci-rust.yml`、
  與 Rust 同樹 checkout，不再需要跨 repo PAT——此條僅存歷史。】

- **GitHub Actions 額度：`ci.yml`（Go）push 觸發的 runs 全數未啟動**
  【2026-09-12 記錄】：私有 repo 的 Actions 因帳號「recent account payments
  have failed or your spending limit needs to be increased」無法啟動 job——
  自 2026-09-09 起每個 push 的 run 都瞬間 failure：job 無 steps、無 runner、
  無 log（2026-09-04 當天仍有成功的 run，Go repo 時代）。**不是 regression**：
  程式 gate 以本機重驗為準（見上方 2026-09-09 完成 gate 全量重驗）。
  修復途徑擇一：Settings → Billing & plans 補付款／提高 spending limit；
  或把 `ci.yml` 觸發改為 `workflow_dispatch`（與 Rust 端同政策，省配額）。
