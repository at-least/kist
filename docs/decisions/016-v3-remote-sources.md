# 016 — v3：roots、metadata 聯集、touch 復活與「遠端來源」需求

日期：2026-09-08。狀態：實作中（kist-rs 已遷移；kist-go 移植與雙向 E2E 進行中）。

## 背景

v2 完成並雙向驗證後出現兩個新輸入：

1. **需求：備份兩個遠端之間**——來源可以是 SFTP/S3（client 當轉運：讀遠端、
   切塊、加密、上傳，金鑰不出機器）。v2 的格式雖然「塞得下」遠端來源
   （欄位皆可省略），但 metadata 模型是 POSIX 形狀：`uid/gid` 省略若 0
   （root 的檔案被記成「沒記錄」）、`mode=0` 的意思是 Windows——遠端來源
   得假裝自己是 Windows。
2. **v1/v2 的教訓清單**：合成根是 stats 口徑分裂點（2026-09-08 釘定）、
   過程計數讓兩實作「合法地」分歧、AAD 列舉欄位每加一個不變式就要改排版、
   每次備份重 put 全部樹既是寫放大也是 §6a HEAD→DELETE 視窗的根源、
   `packs_revived` 這種依 GC 狀態而變的數字不該在不可變結構裡。

v3 的設計立場：**v2 核心全保留**（pack、每 chunk AEAD、明文 keyed tree
命名、欄位表 CBOR、凍結 FastCDC、supersedes index、兩階段無資訊 GC、
Put-only backup、snapshot 唯一 commit point、parity sidecar——它們有
跨語言 PoC 與 200-case proptest 背書），變更是外科手術式的九項，每項
對應一個教訓或需求（見 format-v3-draft §0 對照表）。設計軸的轉移：
**「來源能證明什麼就記什麼」**——內容 hash、版本 ID 是來源能證明的，
mtime/uid 是來源聲稱的；變更偵測與還原保真度都掛在可證明性上。

## 決定

### 1. roots 取代合成根

`Snapshot.roots = [{path, tree}]`（path 為不透明定位：本機絕對路徑、
`sftp://…`、`s3://…`）。樹節點名**永遠**是單一路徑元件——v2 的「根節點
以絕對路徑命名」是全格式唯一的例外，stats 口徑就在這裡分裂，而且
`s3://bucket/prefix` 根本不是檔案系統路徑。restore/mount 依同一套映射：
去 scheme、`/` 切段；檔案來源（root tree 恰一個非目錄 entry、名稱＝定位
末段）落在父路徑——與 v2 絕對路徑還原落點一致，且「目錄恰好只含一個
同名檔案」的還原結果相同，判別沒有歧義代價。`backup /`（定位切不出
組件）在 mount 攤平到頂層、restore 直接落在 target——與 v2 一致。

### 2. stats 只留資料事實

snapshot.stats 收斂為 files/dirs/symlinks/bytes（依 mk 定義口徑；dirs
不含 roots——root 是 path 不是 entry；bytes 對 (dev,ino) 群組跨 roots
只算一次）。過程計數（chunks_new、packs_revived…）依 GC 狀態與去重
順序而變，**移出格式**進 `BackupReport`。判準：不可變結構只收「兩個
實作對同一棵樹會數出同一組數字」的東西。

### 3. metadata 聯集（mk）

`Entry.mk`（posix/sftp/s3/generic）＋per-kind 必填/選填/必須缺席規則
（讀取端強制）。posix 的 uid/gid 必填——uid 0 = root 是真實值；s3 沒有
mode/uid，缺席＝來源沒有而不是 0。新增 `etag`（來源計算的內容指紋）與
`vern`（來源版本 ID）。

### 4. 快速路徑分級（內容可證明 > kernel 可證明 > 沒有）

posix：size+mtime+ctime+inode+dev（kernel 背書＋racy guard）。
s3：etag+size（etag 是來源計算的內容指紋——內容證明，無需 racy guard）。
sftp/generic：**沒有**快速路徑——mtime 是來源聲稱的，重讀靠 chunk 去重
吸收。mtime-only 沿用非法。

### 5. 不變式入 wrapped master 密文

`wrapped = AEAD(KEK, master(32) ‖ Invariants CBOR)`，AAD＝常數。解鎖即
取得**認證過**的 repo_id/chunker，與明文 config 比對（不符＝明確的
「config 已被竄改」）。v2 的 AAD 列舉式每加不變式都要改排版；v3 加欄位
只動 Invariants（零值省略＋忽略未知＋never-round-trip 適用）。

