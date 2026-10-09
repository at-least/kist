# kist repo 格式（v4 草稿）

> **狀態：草稿（2026-10-10），待負責人確認，見 ADR 026。** 確認並開始實作時，本文件取代
> `docs/format.md`（v3）成為唯一權威副本；在那之前程式碼仍是 v3，`format.md` 仍是權威。
> v4 不讀 v3：沒有遷移，v3 的 repo 用最後一個 v3 版本讀出或重新備份（§16）。
> 檢視差異：`diff docs/format.md docs/format-v4.md`。

v4 ＝ v3 加兩項格式改動：目錄樹搬進 metadata pack（ADR 017 決定 1），root 記錄種類（ADR 025
的 V19）。GC 補上 ADR 017 決定 2 的條款，並把 prune 的持有者集合寫成規範（ADR 019 C3）。其餘照
v3：金鑰階層的形狀、CBOR 十條規則、FastCDC、pack 排版與 trailer、snapshot 是唯一的 commit
point、pack 的兩階段 GC。

## 0. v3 → v4 變更總覽

| # | 變更 | 動機 |
| --- | --- | --- |
| 0.1 | 目錄樹段搬進 `mpacks/`：排版、trailer、index、gc 標記、parity 都與 `packs/` 相同。`trees/`、`touch/`、樹的 `.r1`、樹的 gc 標記、樹的覆寫修復、commit 前的逐樹檢查全部移除 | 資料遺失方向的 bug 有四件出在「樹的存活靠後端 mtime 表達」這一塊（ADR 017）；樹改走 pack 已有 200 案 proptest 的生命週期。每個 root 一個樹物件的寫入成本（ADR 025 的 V21）一併消失 |
| 0.2 | `Root` 加 `t`（與 `Entry.t` 同碼，必填），restore 映射不再猜 | v3 以「root tree 恰好一個與定位末段同名的非目錄」判別檔案來源，目錄恰好只含一個同名檔案時誤判（V19） |
| 0.3 | 樹段的位元組上限 `TREE_SEGMENT_MAX` ＝ 8 MiB，格式常數 | 樹段進 pack 之後要有解壓與緩衝上限；v3 只限節點數，xattrs 沒有上限 |
| 0.4 | `IndexPack` 加 `kind`（0 資料 pack、1 metadata pack） | index 要分得出持有者在哪個命名空間 |
| 0.5 | `replicas=1` 時 mpack 寫 `.r1`；資料 pack 照舊不寫 | 推翻 v3 §7「pack 不做副本」：parity 是選配、補不了整顆遺失，而樹是還原的必經之路（ADR 017 條件 2） |
| 0.6 | 派生字串與 AAD 常數改成 `kist/v4/*`；gc 標記內容改 `KISTGC4\n` | 與版號一致；v3 的 wrapped master 在 v4 下認證失敗，是版本檢查之外的第二道關（ADR 026） |
| 0.7 | GC 補條款：活躍 client 規則的嚴格方向、時鐘偏差自檢、一次一個 prune、刪前重讀標記、prune 的讀取順序、刪除順序、壞 index blob 的處理、repack 補 parity | ADR 017 決定 2 |
| 0.8 | backup 角色零覆寫：pack、mpack、index blob 都 PutIfAbsent | 讓 bucket policy 能強制 create-only（ADR 017）；v3 規格寫 PutIfAbsent，程式卻用普通 put（ADR 026） |

不改的：金鑰階層的形狀、CBOR 規則、FastCDC、pack 排版、tree 與 Entry 的明文結構（版號除外）、
snapshot 的結構（`Root.t` 除外）、parity、後端能力集。

---

## 1. 總覽

repo 是一個 key → bytes 的命名空間（本機目錄、S3 bucket、SFTP…）。除了 `config` 與 `keys/*`，
所有物件都**不可變**；寫入一律用 conditional put（`PutIfAbsent`），同名即同內容：

| key | 內容 | 加密 | 命名 |
| --- | --- | --- | --- |
| `config` | `RepoConfig`（明文 CBOR） | 否（不含祕密） | 固定 |
| `keys/<slot hex>` | `KeySlot`（明文 CBOR），選配 | 否 | 使用者取 |
| `packs/<hex>` | 資料 pack：chunk（與間接內容的 `ChunkList`） | 每項獨立 AEAD | BLAKE3(整檔 bytes)，無 key |
| `mpacks/<hex>` | metadata pack：樹段 | 每項獨立 AEAD | BLAKE3(整檔 bytes)，無 key |
| `mpacks/<hex>.r1` | 上者的副本（`replicas=1`，§13.9） | 同上 | 主體名＋`.r1` |
| `indexes/<hex>` | sealed(`IndexBlob`)（可 zstd） | 整段 AEAD | BLAKE3(整檔 bytes)，無 key |
| `snapshots/<client hex>/<ts>` | sealed(`Snapshot`) | 整段 AEAD | client id ＋時間 |
| `snapshots/<client hex>/<ts>.r1` | 上者的副本（`replicas=1`） | 同上 | 主體名＋`.r1` |
| `gc/<object hex>` | 待刪標記：固定 8 bytes `KISTGC4\n` | 否（不含資訊） | 被標記物件的名稱 |
| `parity/<pack hex>` | Reed-Solomon 同位（選配，明文 CBOR） | 否 | 對應的 pack 或 mpack |

