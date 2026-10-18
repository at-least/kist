# kist repo 格式（v4）

> **狀態：設計已確認（2026-10-10，見 ADR 026），實作第 1–6 段完成（2026-10-11），第 7 段（收尾：凍結 v4
> fixture、fuzz、bench、量測）待開始。** 本文件是 v4 的唯一權威副本：`kist_format::v4` 與 `kist_crypto::v4`
> （實作第 2 段）、kist-backend 的 v4 契約（第 3 段）、kist-core 的 `v4` 模組（init、open 與 backup，第 4 段；
> restore 與 check，第 5 段；forget、prune、重建、settings、key、`remove-unknown-packs`，第 6 段）、kist-mount 的
> `v4` 模組（第 5 段）與 CLI、kist-app（第 6 段）照它實作；改動格式必須同時改程式碼、向量與本文件。v3 的
> 程式碼與 fixture 已全部移除（第 6、7 段）；v3 的規格 `docs/format.md` 也已刪除，v4 仍沿用的部分（切塊、
> 快速路徑合約、硬連結與深度、parity 的矩陣）已搬進本文件，原文以 `git show 0b3d425:docs/format.md` 取回。
> v4 不支援 v3：config 的 `v` 不是 4 就拒絕，沒有遷移（負責人：「不需要支援上一代，目前都還在內部開發
> 階段」）。
> 第一版草稿（commit `054c7e1`，「v3 加樹進 pack」）已被否決；本版從需求與 bug 歷史重新推導，
> 過程與每一層的取捨見 ADR 026。協定步驟的編號（B1–B5、P0–P9）是規範性的，實作與測試引用它們。

## 0. 與 v3 的差異總覽

| # | 變更 | 為什麼 |
| --- | --- | --- |
| 0.1 | 目錄樹、間接內容清單、root 清單都是 metadata item，放在 `mpacks/` | v3 的樹是獨立物件，存活要靠後端 mtime 表達，資料遺失方向的 bug 大多出在這裡；每個目錄每次 backup 都要請求 |
| 0.2 | GC 改成世代鏈：只有建立下一個世代 head 成功的 prune 可以刪東西 | 兩個 prune 重疊時無法同時生效；`gc/`、`touch/`、`indexes/` 與 supersedes 全部拿掉 |
| 0.3 | 刪除的依據只來自 prune 自己讀過、驗證過的資料（pack 的 trailer、解密後的內容） | 備份主機寫的資料不能左右 prune 刪什麼（重新設計 v4 時發現 v3 有這個缺口，見 ADR 026） |
| 0.4 | 任何物件都不覆寫，連 `config` 也是一次寫定 | bucket policy 可以對所有角色強制只能新增；刪除只屬於維護角色 |
| 0.5 | 時間判斷只比對維護角色寫的 head：後端時間與寫入者時間兩個時鐘都要過期 | 不再比較 client 時鐘、prune 主機時鐘與後端時間的差；時鐘出錯只會延後刪除 |
| 0.6 | backup 只沿用自己寫的、prune 驗證過的、或自己驗證過的副本 | 被入侵的備份主機不能讓別台的 backup 回報成功卻還原不了 |
| 0.7 | 目錄依內容切段，多段時有目錄索引；root 記錄種類，檔案來源直接存在 root 裡 | 大目錄改一個檔只重寫一段（v3 要重寫整條鏈）；修 V19、V21 |
| 0.8 | 每個 client 的物件放在自己的前綴下 | 部署時可以讓每台主機只能寫自己的前綴，出事可以追到是哪一台 |
| 0.9 | 金鑰槽以密碼推導出的名稱存放，開啟時只讀一個 | 不能塞爆槽數、不能用極大的 KDF 參數卡死開啟（ADR 017 的否決理由） |

沿用 v3、經比較後留下的：keyed BLAKE3 的內容識別、每項獨立的 XChaCha20-Poly1305、CBOR 十條規則、
FastCDC 與凍結的 gear 表、pack 的排版、snapshot 是唯一的 commit point、先標記再刪的兩階段刪除、
metadata 聯集（`mk`）與快速路徑合約。

---

## 1. 命名空間

repo 是 key → bytes 的命名空間。**每一次寫入都是一次只在不存在時建立的單一 PUT**（不分段上傳），
任何物件建立後都不再改變；刪除只由維護角色執行。

| key | 誰寫 | 內容 |
| --- | --- | --- |
| `config` | init，一次 | 明文 CBOR，不可變（§3） |
| `keys/<s>` | init、維護 | 金鑰槽（§3） |
| `packs/<c>/<hex>` | backup（client c）、prune | 資料 pack：檔案的 chunk |
| `mpacks/<c>/<hex>` | backup（client c）、prune | metadata pack：metadata item（§7） |
| `parity/<c>/<hex>` | backup、prune | 選配的 Reed-Solomon 同位，對應同名的 pack 或 mpack |
| `deltas/<c>/<hex>` | backup（client c） | 提示：這次 backup 寫的 pack 有哪些項目（§10） |
| `snapshots/<c>/<ts>` | backup（client c） | commit 記錄（§9） |
| `maint/gen/<g>` | prune、init | 世代 head（§10、§11），永不刪除 |
| `maint/map/<hex>` | prune | map 分片（§10） |
| `maint/probe-<p>` | prune | 時鐘與條件寫入探針，用完即刪 |

- `<c>` 是 client id 的 32 位小寫 hex。維護角色（prune 的 repack 輸出）用全零的 client id。
- `<hex>` 是物件 bytes 的 BLAKE3（無 key）的 64 位小寫 hex；parity 例外，與它對應的 pack 或 mpack 同名
  （P9 與刪除順序靠這一點找到它）。`<g>` 是世代號碼的 16 位小寫 hex（字典序＝數值序）；`<ts>` 是
  `YYYYMMDDTHHMMSSnnnnnnnnnZ`，只接受這一種寫法。`<s>` 是槽的名稱（§3），`<p>` 是 OS 亂數，兩者都是
  16 bytes 的 32 位小寫 hex。其他寫法（大寫、長度不對、多一段或少一段、不該有副本的種類帶 `.r1`、v3 的
  `trees/` 與 `indexes/`）都不是 v4 的 key。
- head 一律有一份 `<key>.r1`；`replicas=1` 時，mpack、snapshot、map 分片也各有一份，bytes 與主體相同
  （§13）。
- 讀取端必須驗證名稱：整份讀取的 delta 與 map 分片重算 hash；pack 以 range read 讀取，讀不到整份，
  以每一項重算自己的 ID 驗證（§2），pack 的名稱只是識別；head 與 snapshot 以 AAD 綁定 key 路徑，
  delta 綁定所在前綴的 client（§5）。

建議的權限（S3）：每台備份主機的憑證只能在自己的 `packs/<c>/`、`mpacks/<c>/`、`parity/<c>/`、
`deltas/<c>/`、`snapshots/<c>/` 底下建立物件，可以讀全部；`config`、`keys/`、`maint/` 與全零 client
的前綴只給 init 與維護角色；刪除只給維護角色。bucket policy 以 `s3:if-none-match` 強制條件寫入。

## 2. 識別碼

- chunk ID ＝ keyed BLAKE3(`K_chunk_id`, chunk 明文)，32 bytes。
- meta ID ＝ keyed BLAKE3(`K_meta_id`, metadata item 的規範 CBOR，壓縮之前)，32 bytes。兩把 key
  不同，一個 ID 不會同時是資料與 metadata。
- pack、mpack、delta、map 分片的名稱 ＝ BLAKE3(寫進 repo 的 bytes)；parity 與它對應的 pack 或 mpack 同名。
- CBOR 內的 ID 一律 32-byte byte string；路徑裡一律小寫 hex。

## 3. config 與金鑰

```
config (明文 CBOR，init 時寫一次，之後不變)
  { v: 4, repo_id: bytes(16), created: i64 奈秒,
    chunker: { min, avg, max }, kdf: { alg: "argon2id", t, m (KiB), p, salt: bytes(16) } }

KEK      = Argon2id(password, config.kdf)
槽的名稱  = hex(DeriveKey("kist/v4/slot-name", KEK)[..16])        → keys/<名稱>
槽的金鑰  = DeriveKey("kist/v4/slot-key", KEK)
槽        = CBOR { v: 4, wrapped: nonce(24) ‖ AEAD(槽的金鑰, master(32), AAD) ‖ tag(16) }
           AAD = "kist/v4/slot" ‖ BLAKE3(config 的 bytes)

master ─DeriveKey─▶ K_chunk_id "kist/v4/chunk-id"   K_meta_id "kist/v4/meta-id"
                    K_cut      "kist/v4/cut"        K_data    "kist/v4/data"
                    K_meta     "kist/v4/meta"       K_snap    "kist/v4/snapshot"
                    K_index    "kist/v4/index"      K_cache   "kist/v4/cache"
```

- 開啟 repo：讀 `config`、`v` 不是 4 → 拒絕；以密碼算出 KEK 與槽的名稱，只 GET 那一個槽。沒有掃描槽，
  KDF 參數只來自不可變的 config，所以塞進大量槽或極大參數的槽都卡不住開啟。
- config 被換掉（任何一個 byte）→ 所有槽的 AAD 都對不上 → 開不起來，訊息是「密碼錯誤或 config 被竄改」。
  不會出現 v3 早期那種「認錯 repo、以為 chunk 已存在」的無聲遺失。
