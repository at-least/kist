# kist repo 格式（v3）

> **狀態：v3，2026-09-09 定案。** 本文件是唯一權威副本，由 kist-rs（產品）
> 持有；kist-go 的 `docs/format.md` 是它的逐 byte 拷貝，由該 repo 的 CI
> 檢查一致性（§0.9）。改動格式必須同時改兩邊的程式碼、golden files 與
> 本文件。

v3 = v2 的外科手術修訂 ＋「備份兩個遠端之間」需求（遠端來源）的格式支援。
v2 的核心架構（pack、每 chunk AEAD、明文 keyed tree 命名、欄位表 CBOR、
凍結 FastCDC、supersedes index、無資訊兩階段 GC、Put-only backup、
snapshot 唯一 commit point、parity sidecar）**全部保留**——它們有跨語言
PoC 與 200-case proptest 背書，重開等於歸零重驗。

## 0. v2 → v3 變更總覽（每條對應一個教訓或需求）

| # | 變更 | 動機（教訓/需求出處） |
| --- | --- | --- |
| 0.1 | 殺掉合成根：`Snapshot.roots = [{path, tree}]`，樹節點名**永遠**是單一路徑元件 | 合成根是 stats 口徑分裂點（2026-09-08 釘定）；`s3://bucket/prefix` 塞不進絕對路徑節點模型 |
| 0.2 | snapshot.stats 只留資料事實（files/dirs/symlinks/bytes），過程計數移出格式 | 依 GC 狀態而變的數字（packs_revived…）讓兩實作「合法地」分歧 |
| 0.3 | 不變式 config 進 wrapped master 密文；明文 config 降級為必須相符的 hint | v2 的 AAD 列舉欄位每加一欄就要改排版；tamper 偵測應無條件 |
| 0.4 | tree 復活改 `touch/<id>`（8 bytes、**覆寫式 Put**——mtime 必須刷新，見 §13.1 的 advisor 審查修正），不再每次 backup 重 put 全部樹 | v2 的樹重 put 是寫放大（S3 上整份 bytes 重傳）＋§6a HEAD→DELETE 視窗與逐樹 commit HEAD 的根源（ADR 003 未兌現承諾）；初版草案的 PutIfAbsent touch 有「第二次 backup 不刷新」的刪除競態（§13.1） |
| 0.5 | trees/snapshots 選配副本 `<id>.r1`（同 bytes、Put-if-absent） | packs 有 parity 但不可重建的 metadata 反而沒有保護；advisor 否決 RS-bundle（第二真相源有幽靈問題），副本＝k=1 的 RS、零新概念 |
| 0.6 | config 加 `min_reader`：版本懸崖變明確拒絕 | 備份工具常態是混合版本 client；「解碼到 v≠N 就拒絕」半讀半猜 |
| 0.7 | index 壓縮觸發寫進規格：有效 blob > 64 時 prune 必須合併 | ADR 005 明載未做；只增不刪的 repo 每次 backup 多一顆 blob |
| 0.8 | Entry metadata 改 kind 聯集（posix/sftp/s3/generic）＋ `etag`/`vern` 欄位 | 遠端來源需求；修 v2 的 uid=0（root vs 不存在）與 mode=0（Windows 偽裝）歧義；快速路徑改「內容可證明」 |
| 0.9 | 規格過程：kist-rs 持唯一權威副本，kist-go 的副本由 CI 檢查逐 byte 一致；每條規範性規則掛向量編號 | 「兩邊各放一份內容相同」是漂移風險；§18 的做法常態化 |

不改的（證據已釘死）：pack 排版、金鑰階層形狀、CBOR、FastCDC、GC 兩階段
骨架、parity、後端契約能力集。

---

## 1. 總覽

repo 是一個 key → bytes 的命名空間（本機目錄、S3 bucket、SFTP…）。所有物件
除了 `config` 與 `keys/*` 之外都是**不可變**的；寫入一律用 conditional put
（`PutIfAbsent`），同名即同內容：

