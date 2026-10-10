# kist repo 格式（v4）

> **狀態：設計已確認（2026-10-10，見 ADR 026），實作中。** 實作第 2 段（kist-format）完成時，本文件
> 取代 `docs/format.md`（v3）成為唯一權威副本；在那之前程式碼仍是 v3，`docs/format.md` 仍描述現有程式碼。
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
| `keys/<hex16>` | init、維護 | 金鑰槽（§3） |
| `packs/<c>/<hex>` | backup（client c）、prune | 資料 pack：檔案的 chunk |
| `mpacks/<c>/<hex>` | backup（client c）、prune | metadata pack：metadata item（§7） |
| `parity/<c>/<hex>` | backup、prune | 選配的 Reed-Solomon 同位，對應同名的 pack 或 mpack |
| `deltas/<c>/<hex>` | backup（client c） | 提示：這次 backup 寫的 pack 有哪些項目（§10） |
| `snapshots/<c>/<ts>` | backup（client c） | commit 記錄（§9） |
| `maint/gen/<g>` | prune、init | 世代 head（§10、§11），永不刪除 |
| `maint/map/<hex>` | prune | map 分片（§10） |
| `maint/probe-<hex>` | prune | 時鐘與條件寫入探針，用完即刪 |

- `<c>` 是 client id 的 32 位小寫 hex。維護角色（prune 的 repack 輸出）用全零的 client id。
- `<hex>` 是物件 bytes 的 BLAKE3（無 key）小寫 hex；`<g>` 是世代號碼的 16 位小寫 hex（字典序＝數值序）；
  `<ts>` 是 `YYYYMMDDTHHMMSSnnnnnnnnnZ`。
- `replicas=1` 時，mpack、snapshot、head、map 分片各有一份 `<key>.r1`，bytes 與主體相同（§13）。
- 讀取端必須驗證名稱：以 BLAKE3 命名的物件重算 hash；pack 內每一項重算自己的 ID（§2）；head 與
  snapshot 以 AAD 綁定 key 路徑，delta 綁定所在前綴的 client（§5）。

建議的權限（S3）：每台備份主機的憑證只能在自己的 `packs/<c>/`、`mpacks/<c>/`、`parity/<c>/`、
`deltas/<c>/`、`snapshots/<c>/` 底下建立物件，可以讀全部；`config`、`keys/`、`maint/` 與全零 client
的前綴只給 init 與維護角色；刪除只給維護角色。bucket policy 以 `s3:if-none-match` 強制條件寫入。

## 2. 識別碼

- chunk ID ＝ keyed BLAKE3(`K_chunk_id`, chunk 明文)，32 bytes。
- meta ID ＝ keyed BLAKE3(`K_meta_id`, metadata item 的規範 CBOR，壓縮之前)，32 bytes。兩把 key
  不同，一個 ID 不會同時是資料與 metadata。
- pack、mpack、delta、map 分片、parity 的名稱 ＝ BLAKE3(寫進 repo 的 bytes)。
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
- 換密碼：`key add` 建一個新槽；`key remove`（維護角色）只在同一個程序內另一個槽成功開啟、而且仍在列表
  裡時才刪，最後一個槽刪不掉。
- 代價：KDF 參數在 init 時決定，之後不能調高；要更強的參數就開新 repo。
- 本地 index 快取的目錄名稱從 `K_cache` 推導，換 repo 位置就不共用（同 v3）。

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
| `Hold` | lost, grace |
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
  （壓縮前）。一致性檢查：連續排列、完整覆蓋、沒有重複 ID、長度不超過上限。
- `alg`：0 原文、1 zstd level 3；壓縮省不到 1/16 就存原文。解壓上限：chunk 是 `chunker.max`，metadata
  item 是 2 MiB——**與 trailer 宣稱的 `r` 無關**，先限制輸出再比對長度。
- 名稱＝BLAKE3(整檔)。寫滿 `settings.pack_target`（資料，預設 64 MiB）或 `settings.mpack_target`
  （metadata，預設 8 MiB）時寫出，backup 結束時寫出剩下的。

## 7. Metadata items

每個 item 是 `alg byte ‖ CBOR`，以 `K_meta` 封、AAD 是自己的 meta ID。`k` 標示種類，在受認證的明文裡。

