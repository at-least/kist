# kist repo 格式（v1）

這份文件與 `crates/kist-format` 一起維護：程式碼改了、這裡也要改，反之亦然。
每種結構在 `crates/kist-format/tests/golden/` 都有一份固定的範例 bytes；
任何會改變 on-disk bytes 的修改都會讓 golden 測試失敗，這是刻意的。

> 狀態：**v1 已於 2026-09-05（M1 審查修正後）凍結**。之後只能透過 `version` 欄位演進；
> 任何改動都需要專案負責人確認，並同步更新 golden files 與這份文件。
> 注意：2026-09-05 把 `repo_id` 與 chunker 參數綁進 master key 的 AAD（§3），這是
> **不相容的改動**——之前建立的 repo 打不開。當時沒有任何真實 repo，所以直接改；
> 這種「bytes 沒變、意義變了」的改動 golden files 抓不到，靠這裡的紀錄。

## 1. 總覽

repo 是一個 key → bytes 的命名空間（本機目錄、S3 bucket…）。所有物件除了
`config` 與 `keys/*` 之外都是**不可變**且**以內容命名**的：

| key | 內容 | 加密 | 命名 |
| --- | --- | --- | --- |
| `config` | `RepoConfig`（明文 CBOR） | 否（不含祕密） | 固定 |
| `keys/<slot id>` | `KeySlot`（明文 CBOR） | 否（不含祕密） | 使用者取 |
| `packs/<hex>` | pack 檔：多個加密 chunk + 加密 trailer | 每 chunk 獨立 AEAD | BLAKE3(整檔 bytes) |
| `indexes/<hex>` | envelope(`IndexBlob`) | 整段 AEAD | BLAKE3(整檔 bytes) |
| `trees/<hex>` | envelope(`Tree`) | 整段 AEAD | BLAKE3(整檔 bytes) |
| `snapshots/<client hex>/<ts>` | envelope(`Snapshot`) | 整段 AEAD | client id + 時間 |
| `gc/<object hex>` | 待刪標記（M3 定義；pack、tree、index 都適用） | — | 被標記物件的名稱 |

「以 BLAKE3(整檔 bytes) 命名」指的是對**寫進 repo 的密文 bytes** 做一般（無 key）
BLAKE3，小寫 hex。任何人下載後都能在不持有金鑰的情況下驗證檔案沒被改過；
而因為 hash 的是密文，名稱不會洩漏明文的任何資訊。

**讀取端必須驗證名稱**：envelope 的 AAD 只綁物件種類，沒綁名稱。把 tree A 的 bytes 複製到
`trees/<B>` 上，解密會成功。所以讀 `trees/*`、`indexes/*` 時一律先算 hash 對名稱；
讀 `snapshots/*` 時用內容裡的 `client_id` 與 `time` 反算 key，必須與實際的 key 相同。

## 2. 識別碼

- `ChunkId`（32 bytes）＝ keyed BLAKE3(hash key, chunk 明文)。它只出現在加密過的
  trailer / index / tree 內容裡，**永遠不會**成為 repo 的 key。
- `ObjectId`（32 bytes）＝ BLAKE3(物件密文 bytes)，即上表的 `<hex>`。

CBOR 內兩者都是 32-byte 的 byte string（major type 2），不是整數陣列。

## 3. 金鑰階層

```
password ─Argon2id(salt, m/t/p 來自 config)─▶ KEK (32B)
KEK ─XChaCha20-Poly1305 解開 wrapped_master_key（AAD 見下）─▶ master key (32B)
master key ─blake3::derive_key(context)─▶
    "kist v1 hash key"    → chunk ID 用的 keyed hash key
    "kist v1 chunk key"   → chunk 加密
    "kist v1 object key"  → envelope 加密（tree / index / snapshot / pack trailer）
    "kist v1 nonce key"   → content-addressed 物件的決定性 nonce（§5.2）
```

wrapped_master_key 的 AAD = `"kist v1 master key\0"` ‖ `repo_id`（16 bytes）‖
`chunker.min` ‖ `chunker.avg` ‖ `chunker.max`（各 u32 little-endian）。
`config` 是明文、沒有 MAC；把這些欄位綁進 AAD 之後，有人改了它們就會解不開 master key，
而不是悄悄讓去重失效。只綁本來就不可變的欄位；`pack_target_size` 可調，不綁。

