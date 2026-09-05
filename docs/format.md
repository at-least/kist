# kist repo 格式（v2 — Go/Rust 統一版）

這份文件是 kist 儲存格式的**權威規格**，由 Go（`github.com/at-least/kist`）與
Rust（`kist-rs`）兩個實作共同遵守，兩邊各放一份**內容相同**的本文件；
改動格式必須同時改兩邊的程式碼、golden files 與本文件。

> 狀態：**v2，2026-09-05 定案**。v2 是兩個互不相容的 v1（Go 版與 Rust 版，
> 各自凍結、皆未發佈、無真實資料）的統一格式，直接取代兩者，不提供 v1
> 遷移。每個設計決定都以跨語言 PoC 實驗取得證據（見 §18 證據對照表）；
> 決策過程與 advisor 結論記錄在兩邊的 `docs/decisions/`（統一格式 ADR）。

## 1. 總覽

repo 是一個 key → bytes 的命名空間（本機目錄、S3 bucket…）。所有物件除了
`config` 與 `keys/*` 之外都是**不可變**的；寫入一律用 conditional put
（`PutIfAbsent`），同名即同內容：

| key | 內容 | 加密 | 命名 |
| --- | --- | --- | --- |
| `config` | `RepoConfig`（明文 CBOR） | 否（不含祕密） | 固定 |
| `keys/<slot hex>` | `KeySlot`（明文 CBOR），選配 | 否 | 使用者取 |
| `packs/<hex>` | pack：magic ‖ 加密 chunks ‖ 加密 trailer ‖ tail | 每 chunk 獨立 AEAD | BLAKE3(整檔 bytes)，無 key |
| `trees/<hex>` | sealed(`Tree`) | 整段 AEAD | **keyed** BLAKE3(明文 CBOR) |
| `indexes/<hex>` | sealed(`IndexBlob`)（可 zstd） | 整段 AEAD | BLAKE3(整檔 bytes)，無 key |
| `snapshots/<client hex>/<ts>` | sealed(`Snapshot`) | 整段 AEAD | client id + 時間 |
| `gc/<object hex>` | 待刪標記：固定 8 bytes `KISTGC2\n` | 否（不含資訊） | 被標記物件的名稱 |
| `parity/<pack hex>` | Reed-Solomon 同位（選配，明文 CBOR） | 否 | 對應的 pack |

「無 key BLAKE3」指對**寫進 repo 的 bytes** 做一般 BLAKE3，小寫 hex。任何人
不持金鑰都能驗證物件沒被改過；名稱不洩漏明文資訊。**tree 例外**：tree 的
名稱是對**明文 CBOR** 的 keyed BLAKE3（§2、§18 證據 P3）——目錄沒變，
名稱就絕對不變，與加密/壓縮的任何偶然無關。

**讀取端必須驗證名稱**：tree 讀出後重算 keyed hash 對名稱；pack / index
讀出後重算無 key hash 對名稱；snapshot 用 AAD（= 完整 key）綁定。

## 2. 識別碼

- `ChunkId`（32 bytes）＝ keyed BLAKE3(hash key, chunk 明文)。只出現在
  加密內容裡，**永遠不是** repo 的 key。
- tree ID ＝ keyed BLAKE3(hash key, tree 明文 CBOR bytes)。與 ChunkId
  同函式、同金鑰，只是輸入不同；作為 `trees/<hex>` 的名稱與 AEAD 的 AAD。
- pack / index blob 名稱 ＝ BLAKE3(物件密文 bytes)，無 key。

CBOR 內一律是 32-byte 的 byte string（major 2）；路徑裡一律小寫 hex。

## 3. 金鑰階層

```
password ─Argon2id(salt 16B, m=64 MiB, t=3, p=4)─▶ KEK (32B)
KEK ─XChaCha20-Poly1305 解開 slot 的 wrapped ─▶ master key (32B)
master ─BLAKE3 DeriveKey─▶
    "kist/v2/hash"    → ChunkId 與 tree ID 的 keyed hash key
    "kist/v2/chunk"   → chunk 加密
    "kist/v2/meta"    → tree 與 snapshot 加密
    "kist/v2/index"   → pack trailer 與 index blob 加密
```