- 換密碼：`key add` 以新密碼推導 KEK 與槽的名稱，把 master 包進新槽 `keys/<名稱>`（只在不存在時建立；這個
  密碼已有槽就拒絕）；`key remove`（維護角色）只在同一個程序開啟時用的槽仍在列表裡、而且不是要刪的那一個
  時才刪，最後一個槽刪不掉。`key remove` 拿同一台主機的維護檔案鎖（§11.3），不走柵欄、不需要 P0；不同主機
  同時互刪對方的槽仍可能兩個都刪掉——這個命令很少用，接受這個風險（實作第 6 段）。槽的 AAD 綁 config 原樣
  的 bytes，所以實作存著讀到的 config bytes、不重新編碼（重新編碼可能丟掉未知的欄位）。
- 代價：KDF 參數在 init 時決定，之後不能調高；要更強的參數就開新 repo。
- 本地 index 快取的目錄名稱 ＝ hex(keyed BLAKE3(`K_cache`, repo 位置)[..16])：換 repo 位置就不共用（同 v3，
  ADR 004：複製到第二個地方的 repo 可能少了東西）。

## 4. CBOR 規則與欄位表

所有 metadata 都是 CBOR（RFC 8949），規範編碼條件與 v3 相同：

1. 整數一律最短編碼。
2. 長度一律 definite。
3. 沒有浮點數：時間是 i64 奈秒、大小是 u64、列舉是 u8。
4. 沒有 tags。
5. struct ＝ map，依下表的欄位順序輸出；解碼端按名稱取值。
6. 二進位資料（名稱、ID、xattr、etag、version）用 byte string；人類可讀字串用 text string。
7. 唯一的自由 map 是 `xattrs`，key 依 bytes 字典序。
8. 解碼拒絕：重複 key、indefinite length、float、tag、尾端多餘 bytes、規定必須缺席卻在場的欄位、
   必填欄位缺席。
9. 忽略未知欄位，但絕不回寫。
10. 新增欄位一律可省略（零值省略）。

| 結構 | 欄位順序 |
| --- | --- |
| `Config` | v, repo_id, created, chunker, kdf |
| `Chunker` | min, avg, max |
| `Kdf` | alg, t, m, p, salt |
| `Slot` | v, wrapped |
| `PackTrailer` | v, kind, entries |
| `PackEntry` | i, o, l, r |
| `Seg` | v, k, entries |
| `Dir` | v, k, level, segs |
| `DirSeg` | first, id |
| `ChunkList` | v, k, chunks |
| `ChunkIndex` | v, k, level, parts |
| `ChunkPart` | size, id |
| `RootSeg` | v, k, roots |
| `Entry` | n, t, mk, size, target, ct, chunks, tree, mode, uid, gid, mtime, ctime, dev, ino, nlink, xattrs, etag, vern |
| `Root` | path, t, tree, entry |
| `Snapshot` | v, roots, time, host, user, client, parent, stats |
| `SnapshotStats` | files, dirs, symlinks, bytes |
| `Delta` | v, client, packs |
| `DeltaPack` | id, kind, size, entries |
| `Head` | v, g, w, parent, shards, settings, drain, bad, hold |
| `Settings` | grace, replicas, pack_target, mpack_target |
| `Drain` | from, old_grace |
| `Bad` | key, first |
| `Hold` | from, lost, grace |
| `Shard` | v, index, records |
| `Record` | id, client, kind, size, entries, by_prune, since, status, held |
| `Status` | s, at, age |

## 5. Sealed 物件

`nonce(24) ‖ ciphertext ‖ tag(16)`，XChaCha20-Poly1305，全部 OS 亂數 nonce。

| 物件 | 金鑰 | AAD |
| --- | --- | --- |
| chunk（`packs/` 內的一項） | `K_data` | chunk ID |
| metadata item（`mpacks/` 內的一項） | `K_meta` | meta ID |
| pack／mpack 的 trailer | `K_index` | `"kist/v4/pack-trailer"` |
| snapshot | `K_snap` | key 路徑（`snapshots/<c>/<ts>`，不含 `.r1`） |
| delta | `K_index` | `"kist/v4/delta"` ‖ client id(16) |
| head | `K_index` | key 路徑（`maint/gen/<g>`） |
| map 分片 | `K_index` | `"kist/v4/shard"`；分片號碼在明文裡，必須等於 head 指向它的位置 |
| 金鑰槽 | 槽的金鑰 | `"kist/v4/slot"` ‖ BLAKE3(config) |

snapshot 的密封金鑰與 AAD 在 v4 這一代內固定不變：較舊的 prune 必須永遠解得開較新的 snapshot，解開後
看不懂版本就拒絕，不能把它當成壞掉的物件（§11.3 P5）。

## 6. Pack 與 mpack

```
+-------------------+----------------------+------------------+--------------------+-------------------+
| magic 8B          | entry × N            | trailer (sealed) | trailer len u64 BE | magic 8B          |
+-------------------+----------------------+------------------+--------------------+-------------------+
entry = nonce(24) ‖ AEAD(alg byte ‖ data) ‖ tag(16)
```

- magic ＝ `"kistpk"` ‖ u16 big-endian `0x0004`，頭尾一致；trailer 的 `v` ＝ 4。
- trailer `{v, kind, entries}`：`kind` 0 ＝資料 pack（只放 chunk）、1 ＝ mpack（只放 metadata item）；
  命名空間必須與 `kind` 相符。`entries` 每項 `{i, o, l, r}`：ID、在檔內的位移、sealed 長度、明文長度
  （壓縮前，不含 alg byte）。一致性檢查：從位移 8 起連續排列、最後一項結束在 trailer 的起點、沒有重複 ID、
  每項的 `l` 在 41（nonce＋alg＋tag）到上限＋41 之間、`r` 不超過上限（資料是 `chunker.max`，metadata 是
  2 MiB）、位移不溢位。寫入端同樣檢查，不一致就不寫。
- `alg`：0 原文、1 zstd level 3；壓縮省不到 1/16 就存原文：省下的 bytes × 16 ≥ 原文長度才存壓縮版
  （整數比較，不用 `原文 / 16`：整數除法讓很短的原文留下沒省到的壓縮版）。解壓上限：chunk 是 `chunker.max`，metadata
  item 是 2 MiB——**與 trailer 宣稱的 `r` 無關**，先限制輸出再比對長度。
- 名稱＝BLAKE3(整檔)。寫滿 `settings.pack_target`（資料，預設 64 MiB）或 `settings.mpack_target`
  （metadata，預設 8 MiB）時寫出，backup 結束時寫出剩下的。

### 6.1 切塊（FastCDC）

檔案內容以 FastCDC（Xia et al., USENIX ATC '16）normalized level 2 切塊。邊界函式是凍結的格式：同一份 bytes 與
同一組參數在任何實作上都必須切出完全相同的邊界（v2 起凍結，v4 沿用，原文從 v3 規格搬到這裡）。

- gear 表 ＝ fastcdc-go v0.2.0 的 256 × u64 表，逐 byte 凍結：各 entry 以 little-endian 串接後的 SHA-256 ＝
  `a98fa4184eb747cd769328307242285f9d4b25afeb28f71ef3fe437a4c278e28`。實作內嵌同一張表，並以 digest 測試釘死
  （`kist_chunker::GEAR_TABLE`）。
- 參數 `min`、`avg`、`max` 來自 config（§3；讀取端驗範圍：min 64 到 1 MiB、avg 256 到 16 MiB、max 1 KiB 到
  64 MiB、min ≤ avg ≤ max），不是寫死的；同一個 repo 的所有 client 用同樣的參數，去重才一致。預設
  512 KiB／2 MiB／8 MiB。
- mask 由 avg 以整數運算推導（不用浮點數）：`bits = round(log2(avg))`、`mask_s = (1 << (bits + 2)) − 1`、
  `mask_l = (1 << (bits − 2)) − 1`（avg ＝ 2 MiB 時 bits ＝ 21：`0x00ff_ffff`／`0x0007_ffff`）。
- 邊界函式（回傳從目前位置起下一個 chunk 的長度）：剩餘輸入不超過 `min` 就整段一塊；否則
  `limit = min(剩餘, max)`、`normal = min(limit, avg)`，指紋 `fp` 從 0 起、從第 `min` 個 byte 開始逐 byte
  `fp = (fp << 1) + gear[byte]`（wrapping）：位置未滿 `normal` 時 `fp & mask_s == 0` 就在這個 byte 之後切，
  之後到 `limit` 之前用 `mask_l`；都沒切到就在 `limit` 切（硬邊界 `max`，或輸入耗盡）。
- 緩衝無關性：讀取器在決定邊界前保證掃描位置之後至少 `max` bytes（除非輸入已耗盡），所以邊界與 reader 的分塊
  方式無關。
- 凍結向量：`crates/kist-chunker/tests/testdata/` 的固定輸入邊界清單（原本與已移除的 Go 實作互驗）與 gear 表的
  digest 測試；只在格式升版時重錄。

## 7. Metadata items

每個 item 是 `alg byte ‖ CBOR`，以 `K_meta` 封、AAD 是自己的 meta ID。`k` 標示種類，在受認證的明文裡。