Argon2id 參數存在 config 裡，開 repo 時**一律讀 config**，不寫死在程式裡；
日後要調高只需改 `init` 的預設值（目前 64 MiB / t=3 / p=1），舊 repo 不受影響。
但明文參數不可信：讀取端對 m_cost（≤ 1 GiB）、t_cost、p_cost（≤ 64）、salt 長度（16）
與 chunker / pack 參數（§7）都有範圍檢查，超出就拒絕，不會真的去配置記憶體。

本機快取（M2）識別 repo 用的 ID 從 master key 派生（`derive_key("kist v1 cache id")`），
不用明文的 `repo_id`。

## 4. CBOR 慣例

- 所有 metadata 用 CBOR（RFC 8949），struct 編成 map，key 是欄位名字串，
  **順序 = 程式碼裡的欄位順序**。
- enum 用 serde 的 externally-tagged 形式：`{"File": {...}}`、`{"Dir": {...}}`。
- 解碼時忽略未知欄位；新增欄位一律加 `#[serde(default)]`，這樣舊版程式讀得了新資料、
  新版程式讀得了舊資料。
- 每個頂層結構都有 `version` 欄位；v1 全部是 `1`。
- 禁止 `HashMap`：同一份資料必須永遠編成同一串 bytes（tree 重用依賴這點）。

## 5. Envelope（獨立物件的外層）

tree / index / snapshot / pack trailer 都長這樣：

```
offset  size  欄位
0       4     magic "KIST"
4       1     envelope 版本 = 1
5       1     物件種類：1 Tree, 2 Index, 3 Snapshot, 4 KeySlot, 5 PackTrailer
6       1     壓縮：0 無, 1 zstd
7       1     保留 = 0
8       24    XChaCha20-Poly1305 nonce
32      …     密文 = AEAD(object key, nonce, AAD = 前 32 bytes, 明文)
```

明文 = CBOR，壓縮欄位為 1 時先 zstd 再加密。密文尾端 16 bytes 是 Poly1305 tag。

### 5.1 為什麼 AAD 是整個 header

物件種類綁進 AEAD 驗證：伺服器端若把某個 index 物件的 bytes 放到 tree 的位置，
解密時 AAD 不符會直接失敗，而不是解出一個「看起來像 tree」的東西。

### 5.2 決定性 nonce（只用於 tree）

tree 要能「目錄沒變就整棵重用」，前提是同樣的明文要編出同樣的密文（名稱才會一樣）。
所以 tree 的 nonce 不是隨機，而是 `keyed BLAKE3(nonce key, 被加密的 body)` 的前 24 bytes，
其中 body 是壓縮後（若有壓縮）真正送進 AEAD 的 bytes。**必須用 body 而不是壓縮前的明文**：
nonce 重用的定義是「同一 key、同一 nonce、不同的輸入 bytes」，而 zstd 換版本時同一份明文
可能壓出不同的 body。從 body 推導後，body 不同 → nonce 不同，永遠不會重用。
兩份不同 body 撞到同一 nonce 的機率等同 BLAKE3 碰撞，可忽略；同一 body 得到同一 nonce
則本來就是我們要的（密文完全相同，沒有多洩漏任何資訊）。nonce key 是祕密，
外人無法從 nonce 推出任何關於明文的事。

副作用：zstd 升級後同一個目錄可能壓出不同 body，tree 名稱就會變，第一次 backup 會把
這些 tree 重新上傳一次；舊的仍然有效，只是多佔一點空間，沒有任何東西會壞。
nonce 存在 header 裡，讀取端從不重算，所以這條規則只約束寫入端，不影響 on-disk 相容性。

其他物件（index / snapshot / pack trailer / chunk）一律用 OS 亂數產生 24-byte nonce。

## 6. Pack

```
+----------+----------------------+------------------+------------------+----------+
| magic 8B | chunk entry × N      | trailer envelope | trailer len u64  | magic 8B |
+----------+----------------------+------------------+------------------+----------+
```

- magic = `"KISTPAK1"`，檔頭檔尾各一份。
- chunk entry = 24-byte nonce ‖ AEAD(chunk key, nonce, AAD = ChunkId 的 32 bytes, payload)。
  payload 是 chunk 明文，或 zstd 壓縮後的明文（由 trailer 的 `flags` 說明）。
  entry 之間沒有分隔符，位置只記在 trailer。