- 子金鑰推導用 **BLAKE3 DeriveKey**（context 如上），不是 HKDF（§18 決策 5）；
  兩語言的 DeriveKey 已用固定向量驗證逐 byte 相同（證據 P4）。
- Argon2id 預設 64 MiB / t=3 / p=4（RFC 9106 第二組建議）；參數存在 config，
  讀取端有上限檢查（m ≤ 1 GiB、t ≤ 64、p ≤ 64、salt 恰 16 bytes）。
- `wrapped` 的 AAD ＝ `"kist/v2/master\0"`（15 bytes）‖ `repo_id`（16B）‖
  `chunker.min` ‖ `chunker.avg` ‖ `chunker.max`（各 u32 little-endian）。
  chunker 參數綁進 AAD：明文 config 被竄改時 master 解不開，而不是悄悄
  讓去重失效。`pack_target` 可調，不綁。
- **沒有 nonce key**：v2 沒有任何決定性 nonce（§5），這類 bug 整個消失
  （v1 Rust 曾真實發生 ADR 002 記錄的壓縮前後 bug）。
- 整條鏈（KEK、四把子金鑰、AAD 排版、sealed master 含 Poly1305 tag）
  已有跨語言測試向量（證據 P4），兩邊測試套件永久保留。

## 4. CBOR 慣例（規範編碼）

所有 metadata 都是 CBOR（RFC 8949）：

- 整數最短編碼；長度 definite。
- **struct 依規格中各結構的欄位表順序輸出——欄位表就是規範順序**。
  解碼端**不得假設順序**：按欄位名稱取值、忽略未知欄位。這讓任何
  serde 風格的實作用宣告順序就能編出規範 bytes，不需要排序（v2 草案
  曾要求 map keys 排序，實測讓 Rust 編碼慢 20 倍；2026-09-05 修訂，
  兩個實作都零成本）。
- 一般 map（目前只有 tree 的 `xattrs`）的 key 依 bytes 字典序排序
  （Rust 的 `BTreeMap` 天然如此；Go 的 xattrs 用自訂 marshaler）。
- 禁止把 struct 編成位置陣列。
- 位元組資料（名稱、ID、xattr）一律 byte string（major 2），不是整數陣列、
  不是 text string（Rust 需 `serde_bytes`/`ByteBuf`）。
- **解碼**：忽略未知欄位（向前相容的基礎）；拒絕重複 map key、indefinite
  length、尾端多餘 bytes。
- **絕不回寫（never round-trip）**：讀出的物件永遠不重新編碼後寫回。
  寫入端一律從事實來源（檔案系統走訪、pack 寫入器）重新構造。這條規則
  讓「忽略未知欄位」不可能造成靜默資料丟失。
- 新增欄位：一律有可省略語意（零值省略），插入位置依欄位表更新並同步
  兩個實作；舊版照樣能讀。

各結構的欄位表（= 輸出順序）：

| 結構 | 欄位順序 |
| --- | --- |
| `Tree` | v, entries, prev |
| `Entry` | n, t, mode, uid, gid, mtime, ctime, size, target, chunks, ct, tree, dev, ino, nlink, xattrs |
| `ChunkList` | v, chunks |
| `PackTrailer` | v, entries |
| `PackEntry` | i, o, l, r |
| `IndexBlob` | v, packs, supersedes |
| `IndexPack` | id, size, entries |
| `RepoConfig` | v, repo_id, created, chunker, pack_target, slot |
| `KeySlot` | v, name, created, kdf, wrapped |
| `KdfParams` | alg, t, m, p, salt |
| `ChunkerParams` | min, avg, max |
| `Snapshot` | v, root, time, host, user, paths, client, parent, stats |
| `SnapshotStats` | files, dirs, symlinks, bytes, chunks_new, chunks_read, packs_new, packs_revived, bytes_stored, errors, files_reused |

## 5. Sealed 物件（沒有 envelope header）

tree / snapshot / pack trailer / chunk 的密封形式都是：

