# kist

Deduplicating, encrypted backups to object storage, from one binary with no cgo.

> **狀態：M1（本機格式定案）完成，格式已凍結。** S3 後端是 M2，無鎖 GC 是 M3。
> 現在可以用，但只能寫本機路徑，而且還沒有 `forget` / `prune`。

```console
$ export KIST_REPOSITORY=/backup/kist KIST_PASSWORD=...
$ kist init
$ kist backup ~/work
snapshot snapshots/fe2988.../20260904t141955.943279016z
  3 files, 3 directories, 1 symlinks
  3.7 MiB read, 2.9 MiB stored in 1 new packs
$ kist backup ~/work            # 沒改東西
  3.7 MiB read, 0 B stored in 0 new packs
$ kist snapshots
$ kist restore snapshots/fe2988.../20260904t141955.943279016z /tmp/out
$ kist check --read-data
```

## 它做什麼

- **去重**：FastCDC 內容定義切塊（512 KiB / 2 MiB / 8 MiB）。在檔案開頭插入位元組不會重排後面的邊界。
- **加密**：一律加密，沒有明文 repo 這個選項。XChaCha20-Poly1305，金鑰從 Argon2id 派生的階層而來。
- **多機共用一個 repo**：沒有鎖。snapshot 用條件寫入提交，其他所有物件都以內容 hash 命名且不可變。
- **可修復**：index 只是快取，永遠可以從 pack 重建。壞掉的 index blob 不會讓 repo 打不開。
- **會說實話**：跳過的檔案、還原不了的中繼資料、`check` 找到的問題，全部會講出來。`check` 有發現就以非零結束。

## 指令

| 指令 | 說明 |
| --- | --- |
| `kist init` | 建立 repo（密碼問兩次，救不回來） |
| `kist backup <path>...` | 備份並提交一個 snapshot |
| `kist snapshots` | 列出 snapshot，舊的在前 |
| `kist restore <snapshot> <target>` | 還原到一個空目錄 |
| `kist check [--read-data]` | 驗證 repo |
| `kist rebuild-index` | 從 pack trailer 重建 index |

repo 位置：`--repo` 或 `$KIST_REPOSITORY`。
密碼：`--password-file`、`$KIST_PASSWORD`，或終端機提示，依此順序。

## `check` 的兩個層級

它們抓的是不同的東西，誰也不包含誰：

- **預設**（只讀中繼資料與 pack trailer）：抓得到不見的 pack、被截斷的 pack、指向不存在 chunk 的 tree——所有從 repo 的「形狀」看得出來的損壞。
- **`--read-data`**：另外把每個 chunk 讀出來解密驗證。**只有這個層級抓得到 chunk 資料裡被翻轉的位元**，因為別的路徑根本不會去解密那些資料。代價是讀完整個 repo。

## 文件

- [`docs/format.md`](docs/format.md) — 儲存格式 v1（已凍結），含「每個說法由哪個測試守著」的對照表
- [`docs/decisions/`](docs/decisions/) — ADR，記錄為什麼這樣設計
- [`PLAN.md`](PLAN.md) — 里程碑與工程規範

## 開發

```console
$ make verify        # build + vet + lint + test + test-race，這是「做完了」的判準
$ make fuzz          # 跑所有 FuzzXxx target
```

大規模驗收測試（10 萬檔 / 10 GiB，預設關閉）：

```console
$ KIST_ACCEPTANCE=1 KIST_ACCEPTANCE_DIR=/somewhere/with/30GiB \
    go test -v -timeout 180m ./internal/repo/ -run TestAcceptance
```

## 不做的事

不支援非加密 repo、不做 GUI、不自己實作加密原語。