- trailer envelope：§5 的格式，種類 = 5，內容 `PackTrailer`：

  ```
  PackTrailer { version: u32, entries: [PackEntry] }
  PackEntry   { id: ChunkId, offset: u64, length: u64, raw_len: u64, flags: u8 }
  ```
  `offset`/`length` 是 entry 在檔案裡的範圍（含 nonce 與 tag），`raw_len` 是明文長度，
  `flags` bit0 = zstd。
- trailer len 是 little-endian u64。讀取時先 range read 檔尾 16 bytes 取得長度，
  再 range read trailer 本體，不用下載整個 pack。
- pack 目標大小 64 MiB（`config.pack_target_size`），寫滿或 backup 結束時 flush。
- 壓縮規則：每個 chunk 先 zstd level 3；若壓後大小 > 原大小 × 97%，改存原文（flags = 0）。

## 7. Config 與 KeySlot

```
RepoConfig {
  version: u32,
  repo_id: bytes(16),          隨機，用來區分 repo
  created: text,               RFC 3339 UTC
  chunker: { min: u32, avg: u32, max: u32 },   預設 512 KiB / 2 MiB / 8 MiB
  pack_target_size: u64,
  key: KeySlot,                slot 0
}
KeySlot {
  version: u32,
  name: text,
  created: text,
  kdf: { algorithm: text = "argon2id", m_cost_kib: u32, t_cost: u32, p_cost: u32, salt: bytes(16) },
  wrapped_master_key: { nonce: bytes(24), ciphertext: bytes(48) },
}
```

`config` 是唯一允許覆寫的物件（換密碼、調參數）。額外的 key slot 寫在 `keys/<id>`，
只新增不修改。兩者都是明文 CBOR，裡面沒有祕密：master key 已被 KEK 包住，
salt 與 KDF 參數本來就是公開的。

參數範圍（讀取端強制）：`chunker.min` 64..=1 MiB、`avg` 256..=16 MiB、`max` 1 KiB..=64 MiB、
min ≤ avg ≤ max；`pack_target_size` 64 KiB..=4 GiB 且 ≥ `chunker.max`；`repo_id` 16 bytes。

backup 帳號需要的最小權限是 `PutObject` + `GetObject` + `ListBucket`，**不需要 `DeleteObject`**
（開 repo 要讀 `config` 與 index，所以字面上的「只有 Put」做不到；抗勒索的重點是拿不到刪除權）。

因為 `config` 可覆寫，有 Put 權限的人可以把它蓋成垃圾讓所有人打不開 repo
（資料本身仍在，只是需要備份的 config）。M2 的抗勒索設計要對 `config` 開 S3 versioning
或 Object Lock，並建議使用者把 `config` 另存一份。

## 8. Tree

```
Tree { version: u32, nodes: [Node], prev: ObjectId | null }
Node { name: bytes, meta: NodeMeta, kind: NodeKind }
NodeMeta { mode: u32, uid: u32, gid: u32, mtime_secs: i64, mtime_nanos: u32,
           ctime_secs: i64, ctime_nanos: u32, inode: u64 }
NodeKind =
  | { "File":    { size: u64, content: Content } }
  | { "Dir":     { subtree: ObjectId } }
  | { "Symlink": { target: bytes } }
Content =
  | { "Direct":   { chunks: [ChunkId] } }
  | { "Indirect": { chunks: [ChunkId] } }     chunks 串起來的明文是 CBOR ChunkList
ChunkList { version: u32, chunks: [ChunkId] }
```

- 寫入端**每次 backup 都重新 put 每個 tree**（同名同 bytes，冪等）：壞掉的 tree 會被
  下一次 backup 修回來，而且不依賴「backup 開始時的物件列表」（GC 可能中途刪掉東西）。
- `nodes` 依 `name` 的 bytes 升冪排序；同一目錄內名稱不重複。
- `name` 在 Unix 是原始 OS bytes；Windows 上是檔名的 UTF-8。
- `mode` 含檔案類型位元（例如一般檔 `0o100644`）。Windows 上 mode/uid/gid 為 0。
- `ctime_*` 與 `inode` 只用於 backup 的快速路徑（size + mtime + ctime + inode 都沒變才沿用
  上次的 chunk 清單），不會被還原。Windows 上為 0；讀到 0 就不拿來比對。
  這三個欄位是 `#[serde(default)]`，缺少時視為 0。