- **Seg**（`k=0`）`{v, k, entries:[Entry]}`：一個目錄的一段，entries 依名稱 bytes 升冪、不重複。
- **Dir**（`k=1`）`{v, k, level, segs:[{first, id}]}`：目錄有多段時的索引。`level=1` 時 `id` 指向 Seg，
  `level=2` 或 `3` 時指向下一層 Dir。
- **ChunkList**（`k=2`）`{v, k, chunks:[chunk ID]}`：檔案的 chunk 超過 256 個時的清單，或清單的一段。
- **ChunkIndex**（`k=4`）`{v, k, level, parts:[{size, id}]}`：chunk 清單有多段時的索引。`size` 是那一段
  涵蓋的明文 bytes（chunk 明文長度的合計），讓 mount 隨機讀取時可以跳過不需要的段。`level=1` 時 `id`
  指向 ChunkList，`level=2` 或 `3` 時指向下一層 ChunkIndex。
- **RootSeg**（`k=3`）`{v, k, roots:[Root]}`：snapshot 的 roots 的一段，依 `path` bytes 升冪、不重複；
  多段時同樣以 Dir 索引。

**切段規則**（目錄、root 清單、chunk 清單都用）：依順序加入；目前這一段已有至少 1024 項之後，遇到某一
項的 keyed BLAKE3(`K_cut`, 名稱) 低 12 位元全為 0，就在這一項之後切；加入下一項會讓這一段的規範 CBOR
超過 2 MiB 時，也在它之前切。chunk 清單沒有名稱，以 chunk ID 的 32 bytes 代替，順序就是檔案內容的順序；
內容中間改了一段，只有附近的段會變。每個 entry 的 xattrs 合計上限 256 KiB。一項超過任一上限時寫不
進去：backup 把它記成這個項目的錯誤、不寫入，結束碼 3。Dir 與 ChunkIndex 本身同樣不超過 2 MiB，最多
三層；一層 ChunkIndex 平均涵蓋約 400 TiB（以平均 2 MiB 的 chunk 計），檔案大小沒有實際上限。

**讀取端驗證**（任何一條不成立都是腐壞，大聲失敗）：Dir 的 `first` 嚴格遞增；每一段非空，而且第一項
的名稱等於它在 Dir 裡的 `first`；段內依名稱升冪、不重複；`level` 與指向的種類相符；`ct=0` 的 chunks
不超過 256 個；ChunkIndex 的每一段非空，每一段 chunk 明文長度的合計等於它的 `size`，全部 `size` 的
合計等於 Entry 的 `size`；上面的大小上限。

## 8. Entry（metadata 聯集）

與 v3 相同（欄位、per-kind 規則、快速路徑合約、硬連結範圍、深度上限），只差 `tree` 的意義：

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

## 9. Snapshot

```
Snapshot { v: 4, roots: meta ID（RootSeg 或 Dir）, time: i64 奈秒（backup 開始）, host: text,
           user: text（省略若空）, client: bytes(16), parent: text | null, stats }
Root     { path: byte string, t: u8, tree: meta ID（t=1）, entry: Entry（t=0 或 2） }
```

- `Root.t` 是來源本身的類型（0 檔案、1 目錄、2 符號連結），必填。`t=1` 必須有 `tree`、不得有 `entry`；
  `t=0` 或 `2` 必須有 `entry`、不得有 `tree`，`entry.t` 等於 `Root.t`，`entry.n` 等於定位的最後一段。
- 定位切段：有 `scheme://` 就去掉，以 `/` 切，空段與 `.` 丟掉，`..` 映射成 `__parent__`，段裡有 NUL 是
  腐壞。`/srv/data` 切成 `srv`、`data`；`s3://bucket/prefix` 切成 `bucket`、`prefix`；`/` 沒有段。
- restore 映射：`t=1` 的內容放在 `<target>/<所有段>`（沒有段就直接放在 `<target>`）；`t=0` 或 `2` 放在
  `<target>/<去掉最後一段>/<名稱>`。`t≠1` 時定位至少要有一段。不再從內容猜。
