# kist 儲存格式 v1

> **狀態：M1 結束時凍結。** 之後只能透過每個物件的 `version` 欄位演進——加欄位要加版本號，改語意要加版本號。
> 這份文件描述的是**實作**，每一項都有 `testdata/` 的 golden file 或測試在守著；文末列出對應關係。

## 0. 一句話

repo 是一個 key-value 命名空間。除了 `config` 以外，所有物件都是不可變的、以內容 hash 命名的密文。沒有鎖，沒有需要就地更新的東西。

## 1. 命名空間

| Key | 內容 | 命名依據 |
| --- | --- | --- |
| `config` | repo 參數 + 金鑰封裝 | 固定名稱，**唯一的明文物件** |
| `keys/<id>` | 額外 key slot（多密碼 / 還原金鑰） | *M1 未實作*，格式已保留 |
| `packs/<hash>` | pack file | 密文全檔的 **unkeyed** BLAKE3-256 |
| `indexes/<hash>` | index blob | 密文的 unkeyed BLAKE3-256 |
| `trees/<hash>` | 目錄物件 | 明文 canonical CBOR 的 **keyed** BLAKE3-256 |
| `snapshots/<clientID>/<ts>` | 快照 | 固定位置 |
| `gc/<packID>` | 待刪標記（§12） | 固定位置 |
| `clients/<clientID>` | client 登記（§12） | 固定位置 |

### Key 文法

```
segment = (lowercase-alnum / "-" / "_") *( lowercase-alnum / "-" / "_" / "." )
key     = segment *( "/" segment )
```

三個限制各有原因，不是美學：

- **沒有冒號**——`2026-01-02T03:04:05Z` 這種 RFC 3339 時間戳在 Windows 上不是合法檔名，本機 repo 會直接壞掉。
- **segment 不能以 `.` 開頭**——local backend 在上傳中途會寫 `.tmp-<random>` 暫存檔，這條規則讓那些檔案永遠不可能被當成物件。
- 沒有 `..`、沒有空 segment、沒有大寫——路徑穿越與大小寫不敏感檔案系統的問題一次解決。

實作：`internal/backend.ValidateKey`。

### 不變條件

1. **讀得到的物件就是完整的。** 部分寫入絕不能出現在最終名稱底下，crash 之後也不行。這是「不用鎖也能檢查 repo」的根據。
2. **物件不就地修改。** 唯一的變更是 Delete，而且只有維護操作會做。
3. **`PutIfAbsent` 是條件寫入**，不是最佳化。備份客戶端若能覆寫既有的 pack，光靠自己的憑證就能毀掉整個 repo。

## 2. 金鑰階層

```
password
  │  Argon2id（參數存在 config 的 KeySlot，明文）
  ▼
KEK ──AEAD 解封──▶ master key（32 B，隨機）
                     │  HKDF-SHA256，salt = repoID
                     ├─ "kist/v1/chunk" ─▶ chunk key   （chunk payload）
                     ├─ "kist/v1/hash"  ─▶ hash key    （內容定址）
                     ├─ "kist/v1/index" ─▶ index key   （pack trailer、index blob）
                     └─ "kist/v1/meta"  ─▶ meta key    （tree、snapshot、gc）
```

- Argon2id 預設 RFC 9106 第二組：t=3、m=64 MiB、p=4。第一組（2 GiB）不是一個「同時在跑備份的機器」可以假設能配置的。
- salt 是 repoID，所以同一把 master key 用在兩個 repo 也會得到互不相關的 subkey。
- master key 封裝時的 AAD 是 `"kist/v1/master" || repoID`，所以 key slot 無法搬到另一個 repo 使用。

### AAD 對照表

每個密封物件都宣告自己扮演的角色，讓密文不能從 repo 的一處貼到另一處。

| 物件 | 金鑰 | AAD |
| --- | --- | --- |
| chunk payload | chunk | chunk ID（32 B） |
| pack trailer | index | `"kist/v1/pack-trailer"` |
| index blob | index | `"kist/v1/index"` |
| tree | meta | tree ID（32 B） |
| snapshot | meta | 完整 key path（含 clientID 與時間戳） |
| gc 標記 | meta | 完整 key path（`gc/<packID>`） |
| client 登記 | meta | 完整 key path（`clients/<clientID>`） |
| wrapped master key | KEK | `"kist/v1/master" || repoID` |