- **Seg**（`k=0`）`{v, k, entries:[Entry]}`：一個目錄的一段，entries 依名稱 bytes 升冪、不重複。
- **Dir**（`k=1`）`{v, k, level, segs:[{first, id}]}`：目錄有多段時的索引。`level=1` 時 `id` 指向 Seg，
  `level=2` 或 `3` 時指向下一層 Dir。
- **ChunkList**（`k=2`）`{v, k, chunks:[chunk ID]}`：檔案的 chunk 超過 256 個時的清單，或清單的一段；非空。
- **ChunkIndex**（`k=4`）`{v, k, level, parts:[{size, id}]}`：chunk 清單有多段時的索引。`size` 是那一段
  涵蓋的明文 bytes（chunk 明文長度的合計），讓 mount 隨機讀取時可以跳過不需要的段。`level=1` 時 `id`
  指向 ChunkList，`level=2` 或 `3` 時指向下一層 ChunkIndex。
- **RootSeg**（`k=3`）`{v, k, roots:[Root]}`：snapshot 的 roots 的一段，依 `path` bytes 升冪、不重複；
  多段時同樣以 Dir 索引。

**切段規則**（目錄、root 清單、chunk 清單都用）：依順序加入；加入某一項之前這一段已有至少 1024 項，
而且那一項的 keyed BLAKE3(`K_cut`, 名稱) 前兩個 byte 以 little-endian 讀成的整數低 12 位元全為 0，就在
這一項之後切；加入下一項會讓這一段的規範 CBOR 超過 2 MiB 時，也在它之前切（大小照實算：每項的編碼長度、
容器的固定部分、陣列標頭）。chunk 清單沒有名稱，以 chunk ID 的 32 bytes 代替，順序就是檔案內容的順序；
內容中間改了一段，只有附近的段會變。每個 entry 的 xattrs 合計（名稱＋值）上限 256 KiB。一項超過任一上限時寫不
進去：backup 把它記成這個項目的錯誤、不寫入，結束碼 3。Dir 與 ChunkIndex 本身同樣不超過 2 MiB，最多
三層；一層 ChunkIndex 平均涵蓋約 400 TiB（以平均 2 MiB 的 chunk 計），檔案大小沒有實際上限。

**讀取端驗證**（任何一條不成立都是腐壞，大聲失敗）：Dir 非空、`first` 嚴格遞增、`level` 在 1 到 3；
每一段非空，而且第一項的名稱等於它在 Dir 裡的 `first`，每一項都小於下一段的 `first`（查找靠它）；段內
依名稱升冪、不重複；`level` 與指向的種類相符；`ct=0` 的 chunks
不超過 256 個；ChunkList 非空；ChunkIndex 非空、每一段的 `size` 大於 0，每一段 chunk 明文長度的合計（或下一層
`size` 的合計）等於它的 `size`，全部 `size` 的合計等於 Entry 的 `size`；上面的大小上限。上限在兩處執行：
解碼時拒絕超過上限的內容（item 超過 2 MiB、xattrs 超過 256 KiB、etag 或 vern 超過 1 KiB）；建構時一項
自己放不進一段就回錯誤，由 backup 記成那個項目的錯誤。

## 8. Entry（metadata 聯集）

欄位、per-kind 規則、快速路徑合約、硬連結範圍、深度上限與 v3 相同（原文搬進 §8.1、§8.2），只差 `tree` 的意義：

- `tree`：子目錄內容的 meta ID，指向一個 Seg（只有一段）或 Dir（多段）。
- `ct=1`：`chunks` 恰好一個元素，是 ChunkList（只有一段）或 ChunkIndex（多段）的 meta ID。

| mk | 必填 | 選填 | 必須缺席 |
| --- | --- | --- | --- |
| 0 posix | mode, uid, gid, mtime | ctime, dev, ino, nlink, xattrs, etag, vern | — |
| 1 sftp | mtime | mode, uid, gid | ctime, dev, ino, nlink, xattrs, etag, vern |
| 2 s3 | — | mtime, etag, vern | mode, uid, gid, ctime, dev, ino, nlink, xattrs |
| 3 generic | — | mtime | 其餘全部 |

快速路徑：posix 看 size、mtime、ctime、inode；s3 看 etag 與 size；sftp 與 generic 一律重讀。只看 mtime
一律非法。硬連結以 `(dev, ino)` 在整個 snapshot（跨 roots）內判定。DIR 巢狀的實作上限 256 層。

### 8.1 快速路徑合約（內容可證明 > kernel 可證明 > 來源聲稱）

backup 把來源的一個項目與 parent snapshot 裡同一條路徑的 Entry 比較，條件成立就沿用它的內容（chunk 清單）、
不重讀：

| 來源 | 沿用條件 | 依據 |
| --- | --- | --- |
| mk=0 posix | size、mtime、ctime、inode 都沒變（dev 只在 `nlink>1` 時記錄，供硬連結識別，不參與快速路徑比較） | kernel 維護，使用者改不了（size 在實作的外層判斷比較） |
| mk=2 s3 | etag 相同 + size 相同 | etag 是來源**計算並保證**的內容指紋（單段上傳＝MD5；多段／KMS 的 etag 仍是來源定義的確定性指紋） |
| mk=1 sftp、mk=3 generic | **無安全快速路徑**——一律重讀，靠 chunk 去重吸收 | mtime 與 size 都是 client 可設的（`cp -p` 陷阱） |

只看 mtime 的沿用**非法**（任何來源）。`etag`／`vern` 存在 Entry 裡就是為了下一次 backup 能做這個比較——
這是格式欄位，不是 client 的記憶體狀態（不同機器、不同版本的 client 都要能比）。parent 必須一直存在
（§9 的 forget 規則、B4 的 HEAD）：沿用子樹時不查子樹裡每個 item 的持有者。

### 8.2 硬連結與巢狀深度

- 硬連結：`(dev, ino)` 的同一性判定範圍＝**整個 snapshot（跨 roots）**：兩個 root 裡共用 (dev, ino) 的條目是
  同一份內容——`bytes` 只算一次、restore 重建為一個 hard link（第一個出現的位置建立，其餘 link；失敗降級成
  複本＋警告）。
- 巢狀深度：DIR 的巢狀**沒有格式上的深度限制**，但實作設有上限 256 層。來源路徑允許的巢狀其實可以超過它
  （ext4 單元件可以短到 2 bytes，PATH_MAX 內疊得出約 2000 層），所以 backup 的寫入端套同一把尺——超過上限的
  來源子目錄跳過並記警告，寫出的樹因此永不超限。restore、mount、check、prune 把超過上限的鏈當腐壞或敵意
  repo 乾淨回錯，而不是把走訪遞迴墊進 stack overflow；同一目錄的多段（Dir 索引）用迴圈走，不計深度。

## 9. Snapshot

```
Snapshot { v: 4, roots: meta ID（RootSeg 或 Dir）, time: i64 奈秒（backup 開始）, host: text,
           user: text（省略若空）, client: bytes(16), parent: text（沒有就省略）, stats }
Root     { path: byte string, t: u8, tree: meta ID（t=1）, entry: Entry（t=0 或 2） }
```

- `Root.t` 是來源本身的類型（0 檔案、1 目錄、2 符號連結），必填。`t=1` 必須有 `tree`、不得有 `entry`；
  `t=0` 或 `2` 必須有 `entry`、不得有 `tree`，`entry.t` 等於 `Root.t`，`entry.n` 等於定位的最後一段。
- 定位切段：有 `scheme://` 就去掉，以 `/` 切，空段與 `.` 丟掉，`..` 映射成 `__parent__`，段裡有 NUL 是
  腐壞。`/srv/data` 切成 `srv`、`data`；`s3://bucket/prefix` 切成 `bucket`、`prefix`；`/` 沒有段。
- restore 映射：`t=1` 的內容放在 `<target>/<所有段>`（沒有段就直接放在 `<target>`）；`t=0` 或 `2` 放在
  `<target>/<去掉最後一段>/<名稱>`。`t≠1` 時定位至少要有一段。不再從內容猜。
- `parent`：同一個 client、roots 的路徑與 `t` 相同的上一個 snapshot 主體的 key，只用於加速；沒有就省略，
  明寫的 null 也視為沒有。
- 讀取端：內容的 `client` 與 `time` 必須等於 key 的 client 與時間戳（`.r1` 也一樣）；`parent` 必須是同一個
  client 的 snapshot 主體的 key。主體讀不了（不見、解不開、內容與 key 不符）就讀 `.r1` 並警告（P5 的順序；
  `.r1` 在 B4 通過之後才寫，內容自己是完整的）；兩份都不在是「找不到」，不是腐壞。列出 snapshot 時只列主體
  （主體出現＝commit），只有 `.r1` 的由 check 回報（§13）。
- stats：`files`、`symlinks` 按名稱計；`dirs` 是目錄節點數，roots 不算；`bytes` 是內容總和，`(dev, ino)`
  只算一次。過程計數不在格式裡。
- commit：`replicas=1` 時先寫 `.r1`，再寫主體；主體出現＝commit。只在不存在時建立得到「已存在」時先讀回
  比對：bytes 相同就是自己剛寫進去的（S3 回 5xx、client 重試、第一次其實已寫入，§12），commit 已完成；不同
  才是撞名，時間戳 +1 奈秒、重新密封（AAD 與 `time` 都帶著時間戳），`replicas=1` 時連 `.r1` 一起重寫。讀回
  失敗就是 commit 與否不明，以錯誤結束。每次密封都用亂數 nonce，所以 bytes 相同幾乎只可能是自己那一份。