| key | 內容 | 加密 | 命名 |
| --- | --- | --- | --- |
| `config` | `RepoConfig`（明文 CBOR） | 否（不含祕密） | 固定 |
| `keys/<slot hex>` | `KeySlot`（明文 CBOR），選配 | 否 | 使用者取 |
| `packs/<hex>` | pack：magic ‖ 加密 chunks ‖ 加密 trailer ‖ tail | 每 chunk 獨立 AEAD | BLAKE3(整檔 bytes)，無 key |
| `trees/<hex>` | sealed(`Tree`) | 整段 AEAD | **keyed** BLAKE3(明文 CBOR) |
| `trees/<hex>.r1` | 同上之副本（選配，§13.5） | 同上 | 主體名＋`.r1` |
| `indexes/<hex>` | sealed(`IndexBlob`)（可 zstd） | 整段 AEAD | BLAKE3(整檔 bytes)，無 key |
| `snapshots/<client hex>/<ts>` | sealed(`Snapshot`) | 整段 AEAD | client id + 時間 |
| `snapshots/<client hex>/<ts>.r1` | 同上之副本（選配） | 同上 | 主體名＋`.r1` |
| `gc/<object hex>` | 待刪標記：固定 8 bytes `KISTGC3\n` | 否（不含資訊） | 被標記物件的名稱 |
| `touch/<tree hex>` | 復活訊號：固定 8 bytes `KISTTC3\n`（**覆寫式 Put**，§13.1） | 否（不含資訊） | 樹名稱 |
| `parity/<pack hex>` | Reed-Solomon 同位（選配，明文 CBOR） | 否 | 對應的 pack |

命名與驗證規則沿用 v2：「無 key BLAKE3」指對寫進 repo 的 bytes 做一般
BLAKE3（小寫 hex）；tree 例外——名稱是對**明文 CBOR** 的 keyed BLAKE3
（目錄沒變，名稱就絕對不變）。讀取端必須驗證名稱（tree 重算 keyed hash、
pack/index 重算無 key hash、snapshot 用 AAD 綁 key）。副本 `.r1` 內容與
主體逐 byte 相同，名稱驗證同樣適用（副本不是獨立物件，是同一物件的重複
存放）。

## 2. 識別碼

沿用 v2，無變更：

- `ChunkId`（32 bytes）＝ keyed BLAKE3(hash key, chunk 明文)。只出現在加密
  內容裡，永遠不是 repo 的 key。
- tree ID ＝ keyed BLAKE3(hash key, tree 明文 CBOR bytes)；作為
  `trees/<hex>` 名稱與 AEAD 的 AAD。
- pack / index blob 名稱 ＝ BLAKE3(密文 bytes)，無 key。
- CBOR 內一律 32-byte byte string（major 2）；路徑裡一律小寫 hex。

## 3. 金鑰階層

```
password ─Argon2id(salt 16B, m=64 MiB, t=3, p=4)─▶ KEK (32B)
KEK ─XChaCha20-Poly1305 解開 slot 的 wrapped ─▶ master key (32B) ‖ 不變式 CBOR
master ─BLAKE3 DeriveKey─▶
    "kist/v3/hash"    → ChunkId 與 tree ID 的 keyed hash key
    "kist/v3/chunk"   → chunk 加密
    "kist/v3/meta"    → tree 與 snapshot 加密
    "kist/v3/index"   → pack trailer 與 index blob 加密
```

- 子金鑰推導沿用 BLAKE3 DeriveKey；Argon2id 預設 64 MiB / t=3 / p=4，參數
  與上限檢查沿用 v2。
- **wrapped 的明文 payload ＝ master(32B) ‖ 不變式 CBOR**（v3 新）：

  ```
  Invariants { v: 3, repo_id: bytes(16), chunker: { min, avg, max } }
  ```

  AAD ＝ 常數 `"kist/v3/master"`。解鎖時從**通過 Poly1305 認證的密文**取出
  權威參數，與明文 `config` 比對：`repo_id` 或 `chunker` 不符 → 明確錯誤
  「config 已被竄改」（不是 v2 的「默默解不開」，更不是去重悄悄失效）。
  未來加不變式欄位：加進 Invariants（零值省略＋忽略未知＋never-round-trip
  規則適用），**不需要動 AAD 排版**——這是對 v2 列舉式的改進。
- 非 invariant（可調、不綁）：`pack_target`、`replicas`、`min_reader`。
- 沒有 nonce key：全部隨機 24-byte nonce（沿用 v2）。
- Argon2 參數在 `KeySlot.kdf`（明文，解密前就需要）；換密碼＝重包 wrapped。