pack trailer 與 index blob 用**常數** AAD，因為它們的名字是自己密文的 hash——密封的當下名字還不存在，用名字當 AAD 會循環。代價是 A pack 的 trailer 貼到 B pack 上仍然能通過認證；擋住這件事的是兩層：trailer 的**一致性檢查**（entry 必須恰好鋪滿 chunk 資料區，不能有洞、重疊或重複），以及 pack 以密文 hash 命名——貼過去名字就對不上，`check` 會抓到。

### AEAD 封裝

```
sealed = nonce(24 B) || XChaCha20-Poly1305(plaintext, aad)   // 含 16 B tag
```

nonce 由 `crypto.NonceStream` 產生：從 32 B 種子展開成 BLAKE3 XOF。這不是為了方便，是為了讓「傳進一個不會前進的亂數來源」這個錯誤**不可能發生**——那會造成同一把金鑰下的 nonce 重用，是這個格式唯一無法倖存的錯誤。

## 3. Chunking

FastCDC，min 512 KiB / avg 2 MiB / max 8 MiB，normalization 2，seed 0。

- mask 由 `bits = round(log2(2 MiB)) = 21` 推出：`maskSmall = 1<<23 - 1`、`maskLarge = 1<<19 - 1`。寫成常數而非執行期 `math.Log2`——不同平台的浮點捨入足以無聲分裂格式。
- gear hash 每個 chunk 從 0 重新開始，**前 MinSize 位元組不進 hash**。
- 緩衝區保證游標後永遠有 ≥ MaxSize 位元組（除非輸入用盡），所以邊界只取決於內容，不取決於 reader 一次給多少。

自己實作，理由與等價性證明見 [ADR 002](decisions/002-chunking.md)。

**chunk ID = keyed BLAKE3-256(hash key, 明文)**，32 B，不截斷。keyed 的意義：能猜到檔案內容的攻擊者，無法用 repo 裡的名字來確認猜測。

## 4. Pack 檔

```
┌─────────────────────────────────────────┐
│ sealed chunk 0                          │  nonce(24) || AEAD(algo(1) || payload)
│ sealed chunk 1                          │
│ ...                                     │
├─────────────────────────────────────────┤
│ sealed trailer                          │  AEAD over canonical CBOR
├─────────────────────────────────────────┤
│ trailer length          8 B, big endian │
│ magic "kistpk" + version  6 B + 2 B BE  │
└─────────────────────────────────────────┘
```

從尾端往回讀：magic → trailer 長度 → trailer。這讓每個 pack 自我描述，index 因此永遠只是快取。

**Trailer**（明文形式）：

```cbor
{ "v": 1, "entries": [ [id(32 B), offset(uint), length(uint)], ... ] }
```

- `offset` 指向 sealed chunk 的第一個位元組（也就是 nonce），`length` 是 sealed 形式的完整長度。`(offset, length)` 就是 reader 要抓的 byte range。
- 這個三元組跟 index 存的三元組**完全相同**，trailer 與 index 不可能對同一件事有兩種解讀。
- 讀取時檢查的不只是「能不能解密」，還有一致性：entry 必須依序、無洞、無重疊、無重複地鋪滿 chunk 資料區，長度不得小於一個空信封、不得大於 `chunker.MaxSize + 1 + overhead`。用合法金鑰簽出來的爛 trailer 仍然是爛 trailer。

**Chunk framing**：AEAD 明文的第一個位元組是壓縮演算法（`0` = raw、`1` = zstd），其餘是 payload。放在密文裡面，所以解密後 chunk 自我描述，trailer 維持三欄。

**壓縮決策**：壓下去再比，不抽樣。保留壓縮結果的條件是 `len(z) < len(p) - len(p)/16`——省不到 1/16 就不值得每次還原都付解壓成本。解碼器上限設在 `chunker.MaxSize`：實測一個 1796 B 的 zstd frame 宣稱解出 16 MiB，會被直接拒絕。