```
nonce(24) ‖ ciphertext ‖ tag(16)        // XChaCha20-Poly1305
```

AAD 依角色：

| 物件 | AAD |
| --- | --- |
| chunk | 該 chunk 的 `ChunkId`（32 bytes） |
| tree | 該 tree 自己的 ID（32 bytes） |
| snapshot | 完整 key 路徑的 UTF-8 bytes（如 `snapshots/<client>/<ts>`） |
| pack trailer | 常數 `"kist/v2/pack-trailer"` |
| index blob | 常數 `"kist/v2/index"` |

- **全部用 OS 亂數 24-byte nonce**。v1 Rust 的 tree 決定性 nonce（密文命名
  的前提）隨密文命名一起淘汰（證據 P3）。
- 沒有 32-byte header（v1 Rust 的 envelope 淘汰）：kind 由 AAD 常數/路徑綁定，
  壓縮旗標只在需要的地方出現（index blob 的明文首 byte），版本在明文 CBOR
  的 `v` 欄位與 pack magic 裡。未認證的 bytes 減到最少（nonce 一種）。
- tree / snapshot 的明文 = 規範 CBOR，**不壓縮**（P3 的簡化；物件本來就小，
  且 tree 名稱在明文上，壓縮沒有意義）。

## 6. Chunk（資料塊）

payload = `algorithm byte ‖ 資料`：0 = 原文，1 = zstd。algorithm byte 是
**AEAD 明文的第一個 byte**（self-describing；v1 Go 設計）。壓縼規則：
zstd level 3，壓縮後若沒省下 > 1/16（6.25%）就存原文。解壓上限 =
`chunker.max`（防炸彈）。

讀取端解密後**重算 ChunkId** 對照索引——AEAD 證明 bytes 沒被換，
重算證明當初的 ID 沒有說謊。

## 7. Pack

```
+-------------------+----------------------+------------------+--------------------+-------------------+
| magic 8B          | chunk entry × N      | trailer (sealed) | trailer len u64 BE | magic 8B          |
+-------------------+----------------------+------------------+--------------------+-------------------+
```

- magic ＝ `"kistpk"` ‖ 版號 u16 big-endian（v2 = `0x0002`），**檔頭檔尾各一份**，
  兩處必須一致；trailer 的 `v` 也必須等於 2（三處一致，v1 Go 規則）。
- chunk entry ＝ `nonce(24) ‖ AEAD(chunk key, nonce, AAD=ChunkId, payload)`，
  entry 之間無分隔符，位置只記在 trailer。
- trailer 明文 = CBOR `{v:2, entries:[Entry]}`（不壓縮），以 index key 密封：

```
Entry { i: ChunkId, o: u64 起點(含 nonce), l: u64 長度(nonce+密文+tag), r: u64 明文長度 }
```

  `r`（raw_len）是 v2 新欄位：mount/進度/還原不需要先解密解壓就知道大小
  （v1 Go ADR 009 明言這是 v2 該補的欄位）。不需要 flags 欄位——壓縮位元
  在 chunk 明文的 algorithm byte 裡。
- trailer len 是 big-endian u64。讀取：先 range read 檔尾 16 bytes。
- pack 名稱 = BLAKE3(整檔 bytes)。目標大小 = `config.pack_target`
  （預設 64 MiB，寫滿或 backup 結束才 flush）。
- trailer 一致性檢查（讀取端強制）：entries 從檔頭 magic 之後連續排列、
  完整覆蓋資料區、無重複 ID、`l` ≤ chunker.max + 1 + 40。
- 讀取端對整個 pack 重算 BLAKE3 對名稱（`check --read-data` 時）。

## 8. Tree