- forget：不刪任何 client 在同一組 roots 上最新的那一份 snapshot（那是下一次 backup 的 parent）；刪除時先
  刪 `.r1` 再刪主體（主體＝commit，中途停下留下的是一個完整的 snapshot）。強制刪除那一份等於讓這個 client 的這組 roots 退役，只能在它沒有進行中的 backup 時
  做，刪除完成之前也不能開始新的 backup：backup 沿用 parent 的子樹時不查子樹裡每個 item 的持有者，靠的是 parent 一直存在（B4 的 HEAD 只能
  縮小、不能消除這個空窗）。「同一組 roots」是 (client, 每個 root 的 `path` 與 `t`)，與 B1 找 parent 的條件
  相同；保留政策在每一組裡套用。roots 讀不出來的 snapshot 分不了組：政策不碰它、只回報，它也不會是 parent
  （backup 跳過讀不了的）。明確指定接受 `latest`、完整的 key、`<client>/<時間戳>`、時間戳或它的前綴；完整的
  key 照列表找主體或 `.r1`、不解密，所以只有 `.r1` 的、兩份都解不開的 snapshot 都刪得掉（P5 停住時維護角色的
  解除方法）。forget 不建立任何物件、不走柵欄、不需要 P0（§12）。

## 10. Index：map 與 delta

**Head** `maint/gen/<g>`：

```
Head { v: 4, g, w: 寫入者的牆鐘時間（i64 奈秒）, parent: BLAKE3(head g−1 寫進 repo 的 bytes)（g=0 時省略）,
       shards: [256 個 hash，空分片為 null], settings: { grace, replicas, pack_target, mpack_target },
       drain: { from: g, old_grace } | 省略, bad: [{ key, first: g }] | 省略,
       hold: { from: g, lost: g, grace: 秒 } | 省略 }
```

- `parent` 是 head g−1 寫進 repo 的（密封後的）bytes 的 BLAKE3，主體與 `.r1` 相同；g > 0 必填。
- `T(g)` ＝ head g 的後端修改時間（讀到的那一份：主體，主體讀不了時是 `.r1`），截到秒。head 永不刪除，所以每個世代的 `T` 與 `w` 永遠查得到。
- init 寫 head 0：空的 map、預設 settings（grace 72 小時；replicas 本機 1、S3 與 SFTP 0，同 v3；
  pack_target 64 MiB；mpack_target 8 MiB）。改 settings 是一個不刪任何東西的世代（§11.4）。
- settings 的範圍：grace 至少 2 小時；replicas 0 或 1；pack_target 與 mpack_target 在 64 KiB 到 4 GiB
  （同 v3 的 pack_target）。v3 的「`chunker.max` ≤ pack_target」不保留：target 只是寫出的門檻（§6），比它
  大的 chunk 照樣放得進一個 pack，而且 target 在可以改的 head 裡、chunker 在不可變的 config 裡。
- 讀取端：明文的 `g` 必須等於 key 的世代；`shards` 恰好 256 個；`drain.old_grace` 大於現在的 grace（drain
  只在調低時開始，期間新的 condemned 用它當 `age`）；`hold.from ≤ hold.lost`、`hold.grace` 至少 2 小時；
  `bad` 的 `key` 只能是 snapshot 主體的 key（P9 會刪它的主體與 `.r1`）。

**Map 分片** `maint/map/<hex>`：`Shard {v, index, records}`；分片 i 放名稱第一個 byte 等於 i 的所有 pack
與 mpack 的記錄，依名稱嚴格遞增（一個 pack 只有一筆記錄）。`index` 必須等於 head 指向它的位置。

```
Record { id, client, kind, size, entries: [{i, o, l, r}], by_prune: bool, since: g,
         status: { s: 0 live | 1 condemned | 2 tombstone | 3 unreadable | 4 unknown, at: g, age: 秒 },
         held: bool（只在重建後的 hold 期間有意義，§11.4） }
```

- `entries` 取自 prune 自己讀到的 trailer；之後唯一的改變是移除被證明不實的項目（P7c、P7d）。
- `by_prune`：prune 自己寫出、讀回驗證過的 pack，所以一定在全零 client 的前綴下；不在就是腐壞（B2 不驗證
  就信它）。
- `status` 的欄位依 `s` 在場（§4 第 8 條）：condemned 必須有 `at` 與 `age`（是一個 grace 值，至少 2 小時）；
  tombstone 必須有 `at`、不得有 `age`；live、unreadable、unknown 兩個都不得有；`s` 不認得就拒絕。
- `since`：這筆記錄進入 map 的世代。
- **排序**（選正本、讀取順序）：live 先於 condemned，其次 `since` 小的先，其次名稱小的先。新進的 pack
  不會取代既有的持有者成為正本。tombstone 排在最後（只在其他都失敗時讀，P6 的正本不選它）；unreadable 與
  unknown 不是持有者。
- 格式層不檢查、由 prune 照協定維持的值：`drain.from ≤ g`、`hold.lost < g`、`bad.first ≤ g`（指向還不存在的
  世代時，讀那一代的 head 就失敗）、記錄項目的位移範圍與重複（讀到的 bytes 解不開或 ID 不符，就換下一個
  持有者）。

**Delta** `deltas/<c>/<hex>`：`Delta {v, client, packs: [{id, kind, size, entries}]}`，backup 寫，只是
提示：讓別台在 prune 收錄之前就能去重與還原新 pack 裡的東西。prune 從不依 delta 做決定。delta 只能描述
同一個 client 前綴下的 pack：`DeltaPack` 沒有 client 欄位，它的 key 由 delta 所在的前綴與 `kind` 決定
（`deltas/<c>/` 只指向 `packs/<c>/` 與 `mpacks/<c>/`）；明文的 `client` 與前綴不同的 delta 視同解不開，
跳過並警告。否則一台主機可以替別台的 pack 宣稱它沒有的內容。

**讀取端**（backup、restore、mount、check）：
- 列 `maint/gen/`（從上次看過的世代 start-after），取**列出來的最大世代**，讀主體、不行讀 `.r1`。讀不了
  或版本不認得：backup 與 prune 停止；restore、mount 與 check 退回較舊的世代並警告：由大到小取第一個 head
  與它的全部分片都讀得了的世代，跳過的逐一警告（check 把它們當錯誤回報）。
- 分片依 hash 快取。delta 只用當下列表裡有的；解不開的 delta 跳過並警告。map 沒提到的 pack 才看 delta。
- 一個 ID 的持有者依排序逐一嘗試；某一份驗證失敗（解不開或 ID 不符）就試下一份；tombstone 的持有者
  排最後、只在其他都失敗時才用並警告；全部失敗 → 重新載入一次 → 仍失敗就報錯。重新載入＝重新列出世代、
  載入最大可用那一代的 map 與當下列表裡的 delta；主要的情況是開始得比 prune 早的讀取（item 被搬進 prune 的
  新 pack、記在下一代，舊的 pack 過了 grace 被刪）。限流：兩次因失敗而重新載入之間至少 60 秒（同 v3 的
  restore），別的讀取在這一項開始之後已經重新載入過就直接用新的再試；被限流擋下就回原本的錯誤。mount 看到
  沒見過的 snapshot 時另外重新載入（限流 1 秒，與前者分開計）：新的 snapshot 可能引用 mount 載入時還不知道
  的 pack。check 不重新載入：它回報讀到的那一代。
- 目錄的查找沿 Dir 的索引只讀涵蓋名稱的那一段（最後一個 `first` ≤ 名稱的段，§7 的檢查照做）。

## 11. GC 與並行（世代柵欄）

### 11.1 符號

- `T(g)`、`w(g)`：head g 的後端時間與寫入者時間。`grace` 取自 head 的 settings（預設 72 小時，至少
  2 小時）。`M` ＝ 1 小時。
- backup 的經過時間 `E` ＝ max(牆鐘經過, 含休眠的單調時鐘經過)。Linux 用 `CLOCK_BOOTTIME`；其他平台的
  對應時鐘未驗證（UNVERIFIED）。
- prune 的整輪（P0 到 P9 結束）必須在 `grace − M` 內完成，以同樣的時鐘計；超過就停止，不再寫 head、
  不再刪任何東西。