## 4. CBOR 慣例（規範編碼——條件完整版）

所有 metadata 都是 CBOR（RFC 8949）。**規範編碼的完整條件**（第三個實作
憑本節就必須能編出與兩個現有實作逐 byte 相同的 bytes；違反任一條＝解碼
拒絕或編碼非法）：

1. 整數一律最短編碼。
2. 所有長度 definite；**禁止 indefinite-length**（任何型別）。
3. **禁止浮點數**：所有數字都是整數（時間 i64 奈秒、大小 u64、列舉 u8）。
4. **禁止 CBOR tags**（tag 0/1 日期之類一律不用）。
5. struct ＝ map，**依規格欄位表順序輸出**（各結構欄位表見 §4.1）。
   解碼端不得假設順序，按欄位名取值。
6. 二進位資料（名稱、ID、xattr、etag、version）一律 byte string（major 2）；
   人類可讀字串（host、user、parent、slot name）用 text string。
7. 唯一的巢狀自由 map：`xattrs`，key 依 bytes 字典序排序。
8. **解碼拒絕**：重複 map key、indefinite length、float、tag、尾端多餘
   bytes、欄位表宣告「必須缺席」卻在場的欄位（§8 的 per-kind 規則）。
9. 忽略未知欄位（向前相容），但**絕不回寫**：寫入端一律從事實來源重新
   構造，讀出的物件永不重編碼後寫回。
10. 新增欄位：一律可省略語意（零值省略），插入位置依欄位表更新並同步
    兩個實作。

### 4.1 欄位表（＝輸出順序）

| 結構 | 欄位順序 |
| --- | --- |
| `Tree` | v, entries, prev |
| `Entry` | n, t, mk, size, target, ct, chunks, tree, mode, uid, gid, mtime, ctime, dev, ino, nlink, xattrs, etag, vern |
| `ChunkList` | v, chunks |
| `PackTrailer` | v, entries |
| `PackEntry` | i, o, l, r |
| `IndexBlob` | v, packs, supersedes |
| `IndexPack` | id, size, entries |
| `RepoConfig` | v, repo_id, created, chunker, pack_target, min_reader, replicas, slot |
| `KeySlot` | v, name, created, kdf, wrapped |
| `KdfParams` | alg, t, m, p, salt |
| `ChunkerParams` | min, avg, max |
| `Snapshot` | v, roots, time, host, user, client, parent, stats |
| `Root` | path, tree |
| `SnapshotStats` | files, dirs, symlinks, bytes |

## 5. Sealed 物件（沒有 envelope header）

沿用 v2：`nonce(24) ‖ ciphertext ‖ tag(16)`（XChaCha20-Poly1305），
AAD 依角色：

| 物件 | AAD |
| --- | --- |
| chunk | 該 chunk 的 `ChunkId`（32 bytes） |
| tree | 該 tree 自己的 ID（32 bytes） |
| snapshot | 完整 key 路徑的 UTF-8 bytes（如 `snapshots/<client>/<ts>`；**不含** `.r1`——副本驗的是主體的 AAD） |
| pack trailer | 常數 `"kist/v3/pack-trailer"` |
| index blob | 常數 `"kist/v3/index"` |

全部 OS 亂數 24-byte nonce；tree/snapshot 明文＝規範 CBOR、不壓縮。

## 6. Chunk（資料塊）

沿用 v2：payload ＝ `algorithm byte ‖ 資料`（0 原文 / 1 zstd level 3，
省不到 1/16 存原文，解壓上限＝chunker.max）。讀取端解密後重算 ChunkId。

## 7. Pack

沿用 v2，版號改 3：

```
+-------------------+----------------------+------------------+--------------------+-------------------+
| magic 8B          | chunk entry × N      | trailer (sealed) | trailer len u64 BE | magic 8B          |
+-------------------+----------------------+------------------+--------------------+-------------------+
```

- magic ＝ `"kistpk"` ‖ u16 big-endian `0x0003`，頭尾各一份、必須一致；
  trailer 的 `v` ＝ 3（三處一致）。
- trailer entry 沿用 `{i, o, l, r}`（含 raw_len）；一致性檢查沿用
  （連續排列、完整覆蓋、無重複 ID、長度上限）。
