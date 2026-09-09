# 008 — v2：與 Go 實作統一的儲存格式

日期：2026-09-05。狀態：已接受（實作完成）。

## 背景

kist 有兩個獨立實作：本 repo（Rust）與 Go 參考實作（`github.com/at-least/kist`）。
兩邊各自凍結了互不相容的 v1——CBOR 編碼（ciborium 宣告順序 vs fxamacker
Core Deterministic 排序）、金鑰推導（blake3 derive_key vs HKDF-SHA256）、
tree 命名（密文 hash＋決定性 nonce vs 明文 keyed hash）、pack 排版
（LE/魔術字兩端 vs BE/只在尾部）、切塊（fastcdc crate v2020 vs 自殖
FastCDC）——兩邊的 repo 完全無法互開（基線證據 P6：Rust 開 Go repo 死在
`created` 欄位型別、Go 開 Rust repo 死在未知欄位 `version`）。

兩個 v1 都未發佈、沒有真實資料，所以 v2 是乾淨取代，不寫遷移。

## 決定

v2 的權威規格是兩邊各放一份、內容相同的 `docs/format.md`；每個決定都先以
跨語言 PoC 取得證據（對照表在規格 §18）。要點：

1. **CBOR**：RFC 8949 Core Deterministic（map keys 依編碼 bytes 排序）。
   Rust 在 ciborium 之上加 Value 層正規化（P1 證明與 fxamacker 逐 byte
   相同）。解碼忽略未知欄位（向前相容），但**絕不回寫**（write path 一律
   從事實來源重建）——這條規則讓「忽略欄位」不可能造成靜默資料丟失。
2. **tree 以明文 keyed hash 命名**、隨機 nonce、AAD＝自己的 ID、metadata
   不壓縮。v1 的密文命名把物件身分綁在 zstd 輸出上（P3：同一明文在兩個
   壓縮等級下名稱不同），而且依賴決定性 nonce——曾真實出過壓縮前後的
   bug（ADR 002）。整個 nonce key 隨之淘汰。
3. **無 envelope header**：sealed 物件＝`nonce‖ct‖tag`，AAD 依角色
   （tree＝ID、snapshot＝完整 key、trailer/index＝角色常數）。