- `parent`：同一個 client、roots 的路徑與 `t` 相同的上一個 snapshot 的 key，只用於加速。
- stats：`files`、`symlinks` 按名稱計；`dirs` 是目錄節點數，roots 不算；`bytes` 是內容總和，`(dev, ino)`
  只算一次。過程計數不在格式裡。
- commit：`replicas=1` 時先寫 `.r1`，再寫主體；主體出現＝commit。同名時時間戳 +1 奈秒重試。
- forget：不刪任何 client 在同一組 roots 上最新的那一份 snapshot（那是下一次 backup 的 parent）；刪除時先
  刪 `.r1` 再刪主體。強制刪除那一份等於讓這個 client 的這組 roots 退役，只能在它沒有進行中的 backup 時
  做：backup 沿用 parent 的子樹時不查子樹裡每個 item 的持有者，靠的是 parent 一直存在（B4 的 HEAD 只能
  縮小、不能消除這個空窗）。

## 10. Index：map 與 delta

**Head** `maint/gen/<g>`：

```
Head { v: 4, g, w: 寫入者的牆鐘時間（i64 奈秒）, parent: BLAKE3(head g−1 的 bytes)（g=0 時省略）,
       shards: [256 個 hash，空分片為 null], settings: { grace, replicas, pack_target, mpack_target },
       drain: { from: g, old_grace } | 省略, bad: [{ key, first: g }] | 省略,
       hold: { lost: g, grace: 秒 } | 省略 }
```

- `T(g)` ＝ head g 的後端修改時間，截到秒。head 永不刪除，所以每個世代的 `T` 與 `w` 永遠查得到。
- init 寫 head 0：空的 map、預設 settings（grace 72 小時；replicas 本機 1、S3 與 SFTP 0，同 v3；
  pack_target 64 MiB；mpack_target 8 MiB）。改 settings 是一個不刪任何東西的世代（§11.4）。

**Map 分片** `maint/map/<hex>`：`Shard {v, index, records}`；分片 i 放名稱第一個 byte 等於 i 的所有 pack
與 mpack 的記錄。

```
Record { id, client, kind, size, entries: [{i, o, l, r}], by_prune: bool, since: g,
         status: { s: 0 live | 1 condemned | 2 tombstone | 3 unreadable, at: g, age: 秒 },
         held: bool（只在重建後的 hold 期間有意義，§11.4） }
```

- `entries` 取自 prune 自己讀到的 trailer；之後唯一的改變是移除被證明不實的項目（P7c、P7d）。
- `by_prune`：prune 自己寫出、讀回驗證過的 pack。
- `since`：這筆記錄進入 map 的世代。
- **排序**（選正本、讀取順序）：live 先於 condemned，其次 `since` 小的先，其次名稱小的先。新進的 pack
  不會取代既有的持有者成為正本。

**Delta** `deltas/<c>/<hex>`：`Delta {v, client, packs: [{id, kind, size, entries}]}`，backup 寫，只是
提示：讓別台在 prune 收錄之前就能去重與還原新 pack 裡的東西。prune 從不依 delta 做決定。delta 只能描述
同一個 client 前綴下的 pack（`deltas/<c>/` 只列 `packs/<c>/` 與 `mpacks/<c>/`），讀取端略過不符的項目
並警告：否則一台主機可以替別台的 pack 宣稱它沒有的內容。

**讀取端**（backup、restore、mount、check）：
- 列 `maint/gen/`（從上次看過的世代 start-after），取**列出來的最大世代**，讀主體、不行讀 `.r1`。讀不了
  或版本不認得：backup 與 prune 停止；restore 與 mount 可以退回較舊的世代並警告。
- 分片依 hash 快取。delta 只用當下列表裡有的；解不開的 delta 跳過並警告。map 沒提到的 pack 才看 delta。
- 一個 ID 的持有者依排序逐一嘗試；某一份驗證失敗（解不開或 ID 不符）就試下一份；tombstone 的持有者
  排最後、只在其他都失敗時才用並警告；全部失敗 → 重新載入一次 → 仍失敗就報錯。

## 11. GC 與並行（世代柵欄）

### 11.1 符號

- `T(g)`、`w(g)`：head g 的後端時間與寫入者時間。`grace` 取自 head 的 settings（預設 72 小時，至少
  2 小時）。`M` ＝ 1 小時。