讀取端必須驗證名稱：pack、mpack、index blob 重算無 key 的 BLAKE3；pack 內的每一項重算自己的
ID（§2）；snapshot 以 AAD 綁 key。副本 `.r1` 與主體逐 byte 相同、驗證規則相同——它不是獨立
物件，是同一物件的重複存放。

## 2. 識別碼

- `ChunkId`（32 bytes）＝ keyed BLAKE3(hash key, chunk 明文)。
- tree ID（32 bytes）＝ keyed BLAKE3(hash key, 樹段的規範 CBOR bytes)，在壓縮之前算。**不再是
  repo 的 key**：它是 mpack trailer 項目的 `i`、樹段 AEAD 的 AAD、`Entry.tree` 與 `Root.tree`
  的值。目錄沒變，tree ID 就不變（子樹整棵重用的依據，與 v3 相同）。
- pack、mpack、index blob 的名稱 ＝ BLAKE3(寫進 repo 的 bytes)，無 key。
- CBOR 內一律 32-byte byte string（major 2）；路徑裡一律小寫 hex。

## 3. 金鑰階層

```
password ─Argon2id(salt 16B, m=64 MiB, t=3, p=4)─▶ KEK (32B)
KEK ─XChaCha20-Poly1305 解開 slot 的 wrapped ─▶ master key (32B) ‖ 不變式 CBOR
master ─BLAKE3 DeriveKey─▶
    "kist/v4/hash"    → ChunkId 與 tree ID 的 keyed hash key
    "kist/v4/chunk"   → chunk 與樹段的加密
    "kist/v4/meta"    → snapshot 的加密
    "kist/v4/index"   → pack／mpack trailer 與 index blob 的加密
```

- wrapped 的明文 payload ＝ master(32B) ‖ 不變式 CBOR：
  `Invariants { v: 4, repo_id: bytes(16), chunker: { min, avg, max } }`，AAD ＝ 常數
  `"kist/v4/master"`。解鎖時從通過認證的密文取出權威參數，與明文 `config` 比對；`repo_id` 或
  `chunker` 不符 → 明確錯誤「config 已被竄改」。新增不變式欄位照 §4 的規則，不動 AAD。
- 樹段改用 chunk key 封（v3 的樹用 meta key）：樹段與 chunk 同一種封裝、放在同一種 pack 裡，
  pack 的讀寫碼原樣共用。meta key 從此只用於 snapshot。
- 非不變式（可調、不綁）：`pack_target`、`replicas`、`min_reader`。
- 沒有 nonce key：全部隨機 24-byte nonce。Argon2 參數在 `KeySlot.kdf`；換密碼＝重包 wrapped。

## 4. CBOR 慣例（規範編碼）

所有 metadata 都是 CBOR（RFC 8949）。規範編碼的完整條件（另一個實作憑本節就必須能編出逐 byte
相同的 bytes；違反任一條＝解碼拒絕或編碼非法）：

1. 整數一律最短編碼。
2. 所有長度 definite；禁止 indefinite-length（任何型別）。
3. 禁止浮點數：所有數字都是整數（時間 i64 奈秒、大小 u64、列舉 u8）。
4. 禁止 CBOR tags。
5. struct ＝ map，依 §4.1 欄位表的順序輸出；解碼端不得假設順序，按欄位名取值。
6. 二進位資料（名稱、ID、xattr、etag、version）一律 byte string；人類可讀字串（host、user、
   parent、slot name）用 text string。
7. 唯一的巢狀自由 map 是 `xattrs`，key 依 bytes 字典序排序。
8. 解碼拒絕：重複 map key、indefinite length、float、tag、尾端多餘 bytes、欄位表宣告「必須缺席」
   卻在場的欄位（§8.1）、必填欄位缺席。
9. 忽略未知欄位（向前相容），但絕不回寫：寫入端一律從事實來源重新構造，讀出的物件永不重編碼
   後寫回。
10. 新增欄位一律可省略語意（零值省略），插入位置依欄位表更新。

### 4.1 欄位表（＝輸出順序）

