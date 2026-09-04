# kist repo 格式（v1）

這份文件與 `crates/kist-format` 一起維護：程式碼改了、這裡也要改，反之亦然。
每種結構在 `crates/kist-format/tests/golden/` 都有一份固定的範例 bytes；
任何會改變 on-disk bytes 的修改都會讓 golden 測試失敗，這是刻意的。

> 狀態：**v1 已於 2026-09-04（M1 結束）凍結**。之後只能透過 `version` 欄位演進；
> 任何改動都需要專案負責人確認，並同步更新 golden files 與這份文件。

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
| `gc/<pack hex>` | 待刪標記（M3 定義） | — | pack 名稱 |

「以 BLAKE3(整檔 bytes) 命名」指的是對**寫進 repo 的密文 bytes** 做一般（無 key）
BLAKE3，小寫 hex。任何人下載後都能在不持有金鑰的情況下驗證檔案沒被改過；
而因為 hash 的是密文，名稱不會洩漏明文的任何資訊。

## 2. 識別碼

- `ChunkId`（32 bytes）＝ keyed BLAKE3(hash key, chunk 明文)。它只出現在加密過的
  trailer / index / tree 內容裡，**永遠不會**成為 repo 的 key。
- `ObjectId`（32 bytes）＝ BLAKE3(物件密文 bytes)，即上表的 `<hex>`。

CBOR 內兩者都是 32-byte 的 byte string（major type 2），不是整數陣列。

## 3. 金鑰階層

```
password ─Argon2id(salt, m/t/p 來自 config)─▶ KEK (32B)
KEK ─XChaCha20-Poly1305 解開 wrapped_master_key（AAD = "kist v1 master key"）─▶ master key (32B)
master key ─blake3::derive_key(context)─▶
    "kist v1 hash key"    → chunk ID 用的 keyed hash key
    "kist v1 chunk key"   → chunk 加密
    "kist v1 object key"  → envelope 加密（tree / index / snapshot / pack trailer）
    "kist v1 nonce key"   → content-addressed 物件的決定性 nonce（§5.2）
```

Argon2id 參數存在 config 裡，開 repo 時**一律讀 config**，不寫死在程式裡；
日後要調高只需改 `init` 的預設值，舊 repo 不受影響。

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
  time: text,                  RFC 3339 UTC，給人看
  paths: [bytes],              備份來源路徑
  root: ObjectId,              根 tree（若根目錄分段，是最後一段）
  parent: text | null,         上一個 snapshot 的 key，只用於加速
  stats: { files, dirs, symlinks, bytes_total, bytes_new, chunks_total, chunks_new, packs_new: u64 },
}
```

`client_id` 由每台機器第一次使用時隨機產生、存在本機（不是用 hostname：改機器名稱不該變成新 client）。

## 10. Index

```
IndexBlob { version: u32, packs: [IndexPack] }
IndexPack { pack: ObjectId, size: u64, entries: [PackEntry] }
```

只是 pack trailer 的快取，可從所有 pack 的 trailer 重建。`size` 是 pack 檔總長度，
讓 `check` 不讀資料也能用 HEAD 抓到被截斷或換掉的 pack。

## 11. 寫入順序（commit point）

backup 的寫入順序固定為：packs → trees → index → snapshot。
snapshot 是唯一的 commit point：它出現之前 repo 裡多出來的物件都只是垃圾，
GC 可以安全回收；它出現之後，它引用的所有東西都已經在 repo 裡。