- **大目錄**：每 10 000 個節點切一段。前面的段先寫出，後一段的 `prev` 指向它；
  父目錄記錄**最後一段**的名稱。讀取時沿 `prev` 收集所有段，再從最舊的一段開始讀。
  目錄前段沒變，那些段的名稱就沒變，照樣重用。
- **大檔案**：chunk 清單超過 256 個時改用 `Indirect`：清單本身編成 `ChunkList`，
  當作一般資料切 chunk 存進 pack；tree 裡只留這些 chunk 的 ID。
- 空目錄的 `subtree` 指向一個 `nodes` 為空的 tree；空檔案的 `chunks` 為空。

## 9. Snapshot

key：`snapshots/<client id hex>/<ts>`，`ts` 是 `YYYYMMDDTHHMMSSnnnnnnnnnZ`
（UTC，奈秒，無分隔符：字典序 = 時間序，且不含 Windows 檔名不允許的冒號）。
寫入必須用 conditional put（`PutMode::Create`），同一 key 不得覆蓋。

```
Snapshot {
  version: u32,
  client_id: bytes(16),
  hostname: text,
  username: text,
  time: text,                  RFC 3339 UTC，backup 開始的時間（與 key 的時間戳同一瞬間）
  paths: [bytes],              備份來源路徑
  root: ObjectId,              根 tree（若根目錄分段，是最後一段）
  parent: text | null,         上一個 snapshot 的 key，只用於加速
  stats: { files, dirs, symlinks, bytes_total, bytes_new, chunks_total, chunks_new, packs_new,
           errors, files_reused: u64 },
           errors = 讀不到而略過的項目數；files_reused = 走快速路徑沿用上次 chunk 清單的檔案數
}
```

`client_id` 由每台機器第一次使用時隨機產生、存在本機（不是用 hostname：改機器名稱不該變成新 client）。

## 10. Index

```
IndexBlob { version: u32, packs: [IndexPack], supersedes: [ObjectId] }
IndexPack { pack: ObjectId, size: u64, entries: [PackEntry] }
```

`supersedes` 列出這個 blob 取代的舊 index blob（M3 repack 用；M1/M2 為空）。
讀取端先讀完所有 blob、收集全部 `supersedes`，被列到的 blob **整個忽略**；
所以新舊 blob 同時存在時一律以新的為準，舊的之後走兩階段刪除。
index blob 本身沒有 snapshot 引用它；GC 判斷一個 blob 可不可刪的規則是
「它被取代了」或「它列的 pack 全部都已經不存在」。

只是 pack trailer 的快取，可從所有 pack 的 trailer 重建。`size` 是 pack 檔總長度，
讓 `check` 不讀資料也能用 HEAD 抓到被截斷或換掉的 pack。

## 11. GC 的相容性要求（M3 實作，格式現在先定）

- 「未被引用就刪」的物件不只 pack：tree 與 index 也是。backup 開始時列出的 `trees/`
  在 GC 刪掉某個 tree 後就不可信，所以 `gc/<object hex>` 對 pack、tree、index 一體適用，
  兩階段刪除（標記 → grace period → 刪）的規則也一樣。
- backup 沿用既有 chunk 時，必須記下它引用了哪些 pack（不是只查「存不存在」），
  才能執行「引用到被標記的 pack 就撤銷標記」。

## 12. 已知的設計限制（不打算在 v1 解決，寫下來免得被當成 bug）

- **回滾／刪除攻擊不可偵測**：有寫入權限的人刪掉最新幾個 snapshot，`snapshots` 與 `check`
  都看不出來。對策在 repo 之外：S3 versioning / Object Lock（M2），以及 client 本機記住
  自己最後寫出的 snapshot key 做比對（M2 的本地快取）。
- **snapshot key 是明文**：洩漏 client id 與備份時間（奈秒）。內容都是加密的。
- **時間戳來自 wall clock**：`latest` 與 parent 的選擇依 key 的時間排序，多台 client
  時鐘偏差會選錯；只影響快速路徑與顯示，不影響資料正確性。
- **client id 被複製**（clone VM）會讓兩台機器共用一個 snapshot namespace；
  parent 只在 `paths` 相同時才沿用，所以不會拿錯資料，但 GC 的活躍判定會混在一起。

## 13. 寫入順序（commit point）

backup 的寫入順序固定為：packs → trees → index → snapshot。
snapshot 是唯一的 commit point：它出現之前 repo 裡多出來的物件都只是垃圾，
GC 可以安全回收；它出現之後，它引用的所有東西都已經在 repo 裡。