- pack 名稱＝BLAKE3(整檔)；目標大小＝`pack_target`（預設 64 MiB）。
- **pack 不做 `.r1` 副本**：parity sidecar 已涵蓋（兩種保護並存只增加
  GC 生命週期複雜度）。

## 8. Tree 與 Entry（metadata 聯集）

```
Tree  { v:3, entries:[Entry...], prev: tree ID | null }
Entry {
  n:    檔名 byte string——**一律單一路徑元件**（任何來源、含根目錄；
        讀取端驗證：非空、不含 `/`、不是 `.` 或 `..`）
  t:    類型 u8：0 檔案, 1 目錄, 2 符號連結
  mk:   metadata 種類 u8：0 posix, 1 sftp, 2 s3, 3 generic
  size: u64（僅檔案）                        省略若 0
  target: bytes（僅符號連結）                 省略若空
  ct:   u8：省略/0 直接；1 間接               省略若 0
  chunks: [ChunkId]（≤256，直接）             省略若空
  tree: 子目錄 tree ID（分段＝最後一段）       省略若 null
  ── 以下為 per-kind 欄位，意義由 mk 決定 ──
  mode, uid, gid, mtime, ctime, dev, ino, nlink, xattrs   （mk=0）
  mode, uid, gid, mtime                                   （mk=1）
  etag, vern, mtime                                       （mk=2）
  mtime                                                   （mk=3）
}
ChunkList { v:3, chunks:[ChunkId] }
```

### 8.1 per-kind 欄位規則（讀取端強制）

| mk | 必填 | 選填 | 必須缺席 |
| --- | --- | --- | --- |
| 0 posix | mode, uid, gid, mtime | ctime, dev, ino, nlink, xattrs, etag, vern | — |
| 1 sftp | mtime | mode, uid, gid | ctime, dev, ino, nlink, xattrs, etag, vern |
| 2 s3 | — | mtime, etag, vern | mode, uid, gid, ctime, dev, ino, nlink, xattrs |
| 3 generic | — | mtime | 其餘全部 |

- **posix 的 uid/gid 必填**（修 v2 歧義）：uid 0 是 root、是真實值，「沒
  記錄」在 posix 來源不存在。s3 欄位全選填：S3 沒有 mode/uid 概念，
  缺席＝來源沒有，而不是 0（v2 的「mode=0 偽裝 Windows」消除）。
- `mtime` 一律 i64 奈秒；s3/sftp 來源只有秒精度（奈秒補 0）。**快速路徑
  比較用來源精度**：s3/sftp 截秒再比。
- `etag`：來源提供的內容 hash（bytes，原樣保存——S3 ETag 十六倍字串、
  未來其他後端的 digest）。`vern`：來源物件版本 ID（bytes；S3 versioning
  開啟時的 VersionId，做 point-in-time 一致性）。
- 其餘沿用 v2：entries 依 `n` bytes 升冪、同目錄名稱不重複；分段（10 000
  節點、`prev` 鏈、父記最後一段）；間接清單（chunks > 256 改 ChunkList
  入 pack）；硬連結（mk=0 的 dev/ino/nlink，`nlink>1` 才記）。

### 8.2 快速路徑合約（內容可證明 > kernel 可證明 > 來源聲稱）

| 來源 | 沿用條件 | 依據 |
| --- | --- | --- |
| mk=0 posix | size + mtime + ctime + inode + dev 都沒變 | kernel 維護，使用者改不了（v2 規則） |
| mk=2 s3 | etag 相同 + size 相同 | etag 是來源**計算並保證**的內容指紋（單段上傳＝MD5；多段/KMS 的 etag 仍是來源定義的確定性指紋） |
| mk=1 sftp / mk=3 | **無安全快速路徑**——一律重讀，靠 chunk 去重吸收 | mtime/size 皆 client 可設（`cp -p` 陷阱） |

mtime-only 沿用**非法**（任何來源）。client 可提供 `--trust-mtime` 顯式
opt-in，但那是 client 政策，規格不背書其安全性。`etag`/`vern` 存在 Entry
裡就是為了下一次 backup 能做這個比較——這是格式欄位，不是 client 記憶體
狀態（兩個實作、兩台機器都要能比）。

