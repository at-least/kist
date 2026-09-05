# 011 — v2：與 Rust 實作統一的儲存格式

日期：2026-09-05。狀態：已接受（實作完成）。

## 背景

kist 有兩個獨立實作：本 repo（Go）與 Rust 重寫（`kist-rs`）。
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

- P1 CBOR：跨語言 conformance 測試（向量由兩邊共用）。
- P2/P3/P4/P5 同規格 §18 所列。
- 端到端：Go init/backup → Rust snapshots/check/restore 逐 byte 相同；
  反向亦然；Rust 對 Go repo 再備份同資料 0 新 chunk（跨實作去重 100%）；
  Go forget+prune（兩階段＋supersedes index 重寫）後兩邊 check 乾淨、
  Rust 還原仍逐 byte 相同。

## 後果

- v1 repo 打不開（沒有任何真實 v1 repo，接受）。
- 切塊維持第一方程式碼（本來就是）；gear 表同時成為 Rust 端的凍結
  常數，兩邊以 digest 測試＋跨語言邊界 golden 釘死。
- snapshot key 時間戳改為 `YYYYMMDDTHHMMSSnnnnnnnnnZ`（大寫 T/Z、無小數
  點；Go layout 無法表達，改為手動格式化/解析）。
- tree 的根節點命名從 basename 改為完整絕對路徑 bytes（restore 重建完整
  路徑，與 Rust 一致）；clients/ 註冊表移除；backup 不再刪 gc 標記
  （Put-only）；xattr 改 byte-string key 的自訂 marshaler。