```
Tree  { v:2, entries:[Entry...], prev: tree ID | null }
Entry {
  n:  檔名 byte string（Unix = 原 OS bytes；Windows = UTF-8）
  t:  類型 u8：0 檔案, 1 目錄, 2 符號連結
  mode: u32（含檔案類型位元；Windows = 0）
  uid, gid: u32（Windows = 0）          省略若 0
  mtime: i64 奈秒
  ctime: i64 奈秒（快速路徑用；0 = 沒有）省略若 0
  size: u64（僅檔案）                    省略若 0
  target: bytes（僅符號連結）            省略若空
  chunks: [ChunkId]（≤256 個，直接內嵌） 省略若空
  ct:  u8 內容型態：省略/0 = 直接；1 = 間接（chunks 指向 ChunkList 的資料塊）
  tree: 子目錄 tree ID（目錄分段時 = 最後一段）省略若 0
  dev, ino, nlink: u64（硬連結；nlink>1 才記）省略若 0
  xattrs: map<byte string, byte string>（鍵依規範排序）省略若空
}
ChunkList { v:2, chunks:[ChunkId] }
```

- `entries` 依 `n` 的 bytes 升冪排序，同一目錄內名稱不重複（讀取端驗證）。
- **根 tree 的節點名稱 = 備份來源的絕對路徑 bytes**（可含 `/`，如
  `/tmp/poc/go2-data`；讀取端驗證「絕對路徑、乾淨元件」）。子目錄的
  節點一律是單一路徑元件。restore 依此在目標底下重建完整絕對路徑。
- **tree 名稱 = keyed BLAKE3(明文 CBOR)**（P3）：同名必然同內容，加密、
  nonce、壓縮版本的偶然都不影響去重。AAD = 自己的 ID，讀取端重算 hash。
- **大目錄**：每 10 000 個節點切段，後一段的 `prev` 指向前一段；父目錄記
  **最後一段**；讀取沿 `prev` 收集後從最舊讀起（v1 Rust 設計）。
- **大檔案**：chunk 清單 > 256 個改間接——清單編成 `ChunkList`、當一般
  資料切塊入 pack，tree 只留 chunk ID（v1 Rust 設計；v1 Go ADR 004 明言
  這是 v2 該做的）。
- **硬連結**：`dev/ino/nlink` 記錄（v1 Go 設計），restore 優先重建連結、
  失敗降級為複本並警告。
- **xattr**：記錄；restore 套用失敗會回報而非假裝成功。Go 端以自訂
  marshaler 產生 byte-string key 的規範 map（Go 原生 map 做不到 byte key）。
- 空目錄 → `tree` 指向空 entries 的 tree；空檔案 → `chunks` 省略。
- 寫入端每次 backup 一律重新 put 每個 tree（幂等）；冪等性由明文命名保證。

## 9. Snapshot

key：`snapshots/<client hex>/<ts>`，`ts` = `YYYYMMDDTHHMMSSnnnnnnnnnZ`
（UTC、奈秒 9 位、無冒號；字典序 = 時間序，Windows 安全）。寫入必須
conditional put；同 key 已存在就前進 1 奈秒重試（有上限）。

```
Snapshot {
  v: 2,
  root: tree ID（根目錄分段時 = 最後一段）,
  time: i64 奈秒（= backup 開始時刻，與 key 同一瞬間；讀取端核對一致）,
  host: text,
  user: text,                              省略若空
  paths: [byte string],
  client: bytes(16)（與 key 的 client hex 必須一致）,
  parent: text | null（上一個 snapshot 的 key；僅加速，可null）,
  stats: { files, dirs, symlinks, bytes, chunks_new, chunks_read, packs_new,
           packs_revived, bytes_stored, errors, files_reused }（全部可省略）
}
```

`client` 由每台機器隨機產生存本機（不是 hostname）。AAD = 完整 key。
快速路徑（v1 Rust 設計）：parent 存在且 `paths` 相同時，size + mtime +
ctime + inode 都沒變、且 mtime/ctime 早於 parent 開始時間（防 racy clean）
的檔案直接沿用 chunk 清單。

## 10. Index blob

key：`indexes/<BLAKE3(密文) hex>`。明文 = `algorithm byte ‖ CBOR`：

```
IndexBlob { v:2, packs:[Pack...], supersedes:[名稱...] }
Pack      { id: 名稱, size: u64（pack 檔總長）, entries:[Entry（§7 同型）] }
```