**Pack 命名 = 密文全檔的 unkeyed BLAKE3-256**。三個後果：不持有任何金鑰也能驗證完整性；兩個 client 造出相同 pack 會自動去重；**寫入者在寫完最後一個位元組前不知道名字**，所以 pack 一定是先落到本機 spool 檔再上傳，不會在記憶體裡組。

目標大小 64 MiB，寫滿或 backup 結束時 flush。

## 5. Index blob

```cbor
{ "v": 1, "packs": [ [packID(32 B), [ [id, offset, length], ... ]], ... ] }
```

- 依 pack 分組，所以 packID 不會在每個 entry 重複。
- 編碼前依 packID 排序：Go 的 map 迭代順序是隨機的，不排序的話兩個 client 記錄同一批工作會寫出兩個 blob 而不是去重成一個。
- 一次 backup 一個 blob，在最後一個 pack 上傳之後、snapshot 提交之前寫。
- **壞掉的 blob 不是致命錯誤。** 打不開的 blob 會被跳過並警告，因為「打開 repo」正是執行 `rebuild-index` 的前提——把它當致命錯誤會變成一個修不了的死結。`rebuild-index` 先寫新 blob 再刪舊的，所以中途 crash 會留下重複（載入時會合併），不會留下空窗。
- **index 永遠只是快取。** `rebuild-index` 只讀 pack trailer 就能重建出完全相同的答案，測試會把 blob 全刪掉來證明這件事。

## 6. Tree 物件

```cbor
{ "v": 1, "entries": [ { "n": name, "t": type, "mode": …, … }, ... ] }
```

Entry 欄位（除 `n`、`t`、`mode` 外皆 `omitempty`）：

| 欄位 | 說明 |
| --- | --- |
| `n` | 名稱，不得為空、`.`、`..`，不得含 `/` 或 NUL |
| `t` | 0 = file、1 = dir、2 = symlink |
| `mode`, `uid`, `gid`, `mtime`, `ctime` | 中繼資料，`mode` 含 setuid/setgid/sticky |
| `size` | 檔案長度 |
| `target` | symlink 目標 |
| `chunks` | 檔案的 chunk ID 陣列，**inline** |
| `tree` | 子目錄的 tree ID |
| `dev`, `ino`, `nlink` | 硬連結識別（僅 `nlink > 1` 時記錄） |
| `xattrs` | 延伸屬性（M1 記錄，還原是 M4） |

**tree 以明文命名，不是密文。** 這一條是承重牆：密文 hash 每次密封都會變（nonce 是隨機的），沒改過的目錄每晚都會換名字，「未變動的子樹整棵重用」就永遠不會發生。

Entry 編碼前依名稱 bytewise 排序，所以一個目錄只有一種編碼、一個名字，跟 client 用什麼順序走它無關。

還原時的順序是**先 chown 再 chmod**。POSIX 的 chown 會清掉 setuid 與 setgid 位元，反過來做會無聲地把它們吃掉：

```
after chmod:  ugrwxr-xr-x
after lchown: -rwxr-xr-x
```

Socket、FIFO、device node 會被跳過並警告：忠實還原它們需要還原程序不該假設有的權限，而且它們的「內容」從來不是使用者想存的東西。

**大檔的 chunk list 內嵌。** 100 GiB 的檔案約 1.6 MiB 的 ID，tree 扛得住。扛不住的是「一個目錄裡放很多超大檔案」——動一個檔就要重寫一個很大的 tree。那是 tree v2 加一層 indirection 的時機，[ADR 004](decisions/004-tree-and-naming.md) 記錄了它，現在**刻意不做**。

## 7. Snapshot

Key：`snapshots/<clientID>/<ts>`，`ts` 格式 `20060102t150405.000000000z`。

固定寬度所以字典序就是時間序；沒有冒號所以在 Windows 上是合法檔名。

```cbor
{ "v": 1, "root": treeID, "time": int64 ns, "host": str,
  "paths": [str], "client": str, "stats": {...} }
```

- **snapshot 是 commit point**：所有 pack、tree、index 都上傳完成後才寫。M1 之後的每個操作都遵守這個順序。中途死掉只會留下沒人引用的物件——浪費空間，不會產生讀不出來的 repo。
- 寫入用 `PutIfAbsent`。撞到同一奈秒的兩個 client 是兩次備份，不是一次覆蓋另一次；碰撞就往後推一奈秒重試。
- AAD 是完整 key path，所以 snapshot 被搬到另一個 client 的命名空間就打不開了。
- 讀取時還會檢查「key 說的」和「物件說的」是否一致：key 只是名字，物件才是紀錄，只信一邊的 reader 可以被一次改名騙過。

