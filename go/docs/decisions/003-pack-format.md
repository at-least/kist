# 003 — Pack 格式與壓縮

- 狀態：已接受（M1，格式凍結）
- 日期：2026-09-04
- 相關：`internal/pack/`、[docs/format.md §4](../format.md)

## 背景

pack 是 chunk 資料唯一的存放處，也是整個 repo 裡唯一會長到 64 MiB 的東西。它的佈局決定了三件事：損壞時能救回多少、index 壞掉時能不能重建、以及還原一個小檔要付多少網路成本。

## 決策

### 1. 從尾端往回讀

```
[sealed chunk]* | sealed trailer | trailer length (8 B BE) | magic (8 B)
```

trailer 放在**尾端**而不是開頭，因為 pack 是串流寫出去的：寫到最後一個 chunk 之前，不知道會有幾個 entry、每個在哪裡。放開頭就得先在記憶體裡組完整個 pack，或是回頭 seek 重寫——後者在 object storage 上根本做不到。

magic 的最後兩個位元組是 big-endian 的版本號。版本同時出現在 magic 和 trailer 的 `v` 欄位，而且**兩者必須相符**——這樣一個 v2 的 trailer 貼到 v1 的尾巴上會被抓到，而不是被當成 v1 解析。

`maxTrailerSize` = 16 MiB：長度欄位是還沒被認證的資料，reader 不能照著它去配置記憶體。實際上一個 64 MiB 的 pack 最多 128 個 entry，trailer 是幾 KB。

### 2. Pack 用**密文**的 unkeyed hash 命名

三個後果，每一個都是想要的：

- **不持金鑰也能驗證完整性。** `check` 的結構檢查、未來的維護角色、甚至一個純粹的儲存端 script，都能確認「這個檔案就是它名字說的那個檔案」。用明文 hash 就做不到。
- **相同的 pack 自動去重。** 兩個 client 同時打包相同內容，`PutIfAbsent` 會告訴第二個「已經有了」，那不是錯誤，是成功。
- **寫入者在寫完最後一個位元組前不知道名字。** 這是代價，也是設計：pack 必須先落到本機 spool 檔再上傳，不能在記憶體裡組。64 MiB × 並發數的記憶體不是值得花的。

### 3. 壓縮：壓下去再比，門檻 1/16

不抽樣估熵。抽樣在「結構化但雜訊大」的資料上兩個方向都會猜錯，而猜錯的成本是**每一次還原都要付**。

保留壓縮結果的條件是 `len(z) < len(p) - len(p)/16`。省不到 6% 就丟掉：那點空間換不到每次讀取都要付的解壓 CPU，而且「只壓縮 1%」的資料通常在下一版編碼器手上會變大。

zstd level 用 `SpeedDefault`（≈ level 3）。備份的瓶頸是磁碟和網路，不是壓縮器。

**演算法位元組放在 AEAD 明文的第一個 byte**，不是放在 trailer。這樣解密後 chunk 就自我描述，trailer 維持乾淨的三欄，而且演算法欄位本身也被認證覆蓋——放在密文外面的話，翻轉它可以讓 reader 對一段合法密文做錯誤的解碼嘗試。

解碼器上限設在 `chunker.MaxSize`。實測（`TestDecompressionBombIsRefused`）：一個 1796 B 的 frame 宣稱解出 16 MiB，會被 `decompressed size exceeds configured limit` 擋下。已確認 `WithDecoderMaxMemory` 對 `DecodeAll` 限制的是**輸出大小**，不是 window size。

### 4. Trailer 的 AAD 是常數，靠別的東西補

pack trailer 與 index blob 的 AAD 是固定字串（`"kist/v1/pack-trailer"`、`"kist/v1/index"`），不是它們的名字。原因是循環：它們的名字是自己密文的 hash，密封的當下名字還不存在。

代價是**A pack 的 trailer 貼到 B pack 上仍然通過認證**。擋住這件事的是兩層，都必要：

1. **一致性檢查**：entry 必須依序、無洞、無重疊、無重複地**恰好鋪滿** chunk 資料區；長度不得小於一個空信封（`overhead + 1`），不得大於 `chunker.MaxSize + 1 + overhead`。用合法金鑰簽出來的爛 trailer 仍然是爛 trailer，而「認證過」不等於「一致」。
2. **pack 以密文 hash 命名**：貼過去名字就對不上，`check` 會抓到。

這個推理寫在這裡，是因為每個看到常數 AAD 的人都會問。

### 5. `VerifyAll` 串流，不整包載入

`check --read-data` 是最可能被指向「幾千個 64 MiB pack」的操作。第一版用 `io.ReadAll` 把整個 pack 讀進記憶體——那是每個 pack 64 MiB × 並發數。改成串流：`io.TeeReader` 一邊餵 hasher 一邊逐 entry 讀進一個 `MaxSize + overhead + 1` 的暫存區。**峰值記憶體是一個 chunk，不是一個 pack。**

順帶得到一件事：pack 的名字是拿整個串流算出來的，不需要第二趟。

### 6. Nonce 由 writer 自己的 XOF 產生

`Seal` 接受呼叫者給的 `io.Reader` 當 nonce 來源。這在測試裡是必要的（沒有固定隨機源就沒有加密物件的 golden file），但也是一個陷阱：傳進一個**不會前進**的 reader，每個 chunk 就會用同一個 nonce，XChaCha20-Poly1305 的保證全部歸零。

解法不是寫註解警告，是讓它做不到：`crypto.NonceStream` 從 32 B 種子展開成 BLAKE3 XOF，每個 `Writer` 持有一個。不管來源給什麼，pack 內的 nonce 一定各不相同。

種子相同的兩個 writer 確實會產生相同的 nonce——那正是 golden file 需要的，而且無害：相同種子、相同金鑰、相同 chunk 會得到逐位元組相同的 pack，那是去重，不是重用。

### 7. 格式與 backend 交界的整數轉換一律檢查

trailer 講的是無號大小，backend API 講的是有號 offset。中間的值來自 pack 自己的 trailer——認證過，但可能是壞掉的 client 寫的。`offsetOf` / `sizeOf` 會檢查而不是假設，所以一個荒謬的大小是一個 corruption 錯誤，不是一個環繞的整數加上一次亂讀。

## 後果

- pack 自我描述，所以 index 永遠可以從 packs 重建，`rebuild-index` 是安全操作。
- 還原一個小檔只抓那個 chunk 的 byte range，不會為了 2 MiB 傳 64 MiB。
- 上傳走 `PutIfAbsent`：備份客戶端**沒有**覆寫既有 pack 的能力。這是 backend 介面從 `PutIfAbsent(key, []byte)` 改成串流版的原因——一個 64 MiB 的 pack 不該經過 `[]byte` API，但也不該退回用無條件的 `Put`。