- 以 index key 密封，AAD = `"kist/v2/index"`；algorithm byte 同 chunk
  （0 原文 / 1 zstd），壓縮用同樣的 1/16 門檻。實測 map 形狀 + zstd
  比 v1 陣列形狀不壓縮還小（8192 packs：8.8 MB vs 12.1 MB；證據 P5）。
- 讀取：先讀所有 blob，收集全部 `supersedes`，被任何**有效** blob 列到的
  整個忽略（v1 Rust 設計）——兩個 prune 重疊、或 prune 途中 rebuild-index
  都安全；被忽略 blob 獨有的 pack 是「幽靈」，不參加正本選擇，下次
  index 重寫時丟掉。
- 合併規則：同一 chunk 出現在多個 pack → **未標記 pack 優先，其次名稱
  最小者贏**——與 prune 的正本選擇（§13.1）同一個 rank。持有標記集合的
  讀取端（backup 開始時列出 `gc/`）用完整規則；沒有標記資訊的讀取端
  （restore、check）用「名稱最小」即可，任一副本都讀得到資料。純函數：
  給定 pack 集合與標記集合，結果與載入順序無關。
- `size` 讓 `check` 不讀資料就能抓出被截斷/換掉的 pack（HEAD 比對）。
- index 只是 pack trailer 的快取，可由所有 pack 重建（`rebuild-index`）。

## 11. Config 與 KeySlot

key `config`，明文規範 CBOR（唯一可覆寫的物件：換密碼、改 pack_target）：

```
RepoConfig {
  v: 2,
  repo_id: bytes(16),
  created: i64 奈秒,
  chunker: { min: u32, avg: u32, max: u32 },   預設 512 KiB / 2 MiB / 8 MiB
  pack_target: u64,                            預設 64 MiB
  slot: KeySlot                                （slot 0）
}
KeySlot {
  v: 2,
  name: text,                                  省略若空
  created: i64 奈秒,
  kdf: { alg: "argon2id", t: u32, m: u32(KiB), p: u32, salt: bytes(16) },
  wrapped: bytes = nonce(24) ‖ 密文(32) ‖ tag(16)
}
```

參數範圍（讀取端強制）：`min` 64..=1 MiB、`avg` 256..=16 MiB、
`max` 1 KiB..=64 MiB、min ≤ avg ≤ max；`pack_target` 64 KiB..=4 GiB 且
≥ `max`。額外 slot 寫 `keys/<slot hex>`（明文 KeySlot，只增不改）。

## 12. 切塊（FastCDC）

兩個實作必須對同 bytes + 同參數產出**完全相同的邊界**（P2 證明 v1 兩家
不同：Go 3 塊 vs Rust 2 塊）。v2 以 Go 的實作為準：

- FastCDC（Xia et al. 2016）+ normalized level 2；
- gear 表 = fastcdc-go v0.2.0 的 256×u64 表，**逐 byte 凍結**
  （LE 串接 SHA-256 = `a98fa4184eb747cd769328307242285f9d4b25afeb28f71ef3fe437a4c278e28`）；
  兩邊程式碼各自內嵌同一張表並以 digest 測試釘死；
- 邊界函式：hash 從 `min` 開始算（前 min bytes 不參與）、尚未滿 avg 用
  `mask_s`、超過 avg 用 `mask_l`、硬邊界 `max`、尾巴不足 min 就整段；
- mask 由 avg 推導：`bits = round(log2(avg))`、
  `mask_s = (1<<(bits+2))-1`、`mask_l = (1<<(bits-2))-1`
  （avg=2 MiB 時 bits=21：`0x00ff_ffff` / `0x0007_ffff`）；
- 參數來自 config（不是寫死），讀取端驗範圍；
- 緩衝無關性：讀取器必須保證掃描位置之後 ≥ max bytes（或輸入結束），
  使邊界與 reader 分塊方式無關；
- 跨語言 golden：固定輸入的邊界清單存兩邊 testdata，CI 互驗。

## 13. GC

- 標記 `gc/<名稱>`，內容固定 8 bytes `KISTGC2\n`（**不帶任何資訊**：
  什麼時候標記看後端修改時間，標記什麼看名稱）。conditional put 寫入，
  已存在不重寫（時間不重設）。pack / tree / index 共用一個命名空間。