4. **金鑰**：子金鑰 BLAKE3 DeriveKey（`kist/v2/{hash,chunk,meta,index}`），
   master AAD 綁 repo_id＋chunker 參數，Argon2id 64MiB/t3/**p4**。
   P4 以固定向量證明兩語言的 KEK、四把子金鑰與 sealed master 逐 byte
   相同。
5. **切塊**：以 Go 實作為準（gear 表＝fastcdc-go v0.2.0、normalized
   level 2 mask、前 min bytes 不雜湊），Rust 完整移植。P2 證明兩家 v1
   對同一輸入切出 3 塊 vs 2 塊（零互通）；移植後邊界完全一致。
   mask 由 `avg` 以整數 round-log2 推導，參數存 config（不再要求等於
   程式常數）。
6. **pack**：magic `kistpk`+u16 BE 版號在**頭尾各一份**、trailer 長度
   BE u64、entry 是短 key map `{i,o,l,r}`——`r`(raw_len) 是新欄位
   （mount/進度不必解密就知道明文大小；Go ADR 009 明言這是 v2 該補的）。
   chunk 壓縮位元在 AEAD 明文第一 byte（self-describing）。
7. **tree 結構**：扁平 entry（保留 v1 Rust 的分段＋間接清單＋ctime/inode
   快速路徑，加入 v1 Go 的硬連結 dev/ino/nlink 與 xattr bytes）。
   檔名一律 bytes；**根 tree 節點以完整絕對路徑命名**（兩個實作一致，
   restore 重建完整路徑）。
8. **index blob**：`supersedes`（重疊 prune 安全）＋per-pack size＋
   「名稱最小者贏」合併；明文首 byte 壓縮旗標＋zstd（P5：8192 packs
   時 8.8MB < v1 陣列形狀不壓縮的 12.1MB）。
9. **GC**：無資訊標記（固定 8 bytes `KISTGC2\n`）、時間取後端 mtime
   （截秒）、無 clients/ 註冊表（活躍度由 snapshot 推得）、backup 嚴格
   Put-only（絕不刪標記；被標記的 pack 不去重、chunk 重寫）、commit 前
   重新驗證引用 + BackupTooLong。
10. parity sidecar 照 Go v1（選配、明文、他端忽略）。

## 證據

- P1 CBOR：`crates/kist-format/tests/poc_cbor.rs` ↔ Go 臨時測試（已轉為
  永久 conformance 測試）。
- P2/P3/P4/P5 同規格 §18 所列。
- 端到端：Go init/backup → Rust snapshots/check/restore 逐 byte 相同；
  反向亦然；Rust 對 Go repo 再備份同資料 0 新 chunk（跨實作去重 100%）；
  Go forget+prune（兩階段＋supersedes index 重寫）後兩邊 check 乾淨、
  Rust 還原仍逐 byte 相同。

## 已知的實作差異（非格式分歧）

- **xattr**：格式兩邊都記錄；目前只有 Go client 擷取（`user.*` namespace），
  Rust client 尚未擷取。restore 兩邊都還不套用、都會警告。含 xattr 的資料
  請用 Go client 備份；Rust restore 遇到 xattr 會明確警告而非靜默丟失。

## 後果

- v1 repo 打不開（沒有任何真實 v1 repo，接受）。
- `fastcdc` 依賴移除；切塊成為第一方程式碼（與 Go 端共用同一張凍結表，
  兩邊以 digest 測試＋跨語言邊界 golden 釘死）。
- snapshot key 時間戳維持本 repo v1 形式（`YYYYMMDDTHHMMSSnnnnnnnnnZ`），
  Go 端改為手動格式化（Go layout 無法表達無小數點的九位數）。

## 修訂（2026-09-05，產品定位確定後）：欄位順序取代排序

kist-rs 確定為交付產品、Go 退居參考實作後重新審視了每個決定。唯一為
遷就 Go 而選的次級設計是 CBOR 的「map keys 依編碼 bytes 排序」：
fxamacker 的 CoreDet 模式免費做到，Rust 卻要 serialize→Value→sort→
serialize，**實測 encode 慢 20 倍**（10k 節點 tree 18.6ms vs 0.96ms），
而 backup 途中每個目錄都要 encode tree。

修訂：規格 §4 改為**欄位表釘死輸出順序**（spec 的欄位表 = 規範順序，
解碼端不得假設順序）。Rust 刪掉正規化層（encode 回到單次序列化），
Go 換掉 CoreDet 模式、struct 欄位順序對齊規格表（KeySlot、Stats 等），
xattrs map 的排序保留（Rust BTreeMap 天然、Go 自訂 marshaler）。對
第三方實作也更友善：serde 風格函式庫的預設行為就是宣告順序。bytes
全部重生成（goldens、共享 tree 向量），重新凍結。

同一輪補上產品缺口（規格本來就要求）：index 合併改為**未標記 pack
優先、其次名稱最小**（與 prune 正本 rank 一致——smallest-wins 會在
標記視窗內把 chunk 導向被標記副本，害 backup 重複重傳，測試
`marked_pack_is_not_used_for_dedup` 抓到）；解碼後的版本欄位檢查
（tree/snapshot/index/trailer）；trailer 一致性檢查（連續性、重複 ID、
長度上限，規格 §7）；**backup 記錄 xattr**（`user.*`，Go 端早有；
產品不能還原自己沒記錄的資料）；兩邊 CLI init 暴露 chunker 參數
（Go 順帶修掉 AAD 用常數而非 config 值的 latent bug）。

重新驗證：兩邊套件全綠、雙向 E2E 含 0-new-chunk 去重、encode
benchmark 回到 1.0x。