`stats` 只是回報，不是結構：沒有任何讀取路徑拿它做決定。欄位：`files dirs symlinks bytes chunks_new chunks_read packs_added packs_revived bytes_stored`，全部 `omitempty`。`packs_revived`（§12）是 M3 加的；因為 decoder 拒絕未知欄位，M1/M2 的 binary 讀不了帶這個欄位的 snapshot——v1 尚未對外發佈，所以直接改，不另開版本。

`clientID` 是本機持久化的 16 B 隨機值（`$XDG_CONFIG_HOME/kist/clients/<repoID>`），不是 hostname。hostname 在一個機群裡會重複、改機器名就會變、而且會把不必要的資訊寫進 repo 列表。hostname 放在 snapshot 內容裡當人看的標籤。

## 8. Config

**唯一的明文物件。**

也是格式上唯一**允許**被覆寫的物件——未來的 `key add`（新增 key slot）需要這個能力。但**目前沒有任何生產程式碼會覆寫它**：`saveConfig` 走的是 `PutIfAbsent`，`Init` 另外還先檢查 config 是否已存在。`Backend.Put`（無條件覆寫）現在沒有生產呼叫者，它存在是為了 M2 之後的 `key add`。

```cbor
{ "v": 1, "repo_id": 16 B, "created": int64 ns,
  "chunker": { "min": …, "avg": …, "max": … },
  "slot": { "v": 1, "kdf": { "alg": "argon2id", "t":…, "m":…, "p":…, "salt": 16 B },
            "wrapped": …, "created": … } }
```

明文是必要的：任何金鑰存在之前就必須先讀得到 Argon2id 參數與 salt。其餘欄位對「能列出這個 repo 的人」本來就不是秘密。

chunker 參數會被記錄並在 open 時強制比對。參數不同的 repo 跟這個 build 完全去重不到，寧可拒絕開啟也不要無聲地讓它膨脹一倍。

## 9. CBOR 規則

- 一律 Core Deterministic（RFC 8949 §4.2.1）：最短形式、map key bytewise 排序。**這不是美觀問題**——tree 以編碼的 hash 命名，兩個編碼器差一個 byte 就會讓一個目錄有兩個名字。
- 時間一律 `int64` 奈秒欄位，不用 CBOR time tag。
- 解碼嚴格：拒絕重複 map key、拒絕不定長度項目、拒絕未知欄位、拒絕尾端多餘位元組，並限制陣列與 map 大小。
  - **拒絕未知欄位**是刻意的：不認識的欄位代表這個物件是更新的格式寫的。默默丟掉它會讓 reader 拿半份文件去做決定；`version` 欄位存在的意義就是讓這件事變成一聲響亮的失敗。

## 10. 權限模型

`PLAN.md` 說「backup 只需要 Put 權限」。**實作之後這句話要修正**。下表不是推理出來的，是 `TestS3BackupPolicy` 用一個只持有這張表權限的 MinIO 使用者跑完整個 backup 得到的——少一項就跑不完：

| 操作 | Get | List | Put（條件式） | Delete |
| --- | --- | --- | --- | --- |
| `backup` | `config`、`indexes/*` | `indexes/`、`gc/` | `packs/ indexes/ trees/ snapshots/ clients/` | 僅 `gc/*` |
| `restore` / `check` | 全部 | 全部 | — | — |
| `forget` / `prune` / `rebuild-index` | 全部 | 全部 | `gc/*`、`indexes/*` | 全部 |

backup **不需要**讀 tree：tree 一律用 `PutIfAbsent` 寫，去重靠的是「已存在」的回應，不是先讀再比。

M3 加了三項，每一項都是 §12 的安全論證需要的：`clients/*` 的 Put（登記自己，讓 prune 知道要等誰）、`gc/` 的 List（看見待刪標記，把被標記的 pack 當成不存在）、`gc/*` 的 Delete（復活）。其中 Delete 是唯一一項 backup 拿得到的刪除權限，而它能刪的東西——一個「沒人需要這個 pack」的宣告——被刪掉的後果只是 prune 下次重算。

