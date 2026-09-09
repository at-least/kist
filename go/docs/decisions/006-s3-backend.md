# 006 — S3 後端與無鎖並發

- 狀態：已接受（M2）
- 日期：2026-09-04
- 相關：`internal/backend/s3.go`、[docs/format.md §10](../format.md)、[005](005-backend-atomicity.md)

## 決策

### 1. `PutIfAbsent` = `PutObject` + `If-None-Match: *`

服務端遇到 key 已存在回 412，變成 `ErrExists`。兩個條件寫入同時撞上可能拿到 409 `ConditionalRequestConflict`，退避重試最多 5 次——之後其中一個一定會看到成功或 412。

body 必須是 `io.ReadSeeker`：重試要重播 body，SDK 自己也會在重試間 rewind。kist 上傳的東西不是 `*os.File` 就是 `bytes.Reader`，所以非 seekable 的 body 是程式錯誤，直接拒絕。

### 2. 錯誤分類同時看 S3 error code 和 HTTP status

S3 相容服務對「該送哪一個」意見不一。實測 MinIO RELEASE.2025-09-07（`TestS3ProbeErrorShapes`，跑完即刪，結果留在這裡）：

```
conditional put on existing  code=PreconditionFailed  status=412
range past end               code=InvalidRange        status=416
head absent                  code=NotFound            status=404
get absent                   code=NoSuchKey           status=404
```

四種情況 code 與 status 都能對上，分類器兩邊都查。

`S3.List` 一開始**沒有**走分類——IAM 測試抓到的：backup client 對 `packs/` 做 List 拿到 403，但錯誤裡沒有 `ErrDenied`。現在每個呼叫都走 `wrap`。

### 3. Range 的兩個邊角

S3 拒絕空 range（`bytes=5-4`），對超出物件尾端的 range 回 416 而不是空 body。本機檔案系統兩者都給空結果。這兩種情況 caller 的意思都是「沒有東西」，所以在確認物件存在後直接回空 reader。conformance suite 的 `{3, 0, ""}` 與 `{10, ReadToEnd, ""}` 兩個案例守著。

### 4. 關掉 checksum trailer

SDK 預設對每個上傳加 CRC checksum trailer，這會把單純的 PUT 變成 aws-chunked body，而「aws-chunked + 條件 header」在 S3 相容服務上是已知的地雷區。kist 的每個物件本來就以自己的 hash 命名，傳輸層 checksum 沒有增加任何東西。

### 5. 無鎖並發：實測結果

`TestS3ConcurrentBackups`，兩個 client 同時對同一 prefix 備份同一份資料，跑 3 輪，`-race` 下：

- 第三個 client `check --read-data` 乾淨，兩個 snapshot 都還原成逐位元組相同。
- 第三個 client 再備份一次：**0 個新 chunk**——兩個 client 的 index blob 正確合併了。
- **每輪存了 2 個 pack。** 相同明文、不同 `NonceStream` 種子、不同密文、不同 pack ID。這是「無鎖 + 隨機 nonce」的代價，Kopia 也付同樣的錢。`rebuild-index` 不會合併它們，只有 M3 的 `prune` 會（作為未被引用的重複）。

### 6. 抗勒索性質在哪裡成立

見 [format.md §10](../format.md)。摘要：「資料前綴上沒有 Delete」兩個服務都由 IAM 保證；「Put 是條件式的」在 AWS 上可以用 `s3:if-none-match` 條件鍵變成儲存端性質（UNVERIFIED，沒有 AWS 帳號），在 MinIO 上被拒絕（`invalid condition key`），只對誠實 client 成立。

搜尋時看到一則 2024 年的 MinIO issue 說它不支援 `If-None-Match: *`。**這已經不是事實**：conformance suite 在 RELEASE.2025-09-07 上以 412 通過。相信實測，不相信舊 issue。

### 7. Object Lock 不是 403

寫進 `S3.Delete` 的註解和 format.md §10。M3 的 `prune` 設計要從這裡開始，不能假設「刪不掉會報錯」。

## 後果

- `s3://bucket[/prefix]` 是合法的 repo 位置；端點與 path-style 從 `KIST_S3_ENDPOINT` / `KIST_S3_PATH_STYLE` 來，憑證走標準 `AWS_*`。
- `make test-s3` 在 Docker 裡起 MinIO 跑 conformance、並發、IAM 三組測試；CI 在 Linux 跑同一個 target。
- UNVERIFIED 清單：真 AWS S3（只測過 MinIO）、`s3:if-none-match` 的實際執行、Object Lock 行為。