- **兩個時鐘的規則**：任何「自某個世代起已過一個 grace」的判斷，後端時間 `T` 與寫入者時間 `w` 都要成立
  ——P7d 的刪除、drain 的結束、重建 hold 的結束、P5 壞 snapshot 的等待。比較的「現在」每個時鐘取 base
  head 的值與這一輪 P0 的讀數中較小的：`min(T(g), N_P)` 與 `min(w(g), P0 時的本機牆鐘)`；P5 的等待只用
  P0 的讀數（等待期間每一輪都停住、不寫 head，`T(g)` 不前進）。取小只會讓現在更早，所以不需要新的假設。
  原本只用 base head 的值：後端時鐘在寫某個世代 c 時慢了、之後某台主機在寫 base head 時快了，兩邊就都
  提早成立（第二輪外部審查 C1–C3）；現在「現在」那一側要被騙過，那個時鐘得在 base head 寫成時和這一輪
  P0 時都錯。反過來，判斷「不可能已過一個 age」時（重建的沿用條件）只看 `w`：`T` 是物件的修改時間，
  主體遺失時改讀 `.r1`、主體被 `check --repair` 重建時都會變，標 tombstone 那時讀到的 `T` 和現在讀到的
  可能不同；`w` 在 head 的內容裡，兩份相同（第二輪外部審查 D1：`.r1` 晚了 72 小時才寫，主體遺失之後
  `T` 的最大減最小變成 0，誠實的時鐘下也資料遺失）。只看一個時鐘的比較只出現在不承擔安全性的地方：P7b
  的年輕規則、P7e、P7g、P9 的分片與 delta 清理（最壞是多一次重建或多留一些空間）。這條規則的三個違反
  （drain、hold、重建的沿用條件）是外部審查在 GC 模型上找到的，之前兩輪設計審查都沒發現。

### 11.2 backup

- **B1** 記下開始時間（牆鐘與單調時鐘）。讀列出來的最大世代 `g_s`（必須讀得了、版本認得，否則停止），
  載入它的分片；列 `deltas/` 並讀新的；找 parent（自己最新、roots 相同的 snapshot）。列表裡沒有 `g_s` 的
  `.r1`（寫它的人還在兩次 PUT 之間，或已經當掉）→ 稍候重新列出一次，仍然沒有就停止：只有一份的 head
  遺失之後，那一代可以被別的計畫重新寫出，照它決定的沿用可能指向已被刪掉的 pack（GC 模型找到：寫入者
  當掉、主體遺失、被入侵的主機的偽造 snapshot 與另一個 prune 同時發生時資料遺失）。下一次維護會補齊它
  （P1）。
- **B2** 一個 chunk 或 metadata item 只在兩個條件都成立時沿用，否則重新寫：
  - 有種類相符的持有者：`Map(g_s)` 裡 live 的記錄、當下列表中某個 delta 提到而 `Map(g_s)` 沒提到的
    pack、或這次自己寫的 pack；
  - 而且可信：持有者是 `by_prune`、是自己寫的，或這台 client 曾從這個持有者取回這個 item 並驗證過
    （解密、重算 ID；結果存在本地快取）。
  從 parent 沿用整棵子樹時，子樹的根 item 照同一條規則判斷，並記下 parent 的 key。記下這次去重用到的
  pack，以及其中哪些來自 delta。
- **B3** 以只在不存在時建立的方式寫 pack（mpack 連同 `.r1`，選配 parity），再寫 delta 列出它們；可以寫
  中間的 delta，重跑時沿用已上傳的 pack。`E ≥ grace − M` 之後不再寫 delta。
- **B4**（緊接在寫 snapshot 之前）讀列出來的最大世代 `g_c`（必須讀得了；`g_c < g_s` 是錯誤）。要求
  `E < min(grace(g_s), grace(g_c)) − M`。去重用到的每個 pack：在 `Map(g_c)` 裡 → 不是 tombstone、
  unreadable 或 unknown，而且沿用的 ID 都還在它的記錄裡（`g_c = g_s` 時 B2 已經查過）；不在 `Map(g_c)`
  裡：B2 時來自 `Map(g_s)` 的 → 停止（誠實的流程裡 live 的記錄要先成為 tombstone、再過一個 grace 才移除，
  不會在一次 backup 之內離開 map；GC 模型也是停止）；來自 delta 的 → HEAD 它，再讀它的 trailer 確認沿用的
  ID 在裡面。自己這次寫的 pack 不能是 tombstone。沿用了
  parent 的子樹時 HEAD parent，不在就停止。任何一項不成立：不 commit，以錯誤結束。
- **B5** 再檢查一次 `E`；`replicas=1` 時寫 snapshot 的 `.r1`；再檢查一次 `E`；寫 snapshot 主體。commit。

### 11.3 prune

只在通過條件寫入探針的後端上執行。同一台主機以檔案鎖保證一次一個維護命令：鎖檔在這個 repo 的本地快取目錄裡
（`maint.lock`），沒有快取時在系統的暫存目錄、以快取目錄的名稱（§3）區分 repo；prune、重建、改 settings、
`remove-unknown-packs` 與 `key remove` 用同一把鎖。維護命令一律讀後端、不用本地快取：它們依讀到的內容決定刪
什麼，快取裡的那一份證明不了後端還在。

- **P0** 後端的條件建立不是原子的（`rclone://`，§12）→ 拒絕，什麼都不寫。否則建立 `maint/probe-<亂數>`，
  再以同名建立一次，必須得到「已存在」；HEAD 它，後端時間與讀完之後的本機牆鐘相差超過 5 分鐘 → 拒絕。
  記下探針的後端時間 `N_P` 與那一次讀到的本機牆鐘（§11.1 的「P0 時的本機牆鐘」）。P0 結束時不論結果都
  刪掉探針（也驗證了維護角色能刪）。
- **P1** 讀列出來的最大世代 g；版本不認得、讀不了、或沒有修改時間 → 拒絕。`T(g)` 比本機時鐘晚超過 `M`
  → 回報，這一輪以「本機時鐘 + M」代替 `T(g)` 當作現在（只會更保守；`T(g)` 當成某個世代的起點時仍用
  原值）。原本在這裡拒絕：一個時間戳錯誤的 head 會讓 GC 停到真實時間追上它，連改 settings 都過不了
  （GC 模型的收尾活性檢查找到）。較舊的 head 之間的時間異常只警告。列表裡有 g 的主體、沒有 `<g>.r1`
  → 以只在不存在時建立的方式補寫 `.r1`（讀到的主體的 bytes），再往下做：寫入者在兩次 PUT 之間當掉時，
  那一代只有一份；在它上面建 g+1 之前先補齊，之後它的主體遺失才不會在世代之間留下空洞（重建要讀範圍內
  每一代的 head，讀不到就永遠拒絕，GC 與所有 backup 都停住；GC 模型找到）。改 settings 與重建讀最新
  head 時同樣補寫；補寫失敗就停止，不在只有一份的 head 上建下一代。
- **P2** 依序列出：`snapshots/`、`deltas/`、`packs/` 與 `mpacks/`（含 `.r1`）、`parity/`、`maint/map/`。
  先列的物件寫在它所依賴的物件之後，所以那些物件一定出現在後列的清單裡。
- **P3** live 或 condemned 的記錄，主體與 `.r1` 都不在 → 先重新列出 `maint/gen/`：已經有比 g 新的 head，
  就只是輸掉柵欄，停止；沒有才拒絕（資料遺失，請執行 check 與修復）。只缺主體、`.r1` 在 → 回報、
  繼續。held 的記錄消失時直接移除（§11.4）。重新列出 head 是外部審查找到的：兩個 prune 以同一代為
  基礎時，贏的那個刪掉了一個在共同基礎裡仍是 condemned 的 pack，輸的那個原本會誤報資料遺失。
- **P4** 列表裡有、map 裡沒有的 pack 是新進者：讀它的 trailer，讀得了 → live（`since = g+1`）；讀不了
  （解不開、排版不對、種類與命名空間不符）→ unreadable（`since = g+1`），沒人會用它；解得開但版本不認得
  → unknown：不使用、不刪除、每一輪回報，也不擋住 prune 與重建（prune 不刪看不懂的東西；要移除由維護
  命令處理）。原本是拒絕：一個這樣的 pack 就讓 GC 永遠停住，再遇上分片遺失，重建也失敗、所有 backup
  都停（外部審查）。
- **P5** 走訪每一個 snapshot，收集被引用的 ID。只有 `.r1`、沒有主體的 snapshot 一樣當成起點並回報
  （可能是 commit 前中斷，也可能是主體遺失）。主體解不開 → 試 `.r1`；兩份都解不開：在 head 的 `bad`
  清單裡已滿一個 grace（`T(first) + grace ≤ N_P`）→ 不再當成起點、回報；否則這一輪不改任何記錄、不刪
  任何東西，回報後停止——它還不在 `bad` 裡時，先以柵欄寫 head g+1 把它記進去（`first = g+1`，分片
  不變）。「已滿一個 grace」是 `T(first) + grace ≤ N_P` 而且 `w(first) + grace ≤` P0 時的本機牆鐘：比的是
  P0 的時間、不是 `T(g)`，因為等待期間每一輪都停在這裡、不寫新的 head，`T(g)` 不會前進（GC 模型找到的
  死結）。維護角色可以直接 `forget <key>` 立刻解除，不需要解密。解得開但版本不認得 → 拒絕。列表之後
  兩份都消失 → 視為已 forget，略過。某個 ID 沒有任何可讀、非 tombstone、種類相符的持有者 → 先重新列出
  `maint/gen/`（同 P3），沒有更新的 head 才拒絕。被引用的 metadata item 照持有者的順序取回驗證（mpack 主體
  不行試 `.r1`），走進它引用的 ID（每個 ID 一次，明確的堆疊）；內容正確但結構不合法的 item 也拒絕（不知道它
  引用什麼）。某個持有者的副本驗證失敗（解不開或 ID 不符）：先整份讀那個 pack，名稱與內容不符（位元損壞）
  而有 parity 就以 parity 在記憶體裡修復，重算 BLAKE3 等於名稱才算數，修好的 bytes 裡這一項解得開就是修得回
  的副本——不從記錄移除、照樣走訪，那個 pack 在 P7c 一律 repack；修不回（沒有 parity、修了名稱仍不符，或名稱
  本來就相符——某一項本來就是假的）才把項目從記錄移除並回報、換下一個持有者（規格錯誤第 17 項的修法 D，
  實作第 6 段）。