### 8.3 硬連結範圍

`(dev, ino)` 的同一性判定範圍＝**整個 snapshot（跨 roots）**：兩個 root
裡共用 (dev,ino) 的條目是同一份內容——`bytes` 只算一次、restore 重建為
一個 hard link（第一個出現的位置建立，其餘 link；失敗降級複本＋警告）。

## 9. Snapshot

key：`snapshots/<client hex>/<ts>`，`ts` 沿用
`YYYYMMDDTHHMMSSnnnnnnnnnZ`；conditional put、+1ns 重試，沿用 v2。

```
Snapshot {
  v: 3,
  roots: [Root...]        // ≥1；依 path bytes 升冪排序、不重複（讀取端驗證）
  time: i64 奈秒（= backup 開始時刻，與 key 同瞬間）,
  host: text（client 主機名）,
  user: text,                              省略若空
  client: bytes(16)（與 key 的 client hex 一致）,
  parent: text | null（上一個 snapshot 的 key；僅加速）,
  stats: { files, dirs, symlinks, bytes }  // 全部可省略
}
Root {
  path: byte string,   // 不透明來源定位：本機絕對路徑 / sftp://host[:port]/path / s3://bucket/prefix
  tree: tree ID        // 根目錄**內容**的 tree（entries＝根目錄的子女）
}
```

- **沒有合成根**：根目錄本身不是任何 tree 的 entry（v2 的絕對路徑節點消
  失）；`paths` 欄位刪除（roots 取代）。
- restore 映射（兩實作必須一致）：root `path` 去掉 scheme 後以 `/` 切
  段、映射到 `<target>/` 之下（`/srv/data` → `target/srv/data`；
  `s3://bucket/prefix` → `target/bucket/prefix`）。**檔案/symlink 來源**
  （root tree 恰好一個非目錄 entry、名稱＝定位末段）落在
  `target/<定位去掉末段>/<名稱>`——與 v2 絕對路徑還原的落點一致；「目錄
  恰好只含一個同名檔案」的還原結果也相同，判別沒有歧義代價。定位切不
  出組件（`/`、`s3://bucket/`）時，內容直接落在 `target`（mount 對應
  攤平到頂層）。鎖進向量。
- parent 快速路徑資格：parent 存在且 **roots 清單（path 集合與順序）相同**。

### 9.1 stats 口徑（只留資料事實；依 mk 定義）

- `files` / `symlinks`：按**名稱**計（hard link 的每個名字各算一個）。
- `dirs`：目錄 **entry** 數（樹裡的子目錄節點）。**roots 不算**（root 是
  path，不是 entry——v2「含備份來源目錄本身」的特殊案例消除）。s3 來源
  沒有目錄 entry → `dirs=0`，自然成立。
- `bytes`：檔案內容總和，**(dev,ino) 群組只算一次**（跨 roots）；無
  hardlink 資訊的條目按名稱各算。
- 檔案按名稱、bytes 按內容是刻意的不對稱（v2 裁定沿用）。
- **過程計數（chunks_new、packs_new、packs_revived、bytes_stored、
  errors、files_reused、chunks_read）不存在於格式**——屬於 backup 的
  報告輸出（CLI/JSON），依 GC 狀態與去重順序而變，不可變結構不收。

## 10. Index blob

沿用 v2（`IndexBlob { v:3, packs, supersedes }`、zstd 1/16 門檻、
supersedes 忽略規則、未標記優先＋名稱最小的純函數合併、per-pack size）。
**新增壓縮觸發（規範性）**：有效（未被 supersede）index blob 數 > 64 時，
prune **必須**合併重寫為一顆（supersedes 全部）；backup/restore 讀取端
不因 blob 數量增長而退化。64 的選擇：每 blob 對讀取端是一次 Get＋合併
成本，64 顆以內時本地快取增量合併的代價可忽略。

## 11. Config 與 KeySlot