- backup 的經過時間 `E` ＝ max(牆鐘經過, 含休眠的單調時鐘經過)。Linux 用 `CLOCK_BOOTTIME`；其他平台的
  對應時鐘未驗證（UNVERIFIED）。
- prune 的整輪（P0 到 P9 結束）必須在 `grace − M` 內完成，以同樣的時鐘計；超過就停止，不再寫 head、
  不再刪任何東西。

### 11.2 backup

- **B1** 記下開始時間（牆鐘與單調時鐘）。讀列出來的最大世代 `g_s`（必須讀得了、版本認得，否則停止），
  載入它的分片；列 `deltas/` 並讀新的；找 parent（自己最新、roots 相同的 snapshot）。
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
  `E < min(grace(g_s), grace(g_c)) − M`。去重用到的每個 pack：在 `Map(g_c)` 裡 → 不是 tombstone 或
  unreadable，而且沿用的 ID 都還在它的記錄裡（`g_c = g_s` 時 B2 已經查過）；不在 `Map(g_c)` 裡（來自
  delta 的）→ HEAD 它，再讀它的 trailer 確認沿用的 ID 在裡面。自己這次寫的 pack 不能是 tombstone。沿用了
  parent 的子樹時 HEAD parent，不在就停止。任何一項不成立：不 commit，以錯誤結束。
- **B5** 再檢查一次 `E`；`replicas=1` 時寫 snapshot 的 `.r1`；再檢查一次 `E`；寫 snapshot 主體。commit。

### 11.3 prune

只在通過條件寫入探針的後端上執行。同一台主機以檔案鎖保證一次一個。

- **P0** 建立 `maint/probe-<亂數>`，再以同名建立一次，必須得到「已存在」；HEAD 它，後端時間與本機牆鐘
  相差超過 5 分鐘 → 拒絕。結束時刪掉探針。
- **P1** 讀列出來的最大世代 g；版本不認得、讀不了、或沒有修改時間 → 拒絕；`T(g)` 比本機時鐘晚超過 `M`
  → 拒絕。較舊的 head 之間的時間異常只警告。
- **P2** 依序列出：`snapshots/`、`deltas/`、`packs/` 與 `mpacks/`（含 `.r1`）、`parity/`、`maint/map/`。
  先列的物件寫在它所依賴的物件之後，所以那些物件一定出現在後列的清單裡。
- **P3** live 或 condemned 的記錄，主體與 `.r1` 都不在 → 拒絕（資料遺失，請執行 check 與修復）；只缺
  主體、`.r1` 在 → 回報、繼續。例外：重建後的 hold 期間，held 的記錄消失時直接移除（§11.4）。
- **P4** 列表裡有、map 裡沒有的 pack 是新進者：讀它的 trailer，讀得了 → live（`since = g+1`）；讀不了
  （解不開、排版不對、種類與命名空間不符）→ unreadable（`since = g+1`），沒人會用它；解得開但版本不認得
  → 拒絕（prune 不刪看不懂的東西）。
- **P5** 走訪每一個 snapshot，收集被引用的 ID。只有 `.r1`、沒有主體的 snapshot 一樣當成起點並回報
  （可能是 commit 前中斷，也可能是主體遺失）。主體解不開 → 試 `.r1`；兩份都解不開：在 head 的 `bad`
  清單裡已滿一個 grace（`T(first) + grace ≤ T(g)`）→ 不再當成起點、回報；否則這一輪不改任何記錄、不刪
  任何東西，回報後停止——它還不在 `bad` 裡時，先以柵欄寫 head g+1 把它記進去（`first = g+1`，分片
  不變）。維護角色可以直接 `forget <key>` 立刻解除，不需要解密。解得開但版本不認得 → 拒絕。列表之後
  兩份都消失 → 視為已 forget，略過。某個 ID 沒有任何可讀、非 tombstone、種類相符的持有者 → 拒絕。