| 結構 | 欄位順序 |
| --- | --- |
| `Tree` | v, entries, prev |
| `Entry` | n, t, mk, size, target, ct, chunks, tree, mode, uid, gid, mtime, ctime, dev, ino, nlink, xattrs, etag, vern |
| `ChunkList` | v, chunks |
| `PackTrailer` | v, entries |
| `PackEntry` | i, o, l, r |
| `IndexBlob` | v, packs, supersedes |
| `IndexPack` | id, kind, size, entries |
| `RepoConfig` | v, repo_id, created, chunker, pack_target, min_reader, replicas, slot |
| `KeySlot` | v, name, created, kdf, wrapped |
| `KdfParams` | alg, t, m, p, salt |
| `ChunkerParams` | min, avg, max |
| `Snapshot` | v, roots, time, host, user, client, parent, stats |
| `Root` | path, t, tree |
| `SnapshotStats` | files, dirs, symlinks, bytes |

`IndexPack.kind` 零值省略（0 ＝資料 pack）。`Root.t` 與 `Entry.t` 一樣一律輸出（0 是檔案，
不是「沒有」），缺席＝解碼拒絕。

## 5. Sealed 物件（沒有 envelope header）

`nonce(24) ‖ ciphertext ‖ tag(16)`（XChaCha20-Poly1305），全部 OS 亂數 nonce。金鑰與 AAD：

| 物件 | 金鑰 | AAD |
| --- | --- | --- |
| chunk payload（`packs/` 內） | chunk | 該 chunk 的 `ChunkId` |
| 樹段 payload（`mpacks/` 內） | chunk | 該段的 tree ID |
| snapshot | meta | 完整 key 路徑的 UTF-8 bytes（如 `snapshots/<client>/<ts>`；不含 `.r1`） |
| pack／mpack trailer | index | 常數 `"kist/v4/pack-trailer"` |
| index blob | index | 常數 `"kist/v4/index"` |

snapshot 的明文＝規範 CBOR、不壓縮。

## 6. Chunk 與樹段的 payload

- chunk：payload ＝ `algorithm byte ‖ 資料`（0 原文／1 zstd level 3；壓縮省不到 1/16 就存
  原文）。解壓上限＝`chunker.max`。讀取端解密後重算 `ChunkId`。
- 樹段：payload ＝ `algorithm byte ‖ 樹段的規範 CBOR`，壓縮規則同上。解壓上限＝
  `TREE_SEGMENT_MAX`（§8）。tree ID 在壓縮前、對規範 CBOR 算。讀取端：解密 → 解壓 → 重算
  tree ID → 解析並驗證（§8、§8.1）。

## 7. Pack 與 mpack

```
+-------------------+----------------------+------------------+--------------------+-------------------+
| magic 8B          | entry × N            | trailer (sealed) | trailer len u64 BE | magic 8B          |
+-------------------+----------------------+------------------+--------------------+-------------------+
```

- magic ＝ `"kistpk"` ‖ u16 big-endian `0x0004`，頭尾各一份、必須一致；trailer 的 `v` ＝ 4
  （三處一致）。`packs/` 與 `mpacks/` 用同一個排版、同一個 magic；種類由命名空間與
  `IndexPack.kind` 表達，不另設 magic 或 AAD。
- trailer entry `{i, o, l, r}`：ID、在檔內的位移、sealed 長度、明文長度（壓縮前）。一致性檢查：
  項目連續排列、完整覆蓋 magic 與 trailer 之間、沒有重複 ID、長度不超過上限——`r` 對資料 pack
  ≤ `chunker.max`，對 mpack ≤ `TREE_SEGMENT_MAX`。
- 資料 pack 的項目是 chunk（含間接內容的 `ChunkList`，§8）；mpack 的項目只有樹段。
- 名稱＝BLAKE3(整檔)；寫滿 `pack_target`（預設 64 MiB）或 backup 結束時 flush，兩種 pack
  各自累積。
- 副本：資料 pack 不寫 `.r1`（parity 涵蓋）；**mpack 在 `replicas=1` 時寫 `.r1`**（§13.9）。
  這推翻 v3 §7：parity 是選配、修不了整顆遺失，而沒有樹就什麼都還原不了。

## 8. Tree 與 Entry（metadata 聯集）