- **P6** 一個 ID 的正本＝它可讀、非 tombstone、種類相符的持有者中排序最前的那個。
- **P7** 建 `Map(g+1)`（`age` 是目前的 grace；正在 drain 時是舊的 grace；重建後的 hold 期間不做 c 與 d）：
  - a) condemned 而且是某個 ID 的正本 → live（hold 期間 held 的記錄除外）；
  - b) live、不是任何 ID 的正本、pack 修改時間 + grace ≤ `T(g)` → condemned（`at = g+1`）。比對 pack
    的修改時間只是為了不去標剛寫入、snapshot 還沒 commit 的 pack，安全性不靠它；
  - c) repack：live、是正本、比 grace 老、當正本的項目（sealed bytes）不到 `repack_below`（CLI 的
    `--repack-below`，預設 50%；0 不依比例 repack）的 pack，以及這一輪以 parity 修好的 pack（不論新舊與
    比例）→ 把它當正本的 item 逐一解密驗證（發現不實的項目就從記錄移除並回報；驗證失敗先照 P5 試
    parity），寫進全零 client 前綴下的新 pack（一個舊的對一個新的；mpack 在 `replicas=1` 時連 `.r1`；舊的
    有 parity 就以同樣的片數為新的寫 parity，§13），**讀回驗證**（名稱、trailer、每一項）後記為 `by_prune`
    的 live；舊的（損壞而修好的那個也是）→ condemned，保留全部項目，之後照常兩階段刪除——不覆寫任何東西、
    只有 P9 會刪。讀回驗證失敗：新 pack 記為 unreadable（`since = g+1`），舊的維持 live，回報；寫入失敗只
    回報，舊的維持 live。`prune --read-data`（實作第 7 段，修法 C 的替代）：其他 live、當正本的資料 pack 也
    整份讀、逐項驗證它當正本的 chunk（mpack 不必：P5 走訪時已逐一取回驗證）——有項目驗證失敗就先以
    parity 修復，修得回的 pack 一律 repack；修不回的項目從記錄移除並回報（它若是唯一的持有者，下一輪 P5
    拒絕：資料真的遺失了）。整輪的時間用掉一半就停、回報讀了幾個，剩下的留給下一次，這一輪照樣寫 head；
  - d) condemned（`at = c`、`age`）、不是任何 ID 的正本、`T(c) + age` ≤ 現在的 `T` **而且** `w(c) + age`
    ≤ 現在的 `w`（§11.1 的現在）→ 刪前驗證：它宣稱持有的每個被引用 ID，正本必須是 `by_prune`，或現在在正本那裡解密、重算 ID 成功；
    驗證失敗就把那個項目從正本的記錄移除並回報，這個 pack 留在 condemned；全部通過 → tombstone
    （`at = g+1`）。驗證反覆到沒有變動（移除一個不實的項目可能換出新的正本）。正本的副本驗證失敗時同樣先
    照 P5 試 parity：修得回就不移除，但這一輪不憑修好的副本把這個 condemned 的持有者標成 tombstone——它留
    在 condemned，repack 寫出 `by_prune` 的新 pack 之後，下一輪以那一份驗證；
  - e) unreadable：主體與 `.r1` 都不在 → 移除記錄；否則 `since = s`、`T(s) + grace ≤ T(g)` →
    condemned；
  - f) tombstone、仍在列表裡 → tombstone（`at = g+1`，刷新）；
  - g) tombstone（`at = d`）、主體與 `.r1` 都不在列表、`T(d) + grace ≤ T(g)`、當下沒有 delta 提到它 →
    移除記錄。
- **P8** 寫出有變動的分片；確認仍在整輪時限內，HEAD `maint/gen/<g+1>.r1`（存在就是輸了），再以只在不存在
  時建立的方式寫 head g+1 的主體——**這兩步合起來是柵欄**：`.r1` 已存在、主體得到「已存在」或超過時限就
  停止，什麼都不刪，也不拿這份計畫重試。成功之後才寫 head 的 `.r1`；寫入失敗（不是「已存在」）→ 不刪
  任何東西、停止：主體是唯一的一份，它遺失時那一代就像從來沒有寫過，這一輪的刪除就沒有依據（第二輪
  外部審查 G1：之後主體遺失，每一輪 prune 都以 P3 拒絕；配合偽造的 snapshot，那一代被改寫、資料遺失）。
  主體留著，下一次維護補寫 `.r1`（P1）；改 settings 與重建一樣。先看 `.r1` 是深度隨機測試找到的：
  主體遺失、`.r1` 還在的世代，原本可以被一個更早讀了 head 的寫入者以不同的內容重新建立。改 settings 與
  重建寫 head 時用同一個柵欄。
- **P9**（仍在整輪時限內）刪除：tombstone 而仍在列表的 pack（順序 `.r1`、parity、主體；Object Lock
  拒絕就保留 tombstone 並回報）；所有 pack 都已在 `Map(g+1)` 或已不在列表的 delta；g 與 g+1 都沒引用、
  修改時間 + grace ≤ `T(g)` 的分片（舊世代的，以及輸掉柵欄的 prune 留下的；不需要讀更舊的 head）；
  pack 已不在、修改時間 + grace ≤ `T(g)` 的 parity；解不開、修改時間 + grace ≤ `T(g)` 的 delta；`bad`
  清單裡已不再當成起點的
  snapshot 的主體與 `.r1`（兩份都通不過認證，沒有可還原的內容）。head 永不刪除。這是 prune 唯一會刪
  `snapshots/` 底下物件的情況，它依賴 §5 的 snapshot 密封方式在 v4 這一代內不變。

### 11.4 重建、settings、grace

- **重建**（最新 head 的分片遺失或壞掉）：維護命令。P0 之後以柵欄寫世代 r ＝ `g_lost + 1`，不走訪
  snapshot、不刪任何東西。`g_lost` 是列出來的最大世代；`g_r` 是分片讀得全的最大世代（head 0 沒有分片，
  一定讀得全）。
  - 遺失的世代 `g_r+1..g_lost` 的 head 裡，`w` 的最大減最小 < `a` 時（`a` 是 `g_r` 到 `g_lost` 的 head
    裡最小的 grace，drain 的舊 grace 也算）：沿用 `Map(g_r)`，其中 condemned 的記錄標成 held；列表裡有、
    `Map(g_r)` 裡沒有的 pack 照 P4 收錄。標成 tombstone 要兩個時鐘都過了一個 age，整段遺失的世代在 `w`
    上都不到一個 age，`Map(g_r)` 裡 live 的 pack 與之後的新進者就不可能已被標成 tombstone。只看 `w`，見
    §11.1。原本只比兩端的 `T`（`T(g_lost) < T(g_r+1) + a`），後端時鐘倒退時不成立（外部審查）；之後改成
    `T` 或 `w` 任一個，又被晚寫的 `.r1` 騙過（第二輪外部審查）。
  - 否則：列表裡的每個 pack 都以 condemned（`at = r`，age 是目前的 age，drain 中就是舊的 grace）、held
    收錄，trailer 照 P4 讀（讀不了 → unreadable）；`Map(g_r)` 的 tombstone 保留。
  - head r 記下 `hold: { from: g_r+1, lost: g_lost, grace: g_r 到 g_lost 的 head 裡最大的 grace }`；已有
    hold 時合併（`from` 取小、`lost` 與 `grace` 取大）。hold 在兩個時鐘上都過了才結束：現在的 `T` ≥
    `from..lost` 裡最大的 `T` + grace，**而且**現在的 `w` ≥ 其中最大的 `w` + grace（§11.1 的現在）。原本只比
    `T(lost)`：遺失世代裡的刪除者以自己的時鐘計整輪時限，後端時鐘偏慢時 hold 會提早結束（外部審查）。
  - hold 期間：held 的記錄不復活；不 repack、不標 tombstone（P7c、P7d）。原因：在遺失的世代裡被標成
    tombstone 的 pack，可能正被當時的 prune 刪除。backup 本來就不沿用 condemned 的 pack。
  - held 的記錄消失時直接移除，不論 hold 是否已過：它可能早在重建之前就被當時的 prune 刪掉了（hold 很短
    時，第一次 prune 可能已在 hold 之後；GC 模型找到這個誤報）。被引用的 ID 若因此沒有持有者，P5 會
    拒絕。held 只在記錄被復活時清掉。
- **改 settings**：維護命令，P0、P1 之後直接以柵欄寫 head g+1（分片不變、settings 改變），不刪任何
  東西。這不是 ADR 017 否決的 `settings/<gen>`：只有維護角色能寫 `maint/`，而且受柵欄保護。