- **P6** 一個 ID 的正本＝它可讀、非 tombstone、種類相符的持有者中排序最前的那個。
- **P7** 建 `Map(g+1)`（`age` 是目前的 grace；正在 drain 時是舊的 grace；重建後的 hold 期間不做 c 與 d）：
  - a) condemned 而且是某個 ID 的正本 → live（hold 期間 held 的記錄除外）；
  - b) live、不是任何 ID 的正本、pack 修改時間 + grace ≤ `T(g)` → condemned（`at = g+1`）。比對 pack
    的修改時間只是為了不去標剛寫入、snapshot 還沒 commit 的 pack，安全性不靠它；
  - c) repack：live、是正本、比 grace 老、有效比例低 → 把它當正本的 item 逐一解密驗證（發現不實的項目
    就從記錄移除並回報），寫進全零 client 前綴下的新 pack，**讀回驗證**後記為 `by_prune` 的 live；舊的
    → condemned，保留全部項目。讀回驗證失敗：新 pack 記為 unreadable（`since = g+1`），舊的維持 live，
    回報；
  - d) condemned（`at = c`、`age`）、不是任何 ID 的正本、`T(c) + age ≤ T(g)` **而且** `w(c) + age ≤ w(g)`
    → 刪前驗證：它宣稱持有的每個被引用 ID，正本必須是 `by_prune`，或現在在正本那裡解密、重算 ID 成功；
    驗證失敗就把那個項目從正本的記錄移除並回報，這個 pack 留在 condemned；全部通過 → tombstone
    （`at = g+1`）；
  - e) unreadable：主體與 `.r1` 都不在 → 移除記錄；否則 `since = s`、`T(s) + grace ≤ T(g)` →
    condemned；
  - f) tombstone、仍在列表裡 → tombstone（`at = g+1`，刷新）；
  - g) tombstone（`at = d`）、主體與 `.r1` 都不在列表、`T(d) + grace ≤ T(g)`、當下沒有 delta 提到它 →
    移除記錄。
- **P8** 寫出有變動的分片；確認仍在整輪時限內，再以只在不存在時建立的方式寫 head g+1 的主體——**這一步是
  柵欄**：得到「已存在」或超過時限就停止，什麼都不刪，也不拿這份計畫重試。成功之後才寫 head 的 `.r1`。
- **P9**（仍在整輪時限內）刪除：tombstone 而仍在列表的 pack（順序 `.r1`、parity、主體；Object Lock
  拒絕就保留 tombstone 並回報）；所有 pack 都已在 `Map(g+1)` 或已不在列表的 delta；g 與 g+1 都沒引用、
  修改時間 + grace ≤ `T(g)` 的分片（舊世代的，以及輸掉柵欄的 prune 留下的；不需要讀更舊的 head）；
  pack 已不在、修改時間 + grace ≤ `T(g)` 的 parity；`bad` 清單裡已不再當成起點的
  snapshot 的主體與 `.r1`（兩份都通不過認證，沒有可還原的內容）。head 永不刪除。這是 prune 唯一會刪
  `snapshots/` 底下物件的情況，它依賴 §5 的 snapshot 密封方式在 v4 這一代內不變。

### 11.4 重建、settings、grace

- **重建**（最新 head 的分片遺失或壞掉）：維護命令。P0 之後以柵欄寫世代 r ＝ `g_lost + 1`，不走訪
  snapshot、不刪任何東西。`g_lost` 是列出來的最大世代；`g_r` 是分片讀得全的最大世代（head 0 沒有分片，
  一定讀得全）。
  - `T(g_lost) < T(g_r + 1) + a` 時（`a` 是 `g_r+1` 到 `g_lost` 的 head 裡最小的 grace，drain 的舊 grace
    也算）：沿用 `Map(g_r)`，其中 condemned 的記錄標成 held；列表裡有、`Map(g_r)` 裡沒有的 pack 照 P4
    收錄。這段時間不夠讓 `Map(g_r)` 裡 live 的 pack 或之後的新進者被標成 tombstone（標成 condemned 之後
    還要再過一個 age），所以它們不會正被刪除。
  - 否則：列表裡的每個 pack 都以 condemned（`at = r`）、held 收錄，trailer 照 P4 讀（讀不了 →
    unreadable）；`Map(g_r)` 的 tombstone 保留。
  - head r 記下 `hold: { lost: g_lost, grace: g_r 到 g_lost 的 head 裡最大的 grace }`；已有 hold 時取較晚
    結束的那個。之後的 prune 在 `T(g) < T(lost) + grace` 時（g 是它的基礎世代）：held 的記錄不復活，
    主體與 `.r1` 都不在就移除記錄；不 repack、不標 tombstone（P7c、P7d）。原因：在遺失的世代裡被標成
    tombstone 的 pack，可能正被當時的 prune 刪除；那些 prune 的整輪時限在 `T(lost) + grace` 之前結束。
    backup 本來就不沿用 condemned 的 pack。過了這個時間，prune 清掉 held 與 hold，之後照一般規則。