```
Tree  { v:4, entries:[Entry...], prev: tree ID | null }
Entry {
  n:    檔名 byte string——一律單一路徑元件（讀取端驗證：非空、不含 `/` 與 NUL、不是 `.` 或 `..`）
  t:    類型 u8：0 檔案, 1 目錄, 2 符號連結
  mk:   metadata 種類 u8：0 posix, 1 sftp, 2 s3, 3 generic
  size: u64（僅檔案）                         省略若 0
  target: bytes（僅符號連結）                  省略若空
  ct:   u8：省略/0 直接；1 間接                省略若 0
  chunks: [ChunkId]（≤256，直接）              省略若空
  tree: 子目錄 tree ID（分段＝最後一段）        省略若 null
  ── 以下為 per-kind 欄位，意義由 mk 決定 ──
  mode, uid, gid, mtime, ctime, dev, ino, nlink, xattrs   （mk=0）
  mode, uid, gid, mtime                                   （mk=1）
  etag, vern, mtime                                       （mk=2）
  mtime                                                   （mk=3）
}
ChunkList { v:4, chunks:[ChunkId] }
```

- 一個目錄的 entries 依 `n` bytes 升冪、名稱不重複，切成一段或多段：每段至多
  `MAX_NODES_PER_TREE` ＝ 10 000 個節點，**且**規範 CBOR 至多 `TREE_SEGMENT_MAX` ＝ 8 MiB
  （8 388 608 bytes）。寫入端在加入下一個節點會超過任一個上限時開新段；段以 `prev` 串接，父節點
  記最後一段。讀取端拒絕超過任一上限的段。
- 單一節點本身就讓一段超過 `TREE_SEGMENT_MAX`（要有超過 8 MiB 的 xattrs 或名稱）時寫不進去：
  backup 把它記成該節點的錯誤、不寫入，結束碼反映錯誤。
- 上限的算術（ADR 026 實測）：真實目錄平均每個節點約 128 bytes，10 000 個約 1.3 MB，所以一般
  目錄由節點數上限決定。位元組上限只在直接 chunk 清單很長（256 × 34 bytes ≈ 8.7 KB，約 950 個
  節點一段）或 xattrs 很大時起作用。
- 間接內容：chunks 超過 256 個時改存 `ChunkList`，當資料放進資料 pack（與 v3 相同）。
- 硬連結：mk=0 的 dev／ino／nlink，`nlink > 1` 才記。

### 8.1 per-kind 欄位規則（讀取端強制）

| mk | 必填 | 選填 | 必須缺席 |
| --- | --- | --- | --- |
| 0 posix | mode, uid, gid, mtime | ctime, dev, ino, nlink, xattrs, etag, vern | — |
| 1 sftp | mtime | mode, uid, gid | ctime, dev, ino, nlink, xattrs, etag, vern |
| 2 s3 | — | mtime, etag, vern | mode, uid, gid, ctime, dev, ino, nlink, xattrs |
| 3 generic | — | mtime | 其餘全部 |

- posix 的 uid／gid 必填：uid 0 是 root、是真實值。s3 欄位全選填：缺席＝來源沒有，不是 0。
- `mtime` 一律 i64 奈秒；s3／sftp 來源只有秒精度（奈秒補 0），快速路徑以來源精度比較。
- `etag`：來源提供的內容指紋，原樣保存；`vern`：來源物件的版本 ID。各自上限 1 KiB，讀寫兩端都
  拒絕超限。

### 8.2 快速路徑合約

| 來源 | 沿用條件 | 依據 |
| --- | --- | --- |
| mk=0 posix | size、mtime、ctime、inode 都沒變（dev 只在 `nlink>1` 時記錄，不參與比較） | kernel 維護，使用者改不了 |
| mk=2 s3 | etag 相同且 size 相同 | etag 是來源計算並保證的內容指紋 |
| mk=1 sftp／mk=3 | 沒有安全的快速路徑，一律重讀，靠 chunk 去重吸收 | mtime 與 size 都能被 client 設定 |

只看 mtime 沿用一律非法。

### 8.3 硬連結範圍

`(dev, ino)` 的同一性判定範圍＝整個 snapshot（跨 roots）：`bytes` 只算一次、restore 重建為一個
hard link（第一個出現的位置建立，其餘 link；失敗降級為複本並警告）。

### 8.4 巢狀深度（實作上限）

tree 的 DIR 巢狀沒有格式上的深度限制；參考實作的上限是 256 層（`kist_core::MAX_TREE_DEPTH`）。
backup 的寫入端套同一把尺（超過的子目錄跳過並記警告），還原、check、prune 把超過上限的 chain
當腐壞或敵意 repo 乾淨回錯。同一目錄的 `prev` 分段用迴圈走，不計深度。

## 9. Snapshot

key：`snapshots/<client hex>/<ts>`，`ts` 為 `YYYYMMDDTHHMMSSnnnnnnnnnZ`；conditional put，
同名時 +1ns 重試。

