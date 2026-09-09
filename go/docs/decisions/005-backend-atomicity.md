# 005 — Backend 原子性與權限模型

- 狀態：已接受（M1）
- 日期：2026-09-04
- 相關：`internal/backend/`、[docs/format.md §1、§10](../format.md)

## 決策

### 1. 契約寫在 package doc，而不是靠實作的巧合

三條，每個 backend 都要遵守：

1. **讀得到的物件就是完整的。** 部分寫入絕不能出現在最終名稱底下，任何時刻都不行，crash 之後也不行。
2. **物件不就地修改。** 唯一的變更是 Delete，只有維護操作會做。
3. **`PutIfAbsent` 要嘛存進去、要嘛回報 `ErrExists`。**

第一條是「不用鎖也能檢查 repo」的全部根據。`check` 之所以能在別人正在備份的時候跑，就是因為它看到的每個物件要嘛不存在、要嘛完整。

### 2. Local 的 `PutIfAbsent` 用 `os.Link`，不是 `O_EXCL`

寫暫存檔 → fsync → `os.Link(tmp, final)` → unlink tmp → fsync 目錄。

`os.Link` 在 POSIX 上遇到目標存在會原子性地失敗（`EEXIST`），在 NFS 上也成立——這是經典的 lockfile 技巧——在 NTFS 上同樣有效。

**不用 `O_EXCL` 開最終名稱**，即使那看起來更直接：那會先建立真正的名字再往裡面填，中間有一個窗口，而且 crash 之後會在一個「本該不可變」的名字底下留下一個**永久的截斷物件**。那正好違反第一條契約。

`Put`（無條件覆寫，只給 `config` 用）同樣走暫存檔，只是最後用 `os.Rename`。

暫存檔名以 `.tmp-` 開頭，而 key 文法**禁止 segment 以 `.` 開頭**，所以 crash 留下的殘骸不可能被當成物件，`List` 也看不到它。

這裡有一個測試抓到的真 bug：清理用的 `defer` 讀的是具名回傳值 `path`，而錯誤路徑回傳的是 `""`，所以 `os.Remove("")` 什麼都沒刪。`TestLocalFailedPutLeavesNothing` 先紅後綠。

### 3. `PutIfAbsent` 是串流的

第一版是 `PutIfAbsent(ctx, key, data []byte)`。寫 pack 的時候才發現問題：一個 64 MiB 的 pack 不該經過 `[]byte` API，但退回用無條件的 `Put` 更糟——**備份客戶端若能覆寫既有的 pack，光靠自己的憑證就能毀掉整個 repo。**

所以改成 `PutIfAbsent(ctx, key, r io.Reader, size int64)`，另外提供 `PutBytesIfAbsent` 給小物件。S3 的 `If-None-Match: *` 條件寫入本來就支援串流 body，這個介面直接對得上。

`size` 會跟實際讀到的位元組數比對：一個提前結束的 reader 不能默默產生一個截斷物件。

### 4. Key 文法的三個限制

```
segment = (lowercase-alnum / "-" / "_") *( lowercase-alnum / "-" / "_" / "." )
key     = segment *( "/" segment )
```

- **沒有冒號**——Windows 檔名不能有，見 [ADR 004 §4](004-tree-and-naming.md)。
- **segment 不能以 `.` 開頭**——見上面第 2 點。
- 沒有 `..`、沒有空 segment、沒有大寫——路徑穿越與大小寫不敏感檔案系統，一次解決。

這是「檔案系統 ∩ S3 bucket ∩ SFTP server」都接受的交集。key 是由十六進位內容位址和固定前綴組成的，沒有任何合法的東西需要這個集合以外的字元。

### 5. Conformance suite 而不是各寫各的測試

`runConformance` 對著介面測契約，local backend 現在跑它，S3（M2）和 SFTP（M4）之後跑同一套。這樣「符合 Backend 介面」才會一直是同一個意思。

### 6. 權限模型：`PLAN.md` 那句話要修正

`PLAN.md` 說「backup 只需要 Put 權限」。實作之後這句話是錯的，M2 的 IAM 測試要照這張表寫：

| 操作 | Get | List | Put | Delete |
| --- | --- | --- | --- | --- |
| `backup` | `config`、`indexes/*`、`trees/*` | `indexes/`、（M3：`gc/`） | `packs/ indexes/ trees/ snapshots/` | 僅 `gc/*`（M3 的復活步驟） |
| `restore` / `check` | 全部 | 全部 | — | — |
| `prune`（M3） | 全部 | 全部 | `gc/*` | 全部 |

backup 需要 Get 是因為它要讀 config 和既有的 index；需要 List 是因為它要找出有哪些 index blob。

真正的抗勒索性質不是「只有 Put」，是：

> **在資料前綴上沒有 Delete，而且 Put 是條件式的。**

備份角色既不能刪、也不能覆寫。這比「只有 Put」精確，也比「只有 Put」更強——因為無條件的 Put 本身就是一種破壞能力。