```
RepoConfig {
  v: 3,
  repo_id: bytes(16),                        // 必須 == Invariants.repo_id（不符＝tamper 錯誤）
  created: i64 奈秒,
  chunker: { min, avg, max },                // 必須 == Invariants.chunker（同上）
  pack_target: u64,                          預設 64 MiB，可調（非 invariant）
  min_reader: u16,                           能安全讀本 repo 的最低格式版號；v3 repo 一律 ≥3
  replicas: u8,                              0 或 1；trees/snapshots 是否寫 .r1
  slot: KeySlot                              （slot 0）
}
KeySlot {
  v: 3,
  name: text,                                省略若空
  created: i64 奈秒,
  kdf: { alg: "argon2id", t, m(KiB), p, salt: bytes(16) },
  wrapped: bytes = nonce(24) ‖ AEAD密文(master ‖ Invariants CBOR) ‖ tag(16)
}
```

- 讀取端遇到 `config.v > 自己支援的版本` 或 `min_reader > 自己的版號` →
  明確錯誤「repo 需要新版 kist」，**不是**靠忽略未知欄位半讀。
- 參數範圍檢查沿用 v2（chunker、pack_target；新增 `min_reader ≤ v`、
  `replicas ≤ 1`）。
- `replicas` 預設值由 init 時的後端決定：本機＝1（單碟無冗餘）、
  S3/SFTP＝0（後端已有冗餘或頻寬成本）；可調（非 invariant）。

## 12. 切塊（FastCDC）

**逐 byte 沿用 v2 §12**：gear 表（fastcdc-go v0.2.0，digest 釘死）、
normalized level 2、min 起算、mask_s/mask_l 由 avg 推導、硬邊界 max、
緩衝無關性、參數進 config、跨語言 golden。v3 不改切塊——上游 crate 的
多變體現象恰是「凍結的東西必須擁有」的證明，而自殖碼已消除該風險類。

## 13. GC

沿用 v2 骨架：無資訊標記（內容改 `KISTGC3\n`）、時間取後端 mtime 截秒、
同秒取安全側、無 clients/ 註冊表、兩階段＋grace＋活躍 client 規則、
repack 同 v2。**變更**：

### 13.1 復活訊號：touch

- `touch/<tree hex>`：固定 8 bytes `KISTTC3\n`，以**覆寫式 Put** 寫入
  （不是 PutIfAbsent）——後端修改時間**必須刷新**，那是復活訊號本體。
  覆寫無害：內容固定 8 bytes，且 v2 的 backup 本來就對樹的**全部 bytes**
  做覆寫 put；v3 把覆寫暴露面縮到 `touch/*` 這一種固定內容物件。
  （advisor 審查抓到的初版錯誤：PutIfAbsent 的 touch 在第二次 backup
  重用同一棵樹時靜默失敗、mtime 不刷新——「D0 touch → D1 標記 →
  D5 backup#2 重用（touch 未刷新）→ prune 刪樹 → snapshot 指向已刪
  的樹」。覆寫式 Put 讓 mtime 單調前進，此時間線不可能。）
- **backup 義務**：走訪途中對每個可達 tree——本次成功新寫的（put 成功）
  不需要；**沿用的（已存在）必須 touch**（覆寫）。等價於：可達 tree 的
  「最近存活證據」永遠不小於本次 backup 的開始時間。
- 效果：樹的 bytes 永不重寫（v2 每次 backup 重 put 的 S3 頻寬消失）；
  v2 的 §6a「HEAD→DELETE 一個 RTT 視窗」縮到 touch 物件上，由 13.3 的
  （平常是空集合的）commit 檢查收尾。
- client 端可用本地狀態（「grace 內我 touch 過」）省略 touch——非規範性
  最佳化，語意不得改變（省略條件必須保守於 mtime 刷新語意）。

### 13.2 刪除條件（phase 2，依物件種類）

- **pack**：沿用 v2——標記過期＋活躍 client 都有新 snapshot＋刪前 HEAD
  （重寫過→撤銷）＋先寫 supersedes 新 index。成組刪 `parity/<hex>`。
- **tree**：標記過期＋活躍 client 規則＋ **touch 檢查**：
  `touch/<hex>` 不存在，或 touch mtime **嚴格小於**標記 mtime
  （`touch < mark` 才算死；`touch ≥ mark`＝活——同秒取安全側，與 v2
  「同秒算重寫過」同一方向），且 tree 本身 mtime ≤ 標記 → 刪
  `trees/<hex>`、`trees/<hex>.r1`、`touch/<hex>`、標記。任一不成立 →
  撤銷標記（復活）。