- **改 settings**：維護命令，P0、P1 之後直接以柵欄寫 head g+1（分片不變、settings 改變），不刪任何
  東西。這不是 ADR 017 否決的 `settings/<gen>`：只有維護角色能寫 `maint/`，而且受柵欄保護。
- **grace**：調高立刻生效。調低開始 drain：持續一個舊的 grace（直到 `T(g) ≥ T(調低的世代) + 舊 grace`），
  期間新的 condemned 記錄用舊的 `age`，也不接受其他 grace 變更。backup 一律用 `min(grace(g_s), grace(g_c))`。

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
| `maint/gen/` 的條件建立是原子的 | S3、Azure、R2、Ceph、MinIO 有文件或原始碼；GCS 用 generation-match 0；SFTP 用 hardlink；本機用 link／O_EXCL；P0 每輪實測 |
| head 的修改時間由後端設定 | 由 P0、P1 與兩個時鐘的規則容錯 |
| 可見延遲＋經過時間低估＋snapshot PUT 延遲 < 1 小時 | 猜測 |
| 備份角色寫不了 `maint/`、`keys/`、`config` | S3 以 bucket policy 強制；本機與 SFTP 與 v3 同樣信任 |
| 單一 PUT 的大小上限夠放一個 pack | 未查證（UNVERIFIED） |

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
  DELETE（只給維護角色）。
- 不需要：覆寫、分段上傳、條件刪除。
- 寬鬆後端（`rclone://`、條件寫入沒有文件的 B2 與 Wasabi）：backup 可以用（pack、delta 以內容命名，
  snapshot 以 client 分前綴、本機鎖保證同一個 client 一次一個）；prune 不能用（P0 不通過就拒絕）。

## 13. 冗餘

- `replicas=1`（本機預設）：mpack、snapshot、head、map 分片寫 `.r1`。mpack 與 head 先寫主體再寫 `.r1`
  （head 的主體是柵欄）；snapshot 先寫 `.r1` 再寫主體（主體是 commit）。資料 pack 不寫副本。
- parity 選配，資料 pack 與 mpack 都可用；repack 為新 pack 寫 parity。
- 刪除順序 `.r1` → parity → 主體。
- `.r1` 在而主體不在：mpack、head、分片是主體遺失（它們先寫主體），讀取端用 `.r1`，`check` 回報，
  `check --repair` 從 `.r1` 重建主體（主體不存在，所以是建立，不是覆寫）。snapshot 先寫 `.r1`，所以也
  可能是 commit 前中斷：prune 照樣當成起點（P5）、`check` 回報，不自動補主體，由使用者 `forget` 或
  決定補回。

## 14. 版本

- `config.v` ＝ 4 代表這一代格式，config 不可變。一開始每個結構的 `v` 與 pack magic 都是 4；同一代內
  各物件的 `v` 可以往上加，讀取端遇到不認得的版本就拒絕，prune 不刪看不懂的東西。
- 同一代內改格式時升 head 的 `v`：較舊的 backup 與 prune 讀不懂最大世代就停止（§10）。
- 派生字串、AAD 常數、magic 的改動屬於下一代格式，不在 v4 內做。

## 15. 向量與 fixture

每條規範性規則至少一個資料檔向量或測試：金鑰推導與槽；CBOR 十條；切段規則與 Dir 驗證；Root 的種類
規則；chunk 清單的切段與 ChunkIndex 的 `size`；pack 的種類、上限與解壓上限；head、分片、記錄的編碼；GC 的情境（重疊的 prune、停住的 prune、
重建、grace 調低再調高、晚到的 delta、沿用被 forget 的 parent 子樹、不實的 trailer、讀不了的物件）。
v3 的 fixture 移除，凍結一份 v4 的。

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
