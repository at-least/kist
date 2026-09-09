# ADR 011：M5 —— 大 repo 的 backup 峰值記憶體（< 512 MiB）

日期：2026-09-06。狀態：已採用。

## 目標與驗收

PLAN 的 M5 目標：「100 萬檔 repo 的 backup 峰值 < 512 MiB」。量化成兩道門：

1. **報告數字**：cgroup 取樣的 `memory.current − file − slab_reclaimable` 最大值
   （50 ms 一次、整個 process tree）。扣 `file` 是因為本機後端寫 pack 的 page cache
   會被 cgroup 計費但不是常駐記憶體；扣 `slab_reclaimable` 是因為走訪 100 萬檔會在
   cgroup 記下 ~400 MiB 的 dentry/inode cache，那也是 kernel 的，不是我們的。
2. **硬門檻**：`systemd-run --scope -p MemoryMax=512M -p MemorySwapMax=0` 下完成
   backup 不被殺。kernel 會先回收可回收頁，所以過了才是真的過；取樣可能漏掉
   瞬時峰值，門檻不會。**門檻跑法是較準的量測**：不限量跑法下 kernel 懶得
   回收 slab，`current − file − slab_reclaimable` 會高估；B 集取樣最壞 524 MiB
   對上門檻跑法實測 442–445 MiB，以門檻為準（另記 scope 的 `memory.peak`
   供對照）。

兩個測試集（seed 產生器 `bench/memory/generate.py`，~100 萬 chunk / ~1 GiB）：

- **A**：1000 個目錄 × 各 1000 檔——常態形。
- **B**：單一平面目錄塞 100 萬檔——parent tree 鏈與 `read_dir` 列表的最壞形。
  parent 結構只在有 parent snapshot 時存在，所以 B 的關鍵跑法是**第二次** backup。

## 結果

release build、本地後端、N≥3 取最大：

| 測試集 | 修正前 | 修正後 | 硬門檻 |
|---|---|---|---|
| A first+second | 1.17–1.9 GiB | 373–431 MiB | 3/3 通過 |
| B first+second | 1.69 GiB | 442–524 MiB | 4/4 通過 |

非門檻記錄：restore（A 快照）71 MiB。

## 找到的根因與修正

用 dhat（`--features kist-cli/dhat`）在 10 萬檔子集歸因，再逐項修、逐項量：

1. **每檔一個 16 MiB zeroed 切塊緩衝**（全部配置量的 94%：100k 檔就配掉
   1.56 TiB）。`Chunks` iterator 每次建立都 `vec![0u8; 2*max]`。
   → kist-chunker 新增 `chunks_with_buf` / `Chunks::take_buf`；backup 整趟
   重用同一批緩衝（小池化）。歸零與配置只發生一次。
2. **每檔一個 1 MiB BufReader**。切塊器的 `fill()` 本來就會把 2×max 的緩衝
   讀滿，BufReader 只是多一層 memcpy 與每檔一次的大配置。
   → 拿掉，File 直接交給切塊器。
3. **pack 緩衝以 Vec 倍增長到 128 MiB 級容量**（target 64 MiB）。`finish()` 後
   整塊變成上傳中的 bytes，疊上新一輪從 8 bytes 開始倍增的 buf。
   → `PackWriter::new` 多收 `chunker.max`，把 buf 容量一次配到
   `target + max_sealed`（64 MiB + 8 MiB + 41），extend 永遠不會觸發倍增；
   `finish()` 換上同容量的新 buf。容量不變式有測試釘住。
4. **tokio blocking pool 預設上限 512 條**。每條一個 glibc arena，各自保留
   高水位，峰值隨 arena 數放大。加密/切塊/壓縮都是 CPU 密集，4 條就夠。
   → CLI 的 runtime 明確 `max_blocking_threads(4)`（連帶 `enable_all()`）。
5. **CBOR 解碼走 `ciborium::value::Value` 中繼**：先解成通用值樹再轉目標型別，
   100 萬 chunk 的 index blob 多出 ~280 MiB 的裝箱垃圾，還把整份文件解兩遍。
   → `cbor::decode` 直接解進 `T`。尾端多餘 bytes 的檢查保留；重複欄位改由
   serde 的 struct visitor 拒絕（wire 型別都是 struct）；深巢狀輸入的防線
   從「資源限制」變成「型別不符」（wire 型別是淺層 struct，遞迴深度天然有界），
   對應測試一併改釘新性質。
6. **index blob 編碼/加密的整份拷貝**：cbor::encode 物化 56 MiB 明文、zstd
   再一份、AEAD encrypt 再一份。
   → CBOR 直接串流進 zstd 編碼器（明文不再物化）；加密改
   `seal_index_blob_in_place`（就地加密，呼叫端 `reserve(TAG_LEN)`）。
7. **收尾階段的疊加**：走訪結束後 overlay（去重 HashMap，100 萬 chunk ≈
   185 MiB）還活著就去做 index blob 編碼。
   → flush 後立刻 `index = None`，再等上傳、寫 index blob。
8. **快取增量合併物化整張舊表**：`records.extend(t.iter()...)` 把 96 B/筆的
   舊表整份收進 Vec。
   → `DiskTable::build_sorted`（預排序串流寫入）+ `MergeOldNew` k-way merge
   （同 ID 新的贏，與原本「新紀錄排前面」同一語意）；rebuild 改樂觀單遍
   （blob 讀完即丟，遇到 supersedes 才退回保守兩遍）。