- 沒有 `clients/` 註冊表：活躍與否由 snapshot 推得（§13.2）。
- 時間一律**取整到秒**（S3 list/head 精度不同；本機 mtime 截秒）：
  同秒內「重寫過」與「標記」分不出先後時，prune 當重寫過（不刪）、
  backup 當沒重寫（不 commit）——都取安全側。

### 13.1 活的定義（每次 prune 從 snapshot 重算）

- tree：任一 snapshot 走得到（含 `prev` 鏈）。
- pack：某被引用 chunk 的**正本**。正本 = 在有效 index 持有該 chunk 的
  pack 中，未標記者優先、其次名稱最小者；幽靈 pack 不參加。
- index blob：未被任何有效 blob `supersedes`。
- 引用不完整（任何 snapshot/tree/index 讀不出、或被引用 chunk 無法解析）
  → prune 整個拒絕：不標、不刪。

### 13.2 兩階段

1. 不活且修改時間距今超過 grace（預設 72 h）→ 寫標記。活的 → 撤銷標記。
2. 標記超過 grace，且每個**活躍 client**（`inactive_after` 預設 30 天內
   有 snapshot）在標記（+`clock_skew`，預設 1 h）之後都有新 snapshot
   （比較 backup 開始時間，保守方向）→ 刪。刪有效 index 裡的 pack 之前
   必須先寫一個不含它的新 index blob（`supersedes` 全部既有 blob）。
   刪前再 HEAD 一次：物件在標記後被重寫過 → 撤銷。
3. repack（選配）：活的、比 grace 老、正本 bytes 比例低於門檻的 pack，
   把正本 chunk（解密驗證後）搬進新 pack；新 blob 列出新 pack 與全部
   既有 pack（新在前、被 repack 者在最後）；舊 pack 留在 index 並標記，
   走 1–2。

### 13.3 backup 的義務（嚴格 Put-only）

- 開始時列出 `gc/`：被標記的 pack **不拿來去重**，裡面的 chunk 重寫一份。
  backup **絕不刪標記**（維持 Put+Get+List 最小權限）。
- 從開始到寫 snapshot 之前若已過 grace（減 1 h 安全邊界）→ 一律不寫
  snapshot、以錯誤結束（`BackupTooLong`；重跑會沿用已上傳資料）。
- 寫 snapshot 前重新載入 index：這次引用的每個 chunk 都要解析得到、
  其 pack 要存在且標記未超過 grace；這次 put 過的 tree 若有超過 grace
  的標記，修改時間必須比標記新。任一不成立 → 不 commit、以錯誤結束。
- restore 端 chunk 的 pack 不見就重載 index 再試（repack 搬走了）。

### 13.4 安全性假設

grace 長於最長 backup；同一 client 一次一個 backup（本機檔案鎖）；
建議一個 repo 只排程一個 prune（重疊也安全，代價是多一輪 index 重寫）；
versioning bucket 刪的只是現行版本，清空間要 lifecycle；Object Lock
保護中的物件刪不掉，prune 回報並保留標記；後端必須支援條件寫入。

## 14. Parity（選配 sidecar）

`parity/<pack hex>`：明文 CBOR `{v:2, k:16, m:1..8, pack_size, shard_len,
hashes:[16+m 個 BLAKE3], parity:[m 個 shard]}`——Reed-Solomon 對**整個
sealed pack** 切 16 資料片。寫入端可選；讀取端**忽略**。修復僅在重算
BLAKE3 == pack 名稱時接受。（v1 Go 設計，照搬。）

## 15. 後端契約

`Put`（僅 config）、`PutIfAbsent`（條件寫；完成前不可見）、`Get`（含
range read，強制能力）、`List`、`Stat`、`Delete`（維護角色專用）。
本機 PutIfAbsent 用 `link(2)`；S3 用 `If-None-Match: *`。key 字法：
`[a-z0-9._-]` 區段、無前導點、無冒號、≤ 1024 bytes。

## 16. 版本與演進