```
Snapshot {
  v: 4,
  roots: [Root...]        // ≥1；依 path bytes 升冪、不重複（讀取端驗證）
  time: i64 奈秒（= backup 開始時刻，與 key 同瞬間）,
  host: text,
  user: text,                              省略若空
  client: bytes(16)（與 key 的 client hex 一致）,
  parent: text | null（上一個 snapshot 的 key；只用於加速）,
  stats: { files, dirs, symlinks, bytes }  // 全部可省略
}
Root {
  path: byte string,   // 不透明來源定位：本機絕對路徑 / sftp://host[:port]/path / s3://bucket/prefix
  t:    u8,            // 來源本身的類型，與 Entry.t 同碼：0 檔案, 1 目錄, 2 符號連結（必填）
  tree: tree ID        // t=1：目錄的內容；t=0 或 2：恰好一個節點，就是來源本身
}
```

- 定位切段（任何實作都必須一致，`mount` 也照這條）：有 `scheme://` 就去掉；以 `/` 切；空段與
  `.` 丟掉；`..` 映射成 `__parent__`（restore 不往目標之外走）；段裡有 NUL＝腐壞。例：
  `/srv/data` → `srv`、`data`；`s3://bucket/prefix` → `bucket`、`prefix`；`s3://bucket/` →
  `bucket`；`/` → 沒有段。
- restore 映射：
  - `t=1`（目錄）：tree 的節點放在 `<target>/<所有段>` 底下。沒有段時直接放在 `<target>`。
  - `t=0` 或 `2`（檔案、符號連結）：tree 必須恰好一個節點、類型等於 `t`、名稱等於最後一段，放在
    `<target>/<去掉最後一段>/<名稱>`。
- 讀取端驗證：`t` 在 {0, 1, 2} 之內；`t≠1` 時定位至少一段，且 tree 恰好一個符合上一條的節點。
  `t=1` 對節點沒有限制：只含一個與最後一段同名的檔案的目錄就是目錄（v3 在這裡會猜錯，V19）。
- 沒有合成根：root 本身不是任何 tree 的節點。
- parent 快速路徑資格：parent 存在，且 roots 清單（`path` 與 `t` 的序列）相同。

### 9.1 stats 口徑（只留資料事實）

- `files`／`symlinks`：按名稱計（hard link 的每個名字各算一個）。
- `dirs`：目錄節點數；roots 不算（root 是定位，不是節點）。s3 來源沒有目錄節點 → `dirs=0`。
- `bytes`：檔案內容總和，`(dev, ino)` 群組只算一次（跨 roots）；沒有硬連結資訊的節點按名稱各算。
- 過程計數（新 chunk 數、新 pack 數、重寫數、錯誤數…）不存在於格式，屬於 backup 的報告輸出。

## 10. Index blob

```
IndexBlob { v:4, packs:[IndexPack], supersedes:[index blob 名稱] }
IndexPack { id: pack 名稱, kind: u8, size: u64, entries:[PackEntry] }
```

- `kind`：0 ＝ `packs/<id>`，1 ＝ `mpacks/<id>`；其他值拒絕。index 以外的地方不記錄種類：讀取
  端依 `kind` 決定 key 前綴。
- 持有者：同一個 ID 可以出現在多個 pack（重寫、repack 搬家）。查詢 chunk 只看 `kind=0` 的持有者，
  查詢樹段只看 `kind=1` 的持有者；只有另一種持有者＝查不到。讀取端保留一個 ID 的全部持有者，依
  正本順序（沒被標記的優先，其次名稱最小）逐一嘗試。
- 合併：多個有效 blob 的聯集以上述正本順序選出每個 ID 的正本，是純函數；被 `supersedes` 列到
  的 blob 忽略。blob 明文可 zstd（省不到 1/16 就存原文）。
- 壓縮觸發（規範性）：有效（未被 supersede）blob 超過 64 顆時，prune 必須合併重寫成一顆
  （supersedes 全部）。

## 11. Config 與 KeySlot

```
RepoConfig {
  v: 4,
  repo_id: bytes(16),                        // 必須 == Invariants.repo_id
  created: i64 奈秒,
  chunker: { min, avg, max },                // 必須 == Invariants.chunker
  pack_target: u64,                          預設 64 MiB（非不變式）
  min_reader: u16,                           能安全讀本 repo 的最低格式版號；v4 repo 一律 ≥4
  replicas: u8,                              0 或 1；snapshot 與 mpack 是否寫 .r1
  slot: KeySlot                              （slot 0）
}
KeySlot {
  v: 4,
  name: text,                                省略若空
  created: i64 奈秒,
  kdf: { alg: "argon2id", t, m(KiB), p, salt: bytes(16) },
  wrapped: bytes = nonce(24) ‖ AEAD 密文(master ‖ Invariants CBOR) ‖ tag(16)
}
```