- **index**：沿用 v2（supersede 鏈）。
- **孤兒清理**：`touch/<hex>` 存在但 `trees/<hex>` 不存在 → 刪 touch。
  `.r1` 存在但主體不存在**且無標記** → **不刪**、`check` 回報（這是
  「主體意外遺失」的災難訊號，副本存在的目的就是它——自動清理會把保險
  單撕掉）。

### 13.3 backup 義務（嚴格 Put-only，修訂版）

- 開始時列 `gc/`：被標記的 pack 不拿來去重（chunk 重寫）、絕不刪標記。
  沿用 v2。
- 沿用的 tree 逐一 touch（13.1）。
- `BackupTooLong`：開始到寫 snapshot 前超過 grace（−1h 安全邊際）→
  不 commit、以錯誤結束。沿用 v2。
- **commit 前檢查**（收窄版）：重新載入 index，本次引用的每個 chunk 都
  解析得到、pack 存在且標記未過期（沿用 v2）。**可達 tree 中帶「開始
  時已過期標記」者**（平常是空集合）：該樹必須仍存在，且其 touch
  mtime ≥ 標記——這是對 prune 單次 HEAD→DELETE TOCTOU 視窗（prune 檢查
  touch 之後、刪樹之前，backup 剛好覆寫 touch）的唯一防線；任一不成立
  → 不 commit、以錯誤結束（重跑會沿用已上傳資料）。開始時乾淨、途中才
  被標記的 tree：標記年齡 < 備份時長 < grace（BackupTooLong 保證），
  phase 2 不會刪——v2 的逐樹 HEAD 檢查從「全部寫過的樹」縮到這個
  （通常空的）集合。

### 13.4 安全性假設

沿用 v2：grace 長於最長 backup；同 client 一次一個 backup（本機檔案鎖）；
一個 repo 建議一個 prune；versioning bucket 刪現行版、清空間靠 lifecycle；
Object Lock 刪不掉就回報保留；後端必須支援條件寫入。**新增**：後端若提
供條件刪除（If-Match），prune 應使用（關閉 v2 記錄的 HEAD→DELETE 視窗；
`object_store` 尚未暴露時，維持 touch 語意已足夠安全）。

### 13.5 副本（`.r1`）

- 寫入端：`replicas=1` 時，tree 寫完主體即寫 `.r1`（同 bytes、
  PutIfAbsent）；snapshot 先寫 `.r1` 再寫主體（**主體出現＝commit**，
  副本先行不會造成假 commit；主體失敗留下的孤兒副本由 13.2 的規則保護）。
- 讀取端：主體 Get 失敗或（hash 命名的）名稱驗證失敗 → 嘗試 `.r1`。
  副本永不改變正確性（兩份都壞才報錯），只改變可用性。
- GC：成組生命週期（13.2）；標記鍵＝主體名（`gc/<hex>`），副本隨主體。
- `check`：主體與副本都驗（都存在時）；副本缺失回報但不失敗（可修補）。

## 14. Parity（選配 sidecar）

沿用 v2 §14（明文 CBOR、`v:3`、RS 16+m 對整個 sealed pack、明文是有意
的、修復僅在重算 BLAKE3 ＝ pack 名稱時接受、prune 成組刪）。

## 15. 後端契約

沿用 v2：`PutIfAbsent`、`Get`（range read 強制）、`List`、`Stat`、
`Delete`（維護角色）。key 字法 `[a-z0-9._-]` 區段、無前導點、無冒號、
≤1024 bytes。`.r1` 與 touch 都是普通物件。**修訂**：覆寫式 `Put` 的
角色分工明確化——維護角色可對 config 覆寫（換密碼、調非不變式參數）；
**backup 角色對 `touch/*` 以外的物件一律 PutIfAbsent**，唯 `touch/*`
（固定 8 bytes 內容）用覆寫 Put 刷新 mtime（§13.1）。**選配能力**：
條件刪除（If-Match；有則 prune 應用，見 13.4）。

## 16. 版本與演進

- `config.v`、各明文 `v`、pack magic 版號全部＝**3**；讀取端遇到 ≠3：
  - 結構 `v` > 3 → 拒絕並提示「repo 需要新版 kist」（配合 `min_reader`）。
  - 結構 `v` < 3 → 拒絕（無 v2→v3 遷移；v2 尚無真實資料）。