- **grace**：調高立刻生效。調低開始 drain：持續一個舊的 grace，在兩個時鐘上都過了才結束（現在的 `T` ≥
  `T(調低的世代)` + 舊 grace，而且現在的 `w` ≥ `w(調低的世代)` + 舊 grace，§11.1 的現在；原本只比 `T`，
  後端時鐘在寫調低的 head 時偏慢就提早結束，外部審查），期間新的 condemned 記錄用舊的 `age`，也不接受其他 grace 變更。
  「現在」是最新 head 的時間（與這一輪 P0 的讀數取小），所以調低之後要有更新的世代（平常是 prune 每一輪寫的）
  drain 才會結束；drain 期間其他 settings 照改、drain 留著，結束之後的下一次改動拿掉它。沒有改變就不寫 head。
  backup 一律用 `min(grace(g_s), grace(g_c))`。
- **移除版本不認得的 pack**（`remove-unknown-packs`，ADR 026 負責人決定 10；實作第 6 段）：維護命令，維護角色
  明確下令。P0、P1 之後以柵欄寫 head g+1，把 map 裡 unknown 的記錄改成 condemned（`at` 是新的世代，`age` 是
  目前的 age，drain 中是舊的 grace），其他不變；沒有 unknown 的記錄就不寫。之後照一般的兩階段刪除（過了 age
  的 prune 標成 tombstone、P9 刪除），不另外多一個刪除者。unknown 的記錄沒有項目，不會是任何 ID 的正本；較新
  的 kist 的 snapshot 若引用它們，prune 在 P5 就停住（版本不認得，或沒有可讀的持有者），走不到刪除——只在沒有
  較新的 kist 使用這個 repo 時下令。

### 11.5 不變式與論證（概要）

**不變式**：任何已 commit、未 forget 的 snapshot 引用的每一個 ID，永遠有一個 kist 沒刪掉的持有者。

1. 只有 P9 會刪 pack，只刪自己這一代標成 tombstone 的，而且只在整輪時限內。
2. tombstone 記錄只在所有可能的刪除者時限都已過去之後才移除（P7g、head 永不刪除讓 `T` 永遠可查）。
3. 用到 pack X 的 backup，一定在任何把 X 標成 tombstone 的世代 d 的 prune 列出 snapshot 之前 commit 並
   可見：X 在 backup 開始時是 live，或是之後才進 map 的新進者（B2、B4），它的 condemned 在 head c 公開
   之後才生效；刪除要
   `T(c) + age ≤ T(d−1)`，而 backup 的經過時間 `< grace − M`（B4）。需要 head 可見的延遲、經過時間的
   低估、snapshot PUT 的延遲三者合計小於 `M`。
4. 刪前驗證讓每個被引用的 ID 在它的 snapshot 被 forget 之前，都保有一個驗證過的持有者。

兩個時鐘都要過期才刪（P7d），後端時鐘偏慢或寫入者時鐘出錯都只會延後刪除。

### 11.6 假設

| 假設 | 狀態 |
| --- | --- |
| LIST 在寫入與刪除之後一致 | AWS S3 有文件；MinIO 只在 xfs/zfs/btrfs；其他後端猜測 |
| `maint/gen/` 的條件建立是原子的 | S3、Azure、R2、Ceph、MinIO 有文件或原始碼；GCS 用 generation-match 0；SFTP 用 hardlink；本機用 link／O_EXCL；P0 每輪實測；rclone 橋接不是（先 stat 再 rename），P0 依種類拒絕 |
| head 的修改時間由後端設定 | 由 P0、P1 與兩個時鐘的規則容錯 |
| 可見延遲＋經過時間低估＋snapshot PUT 延遲 < 1 小時 | 猜測 |
| 備份角色寫不了 `maint/`、`keys/`、`config` | S3 以 bucket policy 強制；本機與 SFTP 與 v3 同樣信任 |
| 單一 PUT 的大小上限夠放一個 pack | AWS S3 的單一 PUT 最多 5 GB（官方文件，2026-10-11 查）；預設的 pack_target 64 MiB 遠低於它。pack_target 接近 4 GiB、chunk 又很小（avg 接近 256 bytes）時，trailer（每項約 60 bytes）可能讓 pack 超過 5 GB，PUT 失敗、backup 報錯，不會無聲出錯。其他 S3 相容服務未查 |
| head 的主體與 `.r1` 不會一起遺失 | 猜測；兩份都遺失時，較早讀過 head 的寫入者可以重新建立那一代（P8）。寫入者在兩次 PUT 之間當掉、之後主體才遺失的情況由 P1 的補寫與 B1 處理，GC 模型有這個故障 |
| 在一個 age 的時間窗內（從一個世代寫成，到從它起算的 age 用完），後端時鐘與寫入者的牆鐘不會都出錯 | 猜測；兩個時鐘的規則（§11.1）保證單一時鐘出錯時只會延後刪除。原本寫「不會同時往同一個方向錯」，沒有排除兩個時鐘在不同時間各錯一次（第二輪外部審查 C1–C3）；「現在」與 P0 的讀數取小之後，這種組合還要那個時鐘從 base head 寫成一直錯到這一輪的 P0 |

### 11.7 對照 v3 的教訓

| 教訓 | v4 |
| --- | --- |
| repack 一步拿掉舊 pack、重疊 prune 留下幽靈、刪 pack 前沒重寫 index、commit 只逐 pack 檢查 | 不可能：舊 pack 保留全部項目並走兩階段；一代只有一個寫入者；記錄只來自 prune 讀過的 trailer |
| 樹的 HEAD→DELETE 視窗、touch 沒刷新、同秒方向、SFTP 零 mtime、帶舊標記的新樹 | 不可能：沒有樹物件、沒有 touch；備份角色寫的物件的時間不承擔安全性 |
| backup 跑超過 grace | 以 B4 的單一規則擋住 |
| client 時鐘、prune 主機時鐘、秒級精度 | 只比 head 的時間；兩個時鐘都要過期；比較都在小時級 |
| config 被竄改造成快取認錯 repo | 槽的 AAD 綁整份 config |
| 備份主機寫的 index 能影響 prune 刪什麼 | 不可能：刪除只看 prune 讀過、驗證過的資料 |

## 12. 後端契約

- 必要：只在不存在時建立的單一 PUT、range GET、寫入與刪除之後一致的 LIST（修改時間至少到秒）、HEAD、
  DELETE（只給維護角色；刪除不存在的物件算成功，各後端一致）。`maint/gen/` 從上次看過的世代起以
  start-after 列出（S3 送 `start-after`；本機與 SFTP 列出整個 prefix 再過濾，結果相同）。
- 不需要：覆寫、分段上傳、條件刪除。
- 寬鬆後端（`rclone://`、條件寫入沒有文件的 B2 與 Wasabi）：backup 可以用（pack、delta 以內容命名，
  snapshot 以 client 分前綴、本機鎖保證同一個 client 一次一個）；prune、重建、改 settings 與
  `remove-unknown-packs`（都要 P0 與柵欄）與 `check --repair`（以只在不存在時建立的寫入修復）不能用；forget 與
  `key add`／`key list`／`key remove` 可以用：它們不建立由柵欄保護的物件、不需要 P0（實作第 6 段；原文把 forget
  也擋掉了，ADR 026 規格錯誤第 18 項）。
  `rclone://` 依後端的種類拒絕（P0）：橋接的條件建立是先 stat 再 rename，兩個寫入者同時通過 stat 時後寫的
  蓋掉先寫的，但依序建立兩次照樣得到「已存在」，P0 的雙重建立看不出來（實作第 3 段在真的 rclone 上
  重現）。S3 相容服務靠 P0 的雙重建立：忽略 `If-None-Match` 的服務會讓第二次建立成功。
- 寫入的結果不一定確定：S3 回 5xx 時 client 會重試，第一次其實已寫入的話重試得到「已存在」；逾時的建立
  不重試，錯誤不代表沒寫入。所以內容命名的 key（pack、mpack、delta、分片）得到「已存在」就是成功；head 的
  柵欄得到「已存在」或錯誤就停止、不刪（P8；如果其實寫進去了，那一代照樣有效，下一次維護補寫 `.r1`）；
  snapshot 主體寫入失敗時 commit 與否不明，backup 以錯誤結束；得到「已存在」時照 §9 先讀回比對，bytes 相同
  就是自己寫進去的，不同才以時間戳 +1 奈秒重試（第 3 段寫這一條時原本只說 +1 奈秒，自己的寫入會變成兩份
  內容相同的 snapshot；第 4 段改成先比對）。

## 13. 冗餘

- head 一律寫 `.r1`（不論 replicas，head 只有幾 KiB）：柵欄靠它擋住主體遺失後被重新建立（P8）。主體
  在、`.r1` 不在的 head：寫入者還在兩次 PUT 之間、已經當掉，或 `.r1` 的 PUT 失敗而停止（P8）；維護角色
  讀到時補寫（P1），backup 不拿它決定沿用（B1）。
- `replicas=1`（本機預設）：mpack、snapshot、map 分片也寫 `.r1`。mpack 與 head 先寫主體再寫 `.r1`
  （head 的主體是柵欄）；snapshot 先寫 `.r1` 再寫主體（主體是 commit）。資料 pack 不寫副本。
- parity 選配，資料 pack 與 mpack 都可用；repack 為新 pack 寫 parity，片數與舊 pack 的 parity 相同（舊的沒有
  就不寫；settings 沒有 parity 的欄位，這是實作上的選擇）。parity 與它對應的 pack 同名（§1）。
  backup 以選項決定片數（0 不寫），寫失敗只警告：pack 本身已安全，少的只是冗餘。