### 6. touch 復活（本 ADR 最重要的競態決定）

樹改為 write-once：backup 對新樹 put_if_absent、對沿用的樹**覆寫式 Put**
`touch/<hex>`（8 bytes）刷新 mtime——mtime 刷新就是復活訊號。prune 刪樹
的條件加上「touch 不存在或**嚴格小於**標記」（touch ≥ 標記＝活，同秒取
安全側）。效果：樹 bytes 永不重寫（v2 每次 backup 在 S3 重傳全部樹）；
commit 前檢查從「全部寫過的樹逐個 HEAD」縮到「開始時已過期標記 ∩ 可達
樹」（平常空集合），作為 prune HEAD→DELETE TOCTOU 的防線。

**advisor 檢查點抓到的初版錯誤**：touch 若設計成 PutIfAbsent（冪等、不
重設時間），第二次 backup 重用同一棵已標記的樹時 mtime 不會刷新——
「D0 touch → D1 標記 → D5 backup#2 重用（touch 沒刷新）→ prune 刪樹 →
snapshot 指向已刪的樹」，資料遺失。修法：覆寫式 Put（內容固定 8 bytes，
覆寫無害；v2 本來就在覆寫整棵樹的 bytes，暴露面反而縮小）＋時間線釘成
向量 V3-GC-5。**教訓：GC 鄰接面的每個「顯然」都要過 proptest 與外部
審查；這個洞是設計審查抓的，不是測試跑出來的。**

附帶決定：樹的 self-heal 保留——put_if_absent 撞 AlreadyExists 時驗證
既有 bytes（解密＋名稱 hash＋結構），壞了才覆寫。v2 靠無條件重 put 順手
療癒壞樹；v3 正常路徑零額外寫入，覆寫只發生在驗證失敗時，且同名必同
內容（名稱＝明文 keyed hash），不可能蓋掉不同的合法樹。

### 7. 副本而非 RS-bundle

trees/snapshots 是不可重建的 metadata（packs 有 parity，它們沒有；
snapshot 是 commit point）。選**每物件副本** `.r1`（k=1 的 RS、零新概念、
現有 keyed hash/AEAD 驗證任何一份），否決 RS-protected checkpoint
bundle——bundle 是第二真相源，有自己的新鮮度、標記、幽靈問題（幽靈 pack
bug 的物種）。副本寫在主體之前（snapshot `.r1` 先行：主體出現＝commit）；
GC 成組生命週期；**孤兒 `.r1`（主體不在、無標記）不自動刪**——那是
「主體意外遺失」的災難訊號，check 回報、restore 自動落副本。replicas
預設依後端：本機 1、遠端 0（InitOptions 可覆寫）。

### 8. min_reader、index 壓縮、規格過程

- `config.min_reader`：混合版本 client 是常態；「解碼到 v≠N 就拒絕」是
  斷崖，半讀半猜比明確拒絕危險。
- 有效 index blob > 64 時 prune 必須合併（ADR 005 明載未做的運維債變成
  可測規範）。
- 規格單一權威副本（kist-rs 持有，Go 端 CI 檢查逐 byte 一致）；每條
  規範性規則掛向量編號（V3-*）；向量由**產品端（Rust）錄製**，Go 對
  （v2 時是 Go 錄——產品定位確立後主客易位）。

## 遠端來源（client 端）

`kist-backend::source::Source`（list/read，blocking Read 介面；遠端實作
內部以 Handle::block_on 橋接 async 串流）。CLI `kist backup sftp://…` /
`s3://…`。威脅模型不變：兩端都只看得到密文。s3 來源的 etag 快速路徑讓
「列清單而不重讀內容」成為可能——這是 `src_etag` 進格式的實際動機。

## 後果

- gc_race 200-case proptest 必須在 v3 語意（touch）下重跑。
- v2 的「樹重 put 自我修復」改為驗證式修復（見決定 6 附帶）。
- 混合版本：min_reader 之前寫的 v2 讀取端讀 v3 repo 會在版本檢查失敗
  ——v2 無真實資料，接受。
- 已知限制：SFTP 來源無符號連結與 xattr（object_store 不暴露）；檔案
  來源在 S3 上的「目錄 vs 檔案」判別是精確的（key 不會等於自己的
  prefix），本機來源用 metadata 判別。