- `min_reader` 只升不降； bump 時機＝引入舊版讀了會做錯決定的變更。
- 加欄位規則沿用 §4；金鑰推導、AAD、magic 的改動 → v4。

## 17. 寫入順序（commit point）

backup：(packs、trees 交錯；沿用的 tree 改 touch；`.r1` 隨主體) →
index blob →（`replicas=1` 時）snapshot `.r1` → **snapshot 主體**。
snapshot 主體是唯一 commit point：它出現之前的新物件都是可回收垃圾；
它出現之後，它引用的東西都已在 repo（或以 touch 標記存活）。

## 18. 規則 × 向量登記表

每條規範性規則至少掛一個跨語言 conformance 向量（兩邊 testdata 各持一
份逐 byte 相同的拷貝；由 kist-rs 的生成腳本產出）。草案階段 ID 已編、
向量待生成：

| 向量 ID | 涵蓋規則 |
| --- | --- |
| V3-KEYS-1 | KEK/四把子金鑰/wrapped(含 Invariants)/sealed master 逐 byte |
| V3-CBOR-1..4 | §4 全部十條：最短整數、禁 indefinite/float/tag、欄位表順序、xattrs 排序、重複 key/尾端 bytes 拒絕 |
| V3-CHUNK-1 | 凍結 FastCDC 邊界（沿用 v2 向量） |
| V3-TREE-1..3 | Entry per-kind 欄位存在/缺席矩陣；posix uid=0 必填；mk 不合法欄位在場→拒絕 |
| V3-ROOTS-1..3 | roots 排序/唯一；節點名單元件（含 `s3://` root）；restore 映射 |
| V3-STATS-1..3 | hard link 按名/bytes 一次；跨 roots hard link；s3 來源 dirs=0 |
| V3-PACK-1 | v3 magic 三處一致；trailer 一致性拒絕案例（沿用 v2 形狀） |
| V3-INDEX-1 | supersedes/合併純函數；>64 觸發合併 |
| V3-GC-1..5 | touch 復活時間線；過期標記＋touch 較新→不刪；孤兒 touch 清理；.r1 成組刪、無標記孤兒不刪；**第二次 backup 重用已標記樹（覆寫 touch 刷新 mtime）——advisor 抓到的初版競態之釘死案例** |
| V3-FAST-1..3 | posix ctime/inode 合約；s3 etag 合約；sftp 無快速路徑（重讀） |
| V3-CONFIG-1 | config↔Invariants 不符＝明確 tamper 錯誤；min_reader 拒絕 |

行為級驗證（非向量）：gc_race proptest 200 cases 必須在 v3 語意下重跑
全綠（touch 取代重 put 之後，競態空間重新驗證——這是 §0.4 的實作義務）。

## 19. 設計決定 × 教訓對照

| 決定 | 教訓/需求出處 |
| --- | --- |
| 保留 v2 核心（pack/AEAD/命名/CBOR/FastCDC/GC 骨架） | 它們有 PoC＋proptest 背書；重開＝歸零重驗 |
| roots 取代合成根 | 特殊案例是 bug 溫床（stats 分裂點）；遠端路徑塞不進絕對路徑模型 |
| stats 只留資料事實 | 不可變結構不收依 GC 狀態而變的數字 |
| 不變式入密文 | AAD 列舉每加欄要改排版；認證密文內的參數無條件防竄改、可擴展 |
| touch 取代重 put | 寫放大＋§6a 視窗＋逐樹 HEAD 的共同根源；8 bytes 表達「還活著」 |
| 副本而非 RS-bundle | bundle 是第二真相源（幽靈物種）；副本＝k=1 RS、零新概念、現有 hash 驗證 |
| metadata 聯集 | 遠端來源；修 uid=0/mode=0 歧義；「來源能證明什麼就記什麼」 |
| 快速路徑分級 | 變更偵港掛在可證明性，不掛在 metadata 豐富度 |
| min_reader | 混合版本是常態；半讀半猜比明確拒絕危險 |
| 壓縮觸入規格 | 已知運維債（ADR 005 未做項）變成可測規範 |
| 單一權威副本＋向量編號 | 「兩邊各一份」是漂移風險；§18 常態化 |