同一個測試也驗了反面：持有 backup 權限的 client 對 pack、`config`、`clients/<id>` 做 Delete → `ErrDenied`；對 `packs/`、`trees/`、`snapshots/`、`clients/`、`` 做 List → `ErrDenied`；讀 pack → `ErrDenied`；對既有 pack 做 `PutIfAbsent` → `ErrExists`（條件寫入在政策限制下仍然正常運作）；對 `gc/<packID>` 做 Delete → 成功。

### 抗勒索性質，以及它在哪裡成立

真正的性質不是「只有 Put」，是：

> **在資料前綴上沒有 Delete，而且 Put 是條件式的。**

前半由 IAM 保證，兩個服務都成立。後半分兩種情況：

- **AWS S3**：bucket policy 可以用 `s3:if-none-match` 條件鍵**要求** PutObject 必須帶 `If-None-Match`（[AWS 文件](https://docs.aws.amazon.com/AmazonS3/latest/userguide/conditional-writes-enforce.html)，2024-11 起）：
  ```json
  "Action": "s3:PutObject",
  "Condition": {"Null": {"s3:if-none-match": "false"}}
  ```
  這樣一個被入侵的 client 送出無條件的 PutObject 會被儲存端拒絕。性質屬於**儲存端**。**UNVERIFIED**：本機沒有 AWS 帳號，這一條沒有實際跑過。
- **MinIO（RELEASE.2025-09-07）**：拒絕這個條件鍵（`invalid condition key 's3:if-none-match'`）。政策裡放不進去，所以無條件的 PutObject 會成功並覆寫 pack——`TestS3BackupPolicy` 實測 `err=<nil>, overwrote=true`。在 MinIO 上，覆寫保護只對**誠實的 client** 成立：kist 自己永遠送 `If-None-Match: *`，MinIO 也正確地以 412 拒絕（conformance 測試證明），但政策攔不住一個不送這個 header 的攻擊者。

所以 `PLAN.md` 的「Put 權限-only 的 IAM policy 可完成 backup」驗收條件通過了，但它證明的東西比看起來少：它證明 backup 不需要 Delete，沒有證明 backup 不能覆寫。後者在 AWS 上可以用一行條件補上，在 MinIO 上目前不行。

### Object Lock 與版本控制

`PLAN.md` 說 `prune` 對受鎖物件「直接略過並回報」。實測（`TestS3ObjectLockIsReportedNotFought`，MinIO `mc mb --with-lock` + `retention set --default governance 1d`）之後，行為是這樣定的：

- 在有版本控制的 bucket 上，不帶 version ID 的 `DeleteObject` **成功**——寫一個 delete marker——即使底下的版本被鎖住。之後對那個 key 的 Get 是 404、List 不再列出它，但位元組留著繼續計費。只有指定 version 的刪除才會被拒絕。
- kist **永遠不刪版本**。要在版本控制底下真的回收空間，是 bucket 擁有者的 lifecycle policy 的事（`NoncurrentVersionExpiration`），不是備份工具該自己動手的。
- `backend.Delete` 在 `DeleteObject` 之後用 `ListObjectVersions` 看一眼：那個 key 還有版本留著就回 `ErrLocked`。列版本是另一項權限（`s3:ListBucketVersions`），拿不到就當作看不見——刪除本身已經發生了。
- `prune` 對回 `ErrLocked` 的 pack：計入 `locked`、**不計入回收位元組**，其餘處理跟刪掉的 pack 完全一樣——index 停止指向它、標記留到下一輪。它已經讀不到了，index 再指著它就是謊話。`forget` 對 snapshot 同理。維護用的刪除（舊 index blob、標記、垃圾）把 `ErrLocked` 當成功：要的結果（key 讀不到了）已經成立。

MinIO 上這條全部跑過。AWS S3 的行為文件上相同，**UNVERIFIED**。

## 11. 這些說法各自由什麼守著

| 說法 | 證據 |
| --- | --- |
| AEAD 信封、子金鑰、key slot 不變 | `internal/crypto/testdata/{envelope,subkeys,ids,keyslot}.*` |
| 一個位元組的竄改（金鑰/AAD/nonce/密文/tag）一定失敗 | `TestOpenRejectsTampering`，9 個案例 |
| nonce 來源不前進也不會重用 nonce | `TestNonceStreamAdvancesEvenWhenItsSourceDoesNot` |
| chunk 邊界不變 | `internal/chunker/testdata/boundaries.txt`（64 MiB 決定性輸入）+ `TestGearTableDigest` |
| 邊界與 reader 的讀取大小無關 | `TestBoundariesDoNotDependOnReadSizes`（`iotest.OneByteReader`） |
| 插入位元組不會重排後面的邊界 | `TestBoundariesSurviveAnInsertionAtTheFront` |
| pack 位元組不變 | `internal/pack/testdata/pack.txt`（完整的小 pack，逐位元組） |
| pack 損壞會被拒絕 | `TestReaderRejectsDamagedPacks`（9 種）+ `TestReaderRejectsInconsistentTrailer`（7 種） |
| 同一個 chunk 不會在一個 pack 裡出現兩次 | `TestDuplicateContentWithinOnePackIsStoredOnce`、`TestWriterRefusesADuplicateChunk` |
| index blob 壞掉不會讓 repo 打不開，而且修得回來 | `TestADamagedIndexBlobIsRepairable`、`TestRebuildIndexPersists` |
| restore 不會寫到目標目錄外面 | `TestSafeJoinRefusesEscapes` |
| setuid/setgid/sticky 位元會被還原 | `TestBackupRestoreIsByteForByte`（模式比對含這三個位元） |
| 解壓炸彈被擋 | `TestDecompressionBombIsRefused`（1796 B → 宣稱 16 MiB） |
| index blob 不變、可從 pack 重建 | `internal/index/testdata/index.txt`、`TestRebuildReconstructsTheIndexFromPacksAlone` |
| tree 不變、順序無關、內容變則名變 | `internal/tree/testdata/tree.txt`、`TestNewSortsEntries`、`TestAChangedEntryChangesTheName`（10 種變動） |
| snapshot 不變、不覆寫、綁定 key | `internal/snapshot/testdata/snapshot.txt`、`TestSaveNeverOverwrites`、`TestSnapshotIsBoundToItsKey` |
| backup → restore 逐位元組相同 | `TestBackupRestoreIsByteForByte`、`TestAcceptance` |
| 第二次備份幾乎不寫東西 | `TestSecondBackupOfUnchangedDataWritesNoPacks` |
| 只改一個檔只重寫那條路徑 | `TestIncrementalBackupOnlyRewritesTheChangedPath` |
| `check` 抓得到人為破壞的 pack | `TestCheckDetectsDamage`、`TestOnlyReadDataCatchesAFlippedBitInAChunk` |
| 同一個 chunk 在多個 pack 裡時，index 指向的那個跟加入順序無關 | `TestDuplicateChunkResolvesToTheSmallestPackID` |
| retention 規則按 UTC 分桶、跳過空桶、每個 client 各算各的 | `TestRetentionPolicyApply`、`TestRetentionPolicySkipsEmptyBuckets`、`TestForgetAppliesThePolicyPerClient` |
| 沒給規則也沒指名 → 拒絕 | `TestForgetRefusesToForgetEverything` |
| prune 兩階段、grace、client 條件、標記不刷新、dry-run 不動手 | `TestPruneMarksThenSweepsAfterTheGrace`、`TestPruneDoesNotRefreshAnExistingMark`、`TestPruneDryRunChangesNothing` |
| 壞掉的 repo 不能 prune | `TestPruneRefusesAnUnhealthyRepository` |
| 刻意競態下不會刪到活的 chunk（A–E，§12） | `TestPruneRaceSnapshotLandsAfterMark`、`TestPruneRaceBackupInFlightAtSweep`、`TestPruneRaceRevival`、`TestPruneRaceDuplicatePacksFromConcurrentBackups`、`TestPruneRaceRevivalDuringSweep`、`TestPruneRaceMarkDuringBackupWithoutUnmarkPermission`；D 另在 MinIO 上跑 `TestS3PruneReclaimsDuplicatePacks` |
| sweep 中途 crash 不會留下「index 指著不存在的 pack 而標記已消失」 | `TestPruneRewritesTheIndexAfterACrashedSweep` |
| 拿不到 `gc/*` Delete 的 backup 仍然安全 | `TestBackupSurvivesBeingUnableToUnmark` |
| `clients/` 底下的垃圾擋不住 prune、也不會被刪 | `TestPruneToleratesJunkClientRecords` |
| 時鐘偏差在容許值內會 hold | `TestPruneHoldsWithinTheClockSkew` |
| backup 權限只能刪 `gc/*` | `TestS3BackupPolicy/can_revive_a_marked_pack_and_nothing_more` |
| Object Lock 底下：回報、不計回收、index 不再指向、repo 仍健康 | `TestS3ObjectLockIsReportedNotFought` |

## 12. 垃圾回收（M3）

沒有鎖，所以 GC 是用**時間**和**兩個小物件**換來的。全部細節與論證在 [ADR 007](decisions/007-garbage-collection.md)；這裡只放格式。

### 物件

```cbor
gc/<packID>        { "v": 1, "marked": int64 ns, "by": clientID }    // meta key，AAD = key
clients/<clientID> { "v": 1, "first_seen": int64 ns }                 // meta key，AAD = key
```

兩者都用 `PutIfAbsent` 寫、**從不更新**：標記越舊 pack 越快能刪，重寫等於 grace 永遠不會到期；登記時間是下界，之後的活動由 snapshot 說。

### 生命週期

```
活的 pack：某個 snapshot 的某個 chunk 在 index 裡「解析到」它。
            解析 = 同一個 chunk 在多個 pack 裡時取 packID 最小的那個（§5），
            所以兩個 client 同時打包同一份資料，一個活、一個死。

prune 一輪 =
  1. 列 gc/、indexes/、clients/、snapshots/（順序有意義），讀所有 pack trailer，走遍所有 snapshot。
     任何 snapshot / tree / chunk 解析不到 → 中止，不動任何東西。
  2. 標記：死而未標 → 寫 gc/<id>；活而有標 → 刪標記。
  3. 清掃：有標、標記早於 now − grace、這輪算出來還是死的、
           而且每個「還在等的 client」的最後活動都晚於 標記 + clock-skew
           → 刪 pack。標記**留著**。
  4. index 若指到任何不存在的 pack（這輪刪的，或上一輪刪到一半 crash 的）→ 重寫 index blob。
  5. 這輪開始時就已經不存在的 pack 的標記 → 刪。步驟 4 一定在 5 之前。

backup 開頭 =
  登記 clients/<id> → 列 gc/ → 重新載入 index → 才開始讀檔。
  chunk 已存在但在被標記的 pack 裡 → 當成不存在：重傳，並刪標記（一個 pack 一次）。
  提交 snapshot 之前再列一次 gc/：這次備份參照到的 pack 若在中途被標記 → 刪標記。
```

「還在等的 client」= 最後活動（登記時間與最新 snapshot 取晚者）距今不超過 `--forget-clients-after`（預設 10 × grace）。預設 grace 72h、clock-skew 1h。

### 假設（違反其中之一，論證就不成立）

1. 後端 read-after-write 一致：寫完的物件立刻能被 List 和 Get 看到。S3 自 2020 起如此；M4 的 SFTP 後端也必須如此。
2. 一個 client 同一時間只跑一個 backup。
3. client 與 pruner 的時鐘差距在 `--clock-skew` 之內。
4. 一個 backup 不會跑超過 `--forget-clients-after`。

## 相關決策

- [001 — 專案骨架、module path 與工具鏈基準](decisions/001-project-skeleton.md)
- [002 — 內容定義切塊：自己實作 FastCDC](decisions/002-chunking.md)
- [003 — Pack 格式與壓縮](decisions/003-pack-format.md)
- [004 — Tree、Snapshot 與命名](decisions/004-tree-and-naming.md)
- [005 — Backend 原子性與權限模型](decisions/005-backend-atomicity.md)
- [006 — S3 後端與無鎖並發](decisions/006-s3-backend.md)
- [007 — 垃圾回收：標記、grace、登記與復活](decisions/007-garbage-collection.md)