- 版本檢查在解開金鑰之前：`config.v` 不是 4、或 `min_reader` 大於自己的版號 → 明確錯誤（§16）。
- 參數範圍與 v3 相同：chunker 的 min 在 64 B 到 1 MiB、avg 在 256 B 到 16 MiB、max 在 1 KiB 到
  64 MiB 之間，且 min ≤ avg ≤ max；`pack_target ≥ chunker.max`；`min_reader ≤ v`；`replicas ≤ 1`。
- `replicas` 預設由 init 時的後端決定：本機＝1（單碟沒有冗餘），S3／SFTP＝0；可調。

## 12. 切塊（FastCDC）

與 v3 逐 byte 相同：gear 表（取自 fastcdc-go v0.2.0，以 digest 釘死）、normalized level 2、
從 min 起算、mask_s／mask_l 由 avg 推導、max 硬邊界、與緩衝方式無關、參數存在 config、凍結的
邊界向量。

## 13. GC

受 GC 管理的物件：`packs/`、`mpacks/`（連同它們的 `.r1` 與 parity）、`indexes/`。snapshot 由
`forget` 刪，不經標記。標記 `gc/<hex>` 是固定 8 bytes `KISTGC4\n`，本身不帶資訊；時間一律取
後端的修改時間、截到秒，同秒取安全側。

### 13.1 需要的集合（prune，規範性）

1. 順序：列 `indexes/` → 讀完全部 index blob → 再列 `packs/`、`mpacks/`、`gc/`。先列 pack 再讀
   index 的話，兩步之間剛上傳、剛索引的 pack 會被當成幽靈丟掉。
2. 幽靈：index 裡有、列表裡沒有的持有者。不參加正本選擇；需要的 ID 只有幽靈持有＝引用不完整。
3. 需要的樹：從任一 snapshot 的 root 走得到（經 `kind=1` 的持有者）。需要的 chunk：需要的樹引用
   的 chunk，連同 `ChunkList` 與它列的 chunk。
4. 一個 pack 或 mpack 是活的，若且唯若它是某個需要的 ID 的正本（§10 的順序，只在存在的持有者
   之間選）。index blob 是活的，若且唯若沒有被 supersede。
5. 年輕物件：後端修改時間在 grace 之內的物件不標記（可能屬於進行中的 backup）。commit 檢查裡
   「自己寫的 pack 免驗」（§13.5）以這條為依據。
6. 有任何 snapshot、樹段、index blob 讀不出來，或需要的 chunk、樹段不在 index 裡 → 整輪拒絕，
   不標也不刪。

### 13.2 第一階段：標記與復活

- 不需要、而且比 grace 老的物件：以 PutIfAbsent 寫 `gc/<hex>`。
- 被標記、但又變成需要的（backup 開始得比標記早、之後才 commit）：刪掉標記（復活）。
- 標記指到的物件不存在：刪掉標記。

### 13.3 第二階段：刪除條件

以下全部成立才刪：

1. 標記比 grace 老。
2. 活躍 client 規則：每個在 `inactive_after` 內有 snapshot 的 client，最新 snapshot 的時間減去
   `clock_skew` 之後都**嚴格晚於**標記時間；相等＝擋住。
3. 時鐘偏差自檢：每一輪在刪除任何東西之前，HEAD 本輪寫出的第一個物件（標記，或第 4 條的 index
   blob），後端時間與 prune 主機時間相差超過 `clock_skew` → 整輪中止，不刪任何東西。prune 主機
   時鐘超前會讓每個標記看起來更老，方向是資料遺失。只刪不標的一輪也會寫第 4 條的 blob，所以每一輪
   要刪東西之前都檢查得到。
4. 要刪的 pack 或 mpack 先從 index 移除：寫一顆不含它、supersedes 全部舊 blob 的新 index blob，
   讀取端永遠不會從 index 指到不存在的 pack。
5. 刪之前重讀它的標記：標記不在了，代表另一個 prune 已復活它 → 跳過。再 HEAD 物件本身：標記之後
   被重寫過 → 復活。
6. 刪除順序：`.r1` → parity → 主體 → 標記。中途當機只會留下有標記、可以重來的半成品，不會留下
   看起來像「主體遺失」的孤兒副本。

### 13.4 repack

需要的、比 grace 老、正本比例低於門檻的 pack 或 mpack：把它是正本的項目解密驗證後重新封裝，
寫進新的同種 pack（repo 用 parity 時連同 parity；mpack 在 `replicas=1` 時連同 `.r1`）。舊的留在
index 裡並打上標記，照 13.3 走。v3 的 repack 不寫 parity sidecar，這是 v3 既有的缺口，v4 補上。

### 13.5 backup 的義務