- parity 的排版（沿用 v3，`v` 是 4）：明文 CBOR `{v, k, m, pack_size, shard_len, hashes, parity}`。整個寫進
  repo 的 pack bytes 切成 `k` ＝ 16 片、每片 `shard_len` ＝ ⌈`pack_size` / 16⌉ bytes（最後一片補零），以
  Reed-Solomon（GF(2^8)，`reed-solomon-erasure` 的 Vandermonde 矩陣，即 Backblaze JavaReedSolomon／
  klauspost/reedsolomon 的矩陣，曾以固定向量驗證逐 byte 相容；與 v3 相同）算出 `m` 片同位，`m` 在 1 到 8；`hashes` 是 16 + `m` 片各自的
  BLAKE3（資料片在前），`parity` 是 `m` 個 byte string。讀取端拒絕：`v` 不是 4、`k` 不是 16、`m` 越界、
  `shard_len` 是 0 或超過 64 MiB、與 `pack_size` 不符、片數或長度不對——都在依 header 配置記憶體之前檢查。
  明文的理由：同位是密文的線性組合，不洩漏內容；偽造或損壞的 parity 最多讓修復失敗，修復的結果以重算 BLAKE3
  等於 pack 的名稱證明。向量 `golden-v4/parity.cbor`，與 v3 凍結的 `parity-golden.txt` 只差 `v` 那一個 byte。
  check 讀它、證明修得回（§13.1）。
- 刪除順序 `.r1` → parity → 主體。
- `.r1` 在而主體不在：mpack、head、分片是主體遺失（它們先寫主體），讀取端用 `.r1`，`check` 回報，
  `check --repair` 從 `.r1` 重建主體（主體不存在，所以是建立，不是覆寫）。snapshot 先寫 `.r1`，所以也
  可能是 commit 前中斷：prune 照樣當成起點（P5）、`check` 回報，不自動補主體，由使用者 `forget` 或
  決定補回。
- 在的、但損壞的物件（名稱與內容不符，或解不開）：v4 不覆寫，所以 `check` 只證明修得回——parity 修出的
  bytes 重算 BLAKE3 等於名稱，或 `.r1` 完好——不寫回。修回的路徑是 prune（ADR 026 規格錯誤第 17 項的修法
  D，實作第 6 段）：P5、P7c、P7d 讀到驗證失敗的副本時先以 parity 在記憶體裡修復，修好的 pack 一律 repack
  寫進新的 pack，損壞的那個走兩階段刪除（§11.3）。restore 與 mount 不用 parity：損壞的副本換下一個持有者，
  全部失敗就報錯（§10），直到 prune 修好它。平常的 prune 只讀到被引用的 metadata item、repack 候選與刪前
  驗證的正本；稠密、live 的資料 pack 裡損壞的 chunk 要 `prune --read-data`（§11.3 P7c：每個 live 的資料
  pack 整份讀、逐項驗證、修得回的 repack）才修得回，所以 `check --read-data` 回報 `repairable` 之後接著跑
  它（restore 自己不用 parity：修法 C 沒做，ADR 026 第七段）。

### 13.1 check

一律讀後端，不用本地快取（快取裡的那一份證明不了後端還在）。不重新載入（§10）。

**不讀資料**（預設）：
- 每一代的 head：主體與 `.r1` 都在、解得開、bytes 相同；`parent` 等於前一代的 BLAKE3；世代從 0 起沒有空洞
  （head 永不刪除）。
- 讀取端用的那一代（§10）：比它新卻被跳過的世代是錯誤；它的分片都在、名稱相符，`replicas=1` 時 `.r1` 也是。
- 記錄與列表（P3）：live 與 condemned 的 pack 都在、大小等於記錄；`by_prune` 的記錄在全零前綴下；mpack 的
  主體不在而 `.r1` 在、或 `replicas=1` 而 `.r1` 不在，照下面的修復處理。當下 delta 提到、map 還沒收錄的
  mpack 的兩份也照這一條檢查與修復（第一次 prune 之前每個 mpack 都只在 delta 裡；實作原本漏了，第 6 段換
  CLI 時的測試發現）；delta 提到卻完全不在列表裡的 pack 仍只是警告（下一句）。警告（不是錯誤）：unreadable 與
  unknown 的記錄、列表裡有而 map 與 delta 都沒提到的 pack（中斷的 backup 留下的，下一次 prune 收錄；mpack 的
  主體與 `.r1` 只回報一次）、解不開的 delta、delta 提到卻不在列表裡的 pack、對應的 pack 已不在的 parity、
  只有 `.r1` 的 snapshot。
- 每一個 snapshot（主體，與只有 `.r1` 的，P5 都當成起點）：讀得出來（主體不行讀 `.r1`，§9），走過它引用的
  每一個 metadata item 與 chunk（每個 ID 一次）：照 P5，每一個都要有可讀、非 tombstone、種類相符的持有者
  （只剩 tombstone 是錯誤：prune 會刪它）；目錄的每一段照 §7 檢查；檔案的 chunk 清單（ChunkList、
  ChunkIndex）與大小照 §7（chunk 的長度取自持有者記錄的 `r`）；巢狀超過實作上限 256 層回報、不深入。讀
  metadata item 時驗證失敗的副本、只剩 tombstone 讀得到的項目都是錯誤（讀取端會換下一份，check 照樣回報）。

**`--read-data`**：map（live、condemned、tombstone）與 delta 提到的每一個 pack 與 mpack 整份下載：名稱等於
BLAKE3、trailer 解得開而合乎 §6、每一項解得開而 ID 相符；**記錄 ⊆ trailer**：記錄（或 delta）列出的每一項都在
trailer 裡、位移與長度相同，驗證不過的項目不能還在記錄裡（P7c、P7d 只會從記錄移除項目，所以反過來不成立）；
mpack 的 `.r1` 與主體 bytes 相同；有 parity 的修得回自己。損壞的物件有驗證過的完好 bytes（parity 修得回、或
`.r1` 完好）就回報「修得回」，不寫回（§13）。

**`--repair`**（維護角色；隱含 `--read-data`；寬鬆後端上拒絕，§12；只建立物件，不做 P0）只做只在不存在時
建立的修復，bytes 一律先驗證（以內容命名的重算名稱，head 與 snapshot 解得開）：
1. head 的主體不在、`.r1` 完好 → 從 `.r1` 建立主體（柵欄先看 `.r1`，所以不會與寫入者衝突，P8）；
2. head 的 `.r1` 不在 → 從主體建立（同 P1 的補寫，任何一代都做）；
3. 分片的主體不在、`.r1` 完好 → 建立主體；`replicas=1` 而 `.r1` 不在 → 建立 `.r1`；
4. mpack 同上（主體或 `.r1`）；
5. `replicas=1` 而 snapshot 的 `.r1` 不在 → 從主體建立；
6. snapshot 的主體不補（§13）。
「已存在」是別人先建了，算成功。在的、但損壞的物件不動。

不檢查：config 與金鑰槽（開啟 repo 時以槽的 AAD 驗證整份 config，只讀一個槽，§3）；較舊世代的分片（P9 會刪
兩代都不再引用的分片）。

## 14. 版本

- `config.v` ＝ 4 代表這一代格式，config 不可變。一開始每個結構的 `v` 與 pack magic 都是 4；同一代內
  各物件的 `v` 可以往上加，讀取端遇到不認得的版本就拒絕，prune 不刪看不懂的東西。
- 同一代內改格式時升 head 的 `v`：較舊的 backup 與 prune 讀不懂最大世代就停止（§10）。
- 派生字串、AAD 常數、magic 的改動屬於下一代格式，不在 v4 內做。

## 15. 向量與 fixture

每條規範性規則至少一個資料檔向量或測試：金鑰推導與槽；CBOR 十條；切段規則與 Dir 驗證；Root 的種類
規則；chunk 清單的切段與 ChunkIndex 的 `size`；pack 的種類、上限與解壓上限；head、分片、記錄的編碼；parity 的排版；GC 的情境（重疊的 prune、停住的 prune、
重建、grace 調低再調高、晚到的 delta、沿用被 forget 的 parent 子樹、不實的 trailer、讀不了的物件）；讀取端
（持有者的順序、驗證失敗換下一份、重新載入與限流、退回較舊的世代、restore 依 `Root.t` 的落點）；§13.1 的
每一條 check 規則與每一種修復。
v3 的 fixture 已移除（實作第 6 段）；v4 的在第 7 段凍結。

## 16. 設計決定 × 教訓

| 決定 | 教訓或需求 |
| --- | --- |
| 世代柵欄 | 重疊的 prune 與 index 改寫是兩類資料遺失的來源；柵欄讓一代只有一個寫入者 |
| 刪除只看 prune 驗證過的資料 | 備份角色不能借 prune 之手刪資料 |
| 只比 head 的時間、兩個時鐘都要過期 | 時鐘問題只能延後刪除，不能提早 |
| 不覆寫任何物件 | 讓 create-only 的 bucket policy 不必開例外 |
| 沿用前先驗證別台的副本 | 還原可靠性排在效能之前 |
| metadata 進 pack、依內容切段 | 每個目錄一個物件既不安全又貴；大目錄改一個檔不該重寫整條鏈 |
| root 記種類 | 記下事實就不用猜（V19） |
