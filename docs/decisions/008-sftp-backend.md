# 008 — SFTP 後端

- 狀態：已接受（M4）
- 日期：2026-09-04
- 相關：`internal/backend/sftp.go`、[docs/format.md §1、§10](../format.md)、[ADR 005](005-backend-atomicity.md)

## 問題

第三個後端，也是第一個既沒有條件寫入（S3）也沒有本機 `link(2)` 語意保證的。契約不變：讀得到就是完整的、不覆寫、`PutIfAbsent` 真的有條件。`runConformance` 已經定義了契約；問題只是 SFTP v3 能不能老實地做到。

## 先量再設計（OpenSSH 8.4p1，`atmoz/sftp`）

| 操作 | 結果 |
| --- | --- |
| `hardlink@openssh.com`、`fsync@openssh.com`、`posix-rename@openssh.com` | 都有 |
| `Link(tmp, 已存在)` | `SSH_FX_FAILURE`（code 4），**通用錯誤，不是 EEXIST** |
| `OpenFile(已存在, O_EXCL)` | 同上 |
| 純 `SSH_FXP_RENAME` 到已存在 | `SSH_FX_FAILURE`（OpenSSH 不覆寫） |
| `posix-rename` 到已存在 | 成功，覆寫 |
| `Stat` / `ReadDir` 不存在 | `os.ErrNotExist`（pkg/sftp 正規化過） |

`pkg/sftp` v1.13.11 把 `os.O_EXCL` 映射到 `SSH_FXF_EXCL`，但 `Link` 不檢查伺服器有沒有那個 extension。

## 決策

### 1. Spool + hardlink，跟 local 一樣

寫到同目錄的 `.tmp-<random>`（dot 開頭，key 文法保證它永遠不是物件）、有 `fsync@openssh.com` 就 `Sync`、關檔、`Link(tmp, final)`、`Remove(tmp)`。link 是原子的，final 名字底下永遠不會出現半個物件。

### 2. link 失敗後 `Stat`，因為 SFTP 沒有「已存在」

失敗碼是通用的 `SSH_FX_FAILURE`。分類法：link 失敗 → `Stat(final)`；存在就是 `ErrExists`（link 原子，存在即完整）；不存在就回真正的錯誤。寫之前多一次 `Stat` 是給沒改過的 tree 的最佳化——每晚都撞——但守門的是 link，不是那次 Stat。

### 3. 沒有 `hardlink@openssh.com` 就拒絕開啟，不退回 rename

純 `SSH_FXP_RENAME` 在 OpenSSH 上不覆寫，在 pkg/sftp 自己的 server 和其他實作上會覆寫。一個「條件」取決於對面是誰的條件寫入不是條件寫入。錯誤訊息直接說要什麼、OpenSSH 有。

### 4. host key 一定驗，沒有 insecure 選項

`known_hosts`（`~/.ssh/known_hosts` 或 `$KIST_SFTP_KNOWN_HOSTS`）。能被導到攻擊者選的伺服器的備份，就是攻擊者選的備份。

x/crypto 的坑：`known_hosts` 只有 ed25519、client 預設演算法偏好讓伺服器出示 ECDSA → `knownhosts: key mismatch`，跟真的中間人長得一樣。處理：第一次握手失敗且 `KeyError.Want` 非空時，用 `Want` 裡的 key 型別當 `HostKeyAlgorithms` 重試一次；重試仍失敗就回原錯誤。`TestSFTPRefusesAnUnknownHost` 三種情況：不認識 → 錯；型別不同但 key 對 → 過；型別對但 key 錯 → `key mismatch`。這正是 ssh(1) 的行為。

### 5. 認證順序：agent、key file、密碼

`$SSH_AUTH_SOCK` 有就先試；`$KIST_SFTP_KEY`（`$KIST_SFTP_KEY_PASSPHRASE`）；`$KIST_SFTP_PASSWORD`。URL 裡帶密碼直接拒絕——那會進 shell history 和 process list。

### 6. 路徑一律 `path.Clean`

`sftp://host/srv/kist/` 的尾斜線會讓 `List` 用來切 key 的 `TrimPrefix(root+"/")` 什麼都切不掉，於是每個列舉都是空的——repo 看起來完好但空，`rebuild-index` 會把所有 index blob 刪光。advisor 找到的，`TestSFTPListSurvivesAnUncleanRootPath` 先紅後綠。`ParseSFTPLocation` 和 `dialSFTP` 各 clean 一次，手寫的 config 也涵蓋。

## 限制（文件化，沒解）

- `pkg/sftp` 除了 `ReadDirContext` 之外全部忽略 `ctx`：伺服器掛住，kist 跟著掛。
- 持久性只有 `fsync@openssh.com`；協定沒有目錄 fsync。
- 吞吐量（loopback 上的 Docker OpenSSH，`TestSFTPThroughput`，64 MiB pack）：get 424 MiB/s；put 循序時 61 MiB/s，開 `UseConcurrentWrites(true)` 之後 174 MiB/s——所以開了。安全，因為暫存檔關檔 + fsync 之後才 link。真實網路上的數字沒量過。
- 沒有 per-prefix 權限（見 format.md §10）；`remove` 不能列進黑名單。
- `List` 一次讀完整個目錄放記憶體；16 萬個 pack 是幾 MB、幾秒。