- 開始時列 `gc/`：被標記的 pack 與 mpack 不拿來去重、不拿來沿用樹；絕不刪標記。
- 沿用樹的規則：index 查得到、而且持有者（`kind=1`）存在且沒被標記的樹段才沿用；否則把它重新
  寫進新的 mpack，跟 chunk 一樣。這一條取代 v3 的整個 touch 機制。
- 沿用的驗證：本輪沒有經 parent 走訪讀過的沿用樹段，讀出來驗（解密、重算 tree ID、解析）；驗不
  過就當作查不到，重新寫。
- 只用 PutIfAbsent（§15）。
- BackupTooLong：從開始到要寫 snapshot 之前超過 grace（減 1 小時安全邊際）→ 不 commit、以錯誤
  結束。
- commit 前檢查：以正本順序重新載入 index，本次引用的每個 chunk 與樹段都解析得到、持有者存在、
  標記（若有）未過期；本次自己寫的 pack 與 mpack 免驗（13.1 第 5 條）。

### 13.6 讀取端

- 不過濾被標記的持有者（只有去重才排除）：已 commit 的 snapshot 可以合法地只剩被標記的持有者。
- 讀一項遇到 NotFound 或 Corrupt：重新載入 index 再試（樹段與 chunk 都會因 repack 搬家），仍
  失敗才報錯。

### 13.7 安全性假設

- grace 長於最長的一次 backup。
- 同一個 client id 一次只跑一個 backup（本機檔案鎖）。
- 一個 repo 一次只跑一個 prune：同一台主機以檔案鎖保證；不同主機的 prune 沒有鎖，靠 13.3 第 5
  條縮小視窗。
- versioning bucket 刪現行版、清空間靠 lifecycle；Object Lock 刪不掉就回報保留。
- 後端必須支援條件寫入。後端提供條件刪除（If-Match）時 prune 應該使用。

### 13.8 壞掉的 index blob

- backup 與 restore：解不開或解析失敗的 blob 跳過並警告（少了它只會多傳資料，或讓找不到的項目
  報錯）。
- prune：拒絕（GC 需要完整的持有者集合）。
- 維護命令：列出並刪除解不開的 blob；`rebuild-index` 從 `packs/` 與 `mpacks/` 的 trailer 重建。

### 13.9 副本（`.r1`）

- 寫入端（`replicas=1`）：mpack 寫完主體再寫 `.r1`（同 bytes、PutIfAbsent）；snapshot 先寫
  `.r1` 再寫主體（主體出現＝commit）。
- 讀取端：主體讀不到，或名稱與內容驗證失敗 → 讀 `.r1`。副本只改變可用性，不改變正確性。
- GC：副本隨主體刪（13.3 第 6 條的順序）。`.r1` 在、主體不在、也沒有標記 → **不刪**，`check`
  回報：這是主體意外遺失的訊號，副本存在就是為了這個。
- `check`：主體與副本都驗（都存在時）；副本缺失回報但不算失敗。

## 14. Parity（選配 sidecar）

與 v3 相同，同時適用於 pack 與 mpack：明文 CBOR、`v:4`、Reed-Solomon 16 個資料 shard 加 m 個同位
（m 在 0..=8）對整個 sealed pack；修復只在重算 BLAKE3 等於 pack 名稱時接受；隨 pack 刪除
（13.3）；repack 為新 pack 寫 parity（13.4）。

## 15. 後端契約

- 能力：`PutIfAbsent`、`Get`（必須支援 range read）、`List`、`Stat`（HEAD）、`Delete`（維護
  角色）。key 字法：`[a-z0-9._-]` 區段、無前導點、無冒號、≤1024 bytes。
- 覆寫的分工：**backup 角色不覆寫任何物件**，pack、mpack、parity、index blob、snapshot 一律
  PutIfAbsent。維護角色可以覆寫 `config`（換密碼、調非不變式參數），`check --repair` 可以把修好的
  bytes 同名重寫。
- 抗勒索（S3）：bucket policy 以 `s3:if-none-match` 強制 backup 主體的 PutObject 帶
  `If-None-Match`，建立權限只給 `packs/`、`mpacks/`、`parity/`、`indexes/`、`snapshots/`，對
  `config`、`keys/`、`gc/` 明確拒絕。v3 要替 `touch/` 與樹的修復開例外，v4 不需要任何例外。
- 選配能力：條件刪除（If-Match），有則 prune 應使用。

## 16. 版本與演進

- `config.v`、各明文 `v`、pack magic 的版號全部＝**4**。讀取端：
  - `v > 4` → 拒絕，提示「repo 需要新版 kist」（配合 `min_reader`）。
  - `v < 4` → 拒絕，提示 v3 repo 要用最後一個 v3 版本讀出或重新備份。沒有 v3 → v4 遷移：v3
    沒有真實資料（負責人，2026-10-10）。
