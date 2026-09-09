# 002 — 內容定義切塊：自己實作 FastCDC

- 狀態：已接受（M1）
- 日期：2026-09-04
- 相關：`internal/chunker/`、`PLAN.md` 技術選型、[001](001-project-skeleton.md)

## 背景

切塊邊界決定了「哪些位元組會被視為同一個 chunk」，也就決定了去重能不能跨備份、跨機器成立。它不是效能參數，它是**格式的一部分**：兩個版本的 kist 若在同一份檔案上切出不同邊界，寫進同一個 repo 時彼此完全去重不到。

`PLAN.md` 的選項是「`github.com/jotfs/fastcdc-go`，或自行實作」，參數固定為 min 512 KiB / avg 2 MiB / max 8 MiB。

## 決策

**自己實作 FastCDC**，`internal/chunker/chunker.go` 約 80 行加一張 256 項的 gear table。

### 為什麼不用 fastcdc-go

兩個理由，第二個才是真正的理由。

**一、它在建構子裡改全域狀態，而且是 data race。** `fastcdc.NewChunker` 會做 `table[i] = table[i] ^ opts.Seed`（fastcdc.go:110），對一張 package-level 的 `[256]uint64` 做 read-modify-write。實測（`go test -race`，scratchpad 的最小重現）：

```
WARNING: DATA RACE
Read at 0x0000007f51a0 by goroutine 13:
  github.com/jotfs/fastcdc-go.NewChunker()
      fastcdc.go:110
Previous write at 0x0000007f51a0 by goroutine 16:
  github.com/jotfs/fastcdc-go.NewChunker()
      fastcdc.go:110
```

一開始的處理是在 `New` 外面加一把 mutex。**那不夠**：接著在自己的並發測試上又跑出第二種 race，`nextChunk` 在切塊時**讀**同一張表，跟另一個 goroutine 的 `NewChunker` **寫**撞在一起：

```
Read at 0x000000875900 by goroutine 42:
  github.com/jotfs/fastcdc-go.(*Chunker).nextChunk()
      fastcdc.go:197
Previous write at 0x000000875900 by goroutine 48:
  github.com/jotfs/fastcdc-go.NewChunker()
      fastcdc.go:110
```

要修就得是 RWMutex：`New` 拿寫鎖、`Next` 拿讀鎖。以我們的 seed = 0 來說，那個「寫」寫回去的是同一個值，所以**實際行為是正確的**，壞的只有 `-race`。但 `make test-race` 是 `make verify` 的一部分，而備份本來就會同時切很多檔案，這在正式環境會一直觸發。

**二、邊界函式屬於凍結格式，不該由別人的 patch release 決定。** 這才是決定性的理由。上面那個 race 遲早會有人修，而修法之一（例如把 seed 移到 Chunker 內、或改寫 `nextChunk`）就可能改掉邊界。到那時我們的使用者會在某次 `go get` 之後發現增量備份突然寫了一整份新資料，而且沒有任何錯誤訊息。一個「儲存格式凍結」的專案，不能把格式的定義權交給一個 v0.2.0 的相依。

RWMutex 方案是可行的，也便宜（每 ~2 MiB 一次鎖）。拒絕它的理由不是效能，是所有權。

### 等價性怎麼證明的

不是宣稱，是跑出來的。移除相依**之前**，寫了一個 `TestMatchesFastCDCGo`，把兩份實作跑在九組輸入上，比對 `(offset, length)` 序列：

```
=== RUN   TestMatchesFastCDCGo/empty
    0 chunks identical over 0 bytes
=== RUN   TestMatchesFastCDCGo/one_byte
    1 chunks identical over 1 bytes
=== RUN   TestMatchesFastCDCGo/below_minimum
    1 chunks identical over 524287 bytes
=== RUN   TestMatchesFastCDCGo/at_minimum
    1 chunks identical over 524288 bytes
=== RUN   TestMatchesFastCDCGo/above_minimum
    1 chunks identical over 524289 bytes
=== RUN   TestMatchesFastCDCGo/three_averages
    4 chunks identical over 6291456 bytes
=== RUN   TestMatchesFastCDCGo/twenty_MiB
    10 chunks identical over 20971520 bytes
=== RUN   TestMatchesFastCDCGo/low_entropy_runs
    3 chunks identical over 25165824 bytes
=== RUN   TestMatchesFastCDCGo/all_zeroes
    4 chunks identical over 31457280 bytes
--- PASS: TestMatchesFastCDCGo (0.10s)
```

而且 `testdata/boundaries.txt`（64 MiB 決定性輸入的所有邊界）在換掉實作前後，資料列完全相同，只有標頭註解從 `normalization=2 seed=0` 改成 `maskS=0x7fffff maskL=0x7ffff`：

```
$ diff <(grep -v '^#' boundaries.before.txt) <(grep -v '^#' boundaries.txt)
IDENTICAL BOUNDARIES
```

證明跑完之後，`TestMatchesFastCDCGo` 和相依一起刪掉。留下來的長期保護是兩個 golden：`testdata/boundaries.txt`（邊界本身）與 `TestGearTableDigest`（那張表的 BLAKE3 摘要）。

### 移植時刻意保留的「怪處」

這些看起來像 bug，但它們定義了邊界，改掉就是改格式：

- `if len(data) <= MinSize { return len(data) }` — 尾端不足最小值時整段一塊。
- gear hash 每個 chunk 從 `fp = 0` 重新開始，而且**前 MinSize 個位元組完全不進 hash**，從 `i = MinSize` 才開始。
- mask 由 `bits = round(log2(AvgSize)) = 21` 推出：`maskSmall = 1<<23 - 1`、`maskLarge = 1<<19 - 1`。寫成常數而不是執行期 `math.Log2`——不同平台的浮點捨入差異足以無聲地分裂格式。
- 緩衝區 `2 * MaxSize`，且 `fill` 保證游標後永遠有 ≥ MaxSize 位元組（除非輸入用盡）。這是「邊界不受 reader 給多少位元組影響」的來源，由 `TestBoundariesDoNotDependOnReadSizes`（用 `iotest.OneByteReader`）守住。

gear table 是用 `sed` 從 module cache 機械抽出來的，不是手打的，抽法寫在 `gear.go` 的註解裡。

## 後果

- `internal/chunker` 沒有第三方相依，`Chunker` 之間不共用任何狀態，並發切塊不需要協調。
- 邊界要改，只能改 `MinSize`/`AvgSize`/`MaxSize`/mask 或 gear table，而這四樣都有 golden 擋著，改動一定會讓測試變紅。
- 代價：FastCDC 上游若有演算法層級的改良，我們不會自動拿到。以「格式凍結」的專案來說這是對的取捨——真要升級，那是一次帶 format version 的明確演進，不是一次 `go get`。