- `config.v`、各明文 `v`、pack magic 版號全部 = **2**；讀取端遇到 ≠2 拒絕。
- 加新欄位：一律「零值省略 + `#[serde(default)]` / Go 指標或自訂解碼」，
  配合 §4 的忽略未知欄位與 never-round-trip 規則，兩方向新舊互讀。
- 需要動到金鑰推導、AAD、magic 的改動 → v3，靠 config 版號談判。

## 17. 寫入順序（commit point）

backup：(packs、trees，走訪途中交錯) → index blob → snapshot。
snapshot 是唯一 commit point：它出現之前的新物件都是可回收垃圾；
它出現之後，它引用的東西都已在 repo。

## 18. 設計決定 × 證據對照

| # | 決定 | 取自 | 證據 |
| --- | --- | --- | --- |
| 1 | CBOR：規格釘死欄位順序（map keys 排序僅 xattrs）+ 忽略未知欄位 + never-round-trip | 產品優先修訂 | 實測排序正規化讓 Rust encode 慢 20 倍（10k 節點 tree 18.6ms vs 0.96ms）；排序是「Go 免費、Rust 付費」的選擇，2026-09-05 修訂為欄位表順序 |
| 2 | tree 以明文 keyed hash 命名；隨機 nonce；AAD=自 ID；metadata 不壓縮 | Go v1 | P3：zstd 版本改變 → 密文命名改名（e4679e→858150）、明文命名不變；v1 Rust 有實際 bug 記錄 |
| 3 | 無 envelope header；per-role AAD | Go v1 | 決策 review：AAD 常數/路徑綁 kind，安全性等價、bytes 更少 |
| 4 | chunk 壓縮位元在明文首 byte；trailer entry 留 {i,o,l,r} 含 raw_len | Go v1 + Rust v1 | Go ADR 009 明言 v2 要 raw_len；advisor：map 優於位置陣列（可演化） |
| 5 | 子金鑰 BLAKE3 DeriveKey；Argon2id 64MiB/t3/p4；AAD 綁 repo_id+chunker | 兩邊折衷 | P4：KEK/子金鑰/AAD/sealed master 跨語言逐 byte 相同 |
| 6 | FastCDC 以 Go 實作為準（表+mask+邊界函式），參數進 config | Go v1 | P2：兩家 v1 同輸入 3 塊 vs 2 塊，零互通 |
| 7 | tree：分段 + 間接清單 + ctime/inode 快速路徑 + 硬連結 + xattr + bytes 檔名 | 兩邊合併 | Go ADR 004/009（v2 該做間接與長度）；Rust ADR 002 |
| 8 | snapshot：parent、client bytes16、ns int、AAD=key、T/Z 時間格式、+1ns 重試 | 兩邊合併 | 決策 review |
| 9 | index：supersedes + per-pack size + 最小名稱贏 + 明文首 byte zstd | 兩邊合併 | P5：8192 packs map+zstd 8.8MB < v1 陣列 12.1MB；Rust ADR 004/005（supersedes 安全性） |
| 10 | GC：無資訊標記、無 clients/、Put-only backup、repack、clock_skew | Rust v1 + Go knob | 決策 review（權限模型：backup 不該有 Delete） |
| 11 | parity 為選配 sidecar，他端忽略 | Go v1 | 決策 review |
| 12 | 兩個 v1 直接淘汰，無遷移 | — | 兩邊 format.md 皆記錄「未發佈、無真實 repo」 |

永久 conformance 測試（跨語言共用向量）：
- 切塊邊界＋金鑰推導＋tree 規範 CBOR：Rust `kist-chunker/tests/interop.rs` 與
  `kist-format/tests/interop.rs` ↔ Go `internal/interop/`（同一份
  testdata 向量，兩邊各自斷言）。
- AEAD 密封 master（固定 nonce 向量）：`kist-crypto/tests/poc_keys.rs`。
- 命名方案對照（明文 vs 密文）：`kist-crypto/tests/poc_tree_naming.rs`。
- CBOR 正規化：`kist-format/tests/poc_cbor.rs`（釘 Go 產生的 hex）。