- 版本檢查在解開金鑰之前；就算略過了它，v3 的 wrapped master 在 `"kist/v4/master"` 下也認證不過。
- `min_reader` 只升不降；bump 的時機＝引入舊版讀了會做錯決定的變更。
- 新增欄位照 §4；金鑰推導、AAD、magic 的改動 → v5。

## 17. 寫入順序（commit point）

backup：packs 與 mpacks 交錯寫（mpack 的 `.r1` 緊跟主體）→ index blob →（`replicas=1` 時）
snapshot `.r1` → **snapshot 主體**。snapshot 主體是唯一的 commit point：它出現之前的新物件都是
可回收的垃圾；它出現之後，它引用的東西都已在 repo 裡。

## 18. 規則 × 向量登記表

每條規範性規則至少掛一個 conformance 向量，由產品端錄製、提交成資料檔。現況（2026-10-10，v3）：
實際存在的資料檔是 `kist-format/tests/golden/` 的七個 golden（tree、chunk_list、pack_layout、
pack_trailer、index、config、snapshot）、`kist-format/tests/testdata/` 的 tree-canonical.hex 與
parity golden、`kist-chunker/tests/testdata/` 的切塊邊界、`kist-crypto/tests/poc_keys.rs` 裡的
金鑰向量，以及凍結的 v3 fixture repo；v3 登記表的其他 ID 大多只是編號。v4 實作時，下表每一列都
要有資料檔或測試：

| 向量 ID | 涵蓋規則 |
| --- | --- |
| V4-KEYS-1 | KEK、四把子金鑰（`kist/v4/*`）、wrapped（含 Invariants）逐 byte |
| V4-CBOR-1..4 | §4 十條；`Root.t` 缺席拒絕 |
| V4-CHUNK-1 | FastCDC 邊界（沿用 v3 的資料檔） |
| V4-TREE-1..4 | per-kind 存在矩陣；posix uid=0；不合法欄位拒絕；段的節點數與位元組上限（寫入端切段、讀取端拒絕） |
| V4-ROOTS-1..4 | roots 排序與唯一；定位切段；`t=1` 只含一個同名檔案仍是目錄；`t≠1` 的驗證 |
| V4-STATS-1..3 | hard link 按名、bytes 一次；跨 roots；s3 來源 dirs=0 |
| V4-PACK-1..2 | magic 三處一致；trailer 一致性拒絕；mpack 的 `r` 上限 |
| V4-INDEX-1..3 | supersedes 與合併純函數；`kind` 與命名空間；>64 觸發合併 |
| V4-GC-1..6 | 被標記持有者的樹段重寫進新 mpack；活躍 client 規則同秒擋住；時鐘偏差自檢中止；刪前標記消失則跳過；刪除順序；無標記的孤兒 `.r1` 不刪 |
| V4-FAST-1..3 | posix 合約；s3 etag 合約；sftp 一律重讀 |
| V4-CONFIG-1..2 | config 與 Invariants 不符＝竄改錯誤；`v=3` 的 repo 拒絕（v3 fixture 當反向向量） |

行為級驗證（非向量）：gc_race proptest 把樹納入持有者模型後重跑 200 案，並補兩個缺口：第二個
pending plan 不再被略過、加入「execute 中途中斷」的操作。

**凍結 fixture repo**：v3 的那份（`crates/kist-core/tests/fixtures/v3/repo`）改當反向向量（v4
讀取端必須以版本錯誤拒絕），另凍結一份 v4 的；它由產品寫出一次、提交進 git、永不重生，每次測試
都以目前的程式開啟、`check --read-data`、還原比對、解碼後重新編碼逐 byte 比對、同內容再備份零新
chunk。

## 19. 設計決定 × 教訓對照

| 決定 | 教訓或需求 |
| --- | --- |
| 樹進 mpack，刪 touch | 四件資料遺失方向的 bug 住在「樹的存活靠後端 mtime」；樹改走 pack 已驗證的生命週期 |
| `Root.t` | v3 以內容猜來源類型，猜錯一種（V19）；記下事實就不用猜 |
| 樹段位元組上限 | 進 pack 的東西都要有解壓與緩衝上限 |
| mpack 寫 `.r1` | parity 選配、修不了整顆遺失；沒有樹就還原不了 |
| 派生字串跟版號 | 版本檢查之外，密碼學上也分開兩個版本 |
| backup 零覆寫 | 讓 create-only 的 bucket policy 不必開例外 |
| 先讀 index 再列 pack、刪前重讀標記、時鐘自檢 | 重疊 prune 與時鐘的競態都是資料遺失方向 |
| 保留 v3 的其餘部分 | 有向量、fuzz、proptest 背書；重做等於歸零重驗 |