9. **平面大目錄的 parent 結構**（B 集第二次 backup ~450 MiB）：`read_tree_chain`
   把整個目錄的 parent entries 讀成 `Vec<Entry>`，再 clone 一份 name 建成 HashMap。
   → `ParentStream` 串流游標：一次只持有一個 segment（≤ 10,000 節點），與
   遞增的目錄列表 merge-join；列表同時只存名稱 bytes（~45 MiB，原本存
   `(name, PathBuf)` 要 ~240 MiB）。跨段 parent reuse 有專門測試。
10. **第一次 backup 的 commit 無謂載入**：`referenced` 必為空（新寫的 chunk
    都在 own_packs），卻仍 `load_index()` 觸發一次全量 index 讀取與快取重建。
    → `referenced` 為空直接跳過。

行為等價由既有測試釘住：golden files、跨語言 conformance 向量（切塊邊界、
tree CBOR）、pack 往返、fuzz target 的不變式，全部通過。

審查（reviewer）之後補的修正：`MergeOldNew` 在舊表先耗盡時會無限迴圈、
表尾讀取錯誤會被吞掉（都有迴歸測試）；切塊緩衝取得順序提前到內部不變量
檢查之後；`cbor::decode` 不走 Value 中繼之後，唯一的一般 map 型別
（tree 的 `xattrs`）補回重複 key 的拒絕（`deserialize_with` 自訂 visitor，
恢復 format.md §4 的完整語意）。

## prune 的峰值記憶體（2026-09-06 追加）

prune 同樣以「100 萬 chunk repo、512 MiB 門檻」驗收（`bench/memory/run-prune.sh`）。
repo 用**當下程式**重建（`run-baseline.sh`，B 測試集；舊 repo 的 tree 未分段，
量出來的數字混雜了歷史格式）。

| 修正前 | 修正後 | 硬門檻 |
|---|---|---|
| 1.00–1.05 GiB | 225–234 MiB | 3/3 通過（memory.peak ~235 MiB） |

dhat 歸因（max-live 總和 1266.7 → 466.6 MiB）找到四個根因：

1. **`walk_tree` 遞迴抱住整條 prev chain**：遞迴是 `Pin<Box<dyn Future>>`，
   每層在 await 下一層時自己的 `tree` 還活著；大目錄上百個 segment 全部同時在
   記憶體（1M 檔 ≈ 240 MiB，1M 個 Entry 同時活著）。→ 先 iterate 自己的
   entries、明確 `drop(tree)`、再遞迴 prev；同時只有一個 tree 在記憶體。
   check 與 prune 共用 walk，同時受益。
2. **三份 100 萬級結構**（ADR 005 §5 的承諾）：walk 用的 `ChunkIndex` overlay
   HashMap（~200 MiB）、`canonical: HashMap<ChunkId, ObjectId>`（~130 MiB）、
   `referenced_chunks: HashSet`（~67 MiB），加上 repack 的 `kept_chunks` HashSet。
   → 合一成 `PruneIndex`：所有 (chunk, holder) 攤平成 `Vec<TableRecord>`
   （88 B/筆），依 (chunk, pack) 排序；contains/get 二分搜尋；referenced 從
   HashSet 改 Vec 收集、排序去重後線性合併打成旗標；正本在原本時點掃組計算。
   **排序鍵不含 phantom/marked**——marks 的取得時點是競態語義的一部分，
   正本選擇維持在 walk 之後、list 過 gc/ 之後。
3. **`canonical.values().collect()` 照 size_hint 保留 100 萬槽**（實際只有
   16 個 pack，66 MiB 瞬時）→ 逐個 insert。
4. **decoded index blobs 整份留著 + `indexed` clone 一份** → entries 從 blob
   move 出來，blob 用完即丟。

等價性由 model-based 測試鎖住（`prune::tests`）：隨機 holders/marks/phantoms
（含跨 blob 重複的 (chunk, pack) 紀錄——兩個 prune 重疊時 index 取聯集會產生）
比對新掃描與舊 canonical 表語義（正本選擇、live bytes、phantom-only 拒絕、
`held_by` 的 kept 語義、與 `ChunkIndex` 的最小 pack 規則）；重複紀錄由
`PruneIndex::new` 排序後去重（獨立 reviewer 抓到：不去重時 live bytes 會被
重複累計，repack 門檻偏保守、自癒但屬語義漂移）。gc_race proptest 100 案例
通過。

## 沒做（留給之後）

- **overlay 的 88 B/entry**：`HashMap<ChunkId, ChunkLocation>` 在 100 萬 chunk
  時 ~178 MiB（resize 瞬間新舊表並存 ~267 MiB），是目前 backup 峰值的大頭。
  要再降得換緊湊的開放定址結構，違反「保守直白」，除非目標再往下修。
- **prune 的 index blob 載入**：`load_index_blobs` 仍把全部解碼後的 blob 收在
  Vec 裡（~88 MiB 瞬時，用完即丟）；要再降可逐 blob 串流。
- **overlay 的 88 B/entry**：`HashMap<ChunkId, ChunkLocation>` 在 100 萬 chunk
  時 ~178 MiB（resize 瞬間新舊表並存 ~267 MiB），是目前峰值的大頭。要再降
  得換緊湊的開放定址結構，違反「保守直白」，除非目標再往下修。
- **MAX_INFLIGHT_UPLOADS = 1**：每個 in-flight pack 都是整份 64 MiB。S3 上
  第二個上傳的吞吐收益小於 64 MiB 的代價；若未來量測發現上傳吞吐受影響
  （ADR 004 的 S3 效能量測還沒做），可改成「依位元組數」而不是「依個數」限流。
- **index blob 數量合併**（ADR 005 的 M5 註記）未做。
