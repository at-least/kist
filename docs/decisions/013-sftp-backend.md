# ADR 013：SFTP 後端（russh + openssh-sftp-client，語意對齊 Go 參考實作）

日期：2026-09-07　狀態：已採用

## 背景

M4 收尾項：「SFTP 後端（Go 參考實作有可對照的實作）」。kist 的定位是 object storage
優先、其次本機與 SFTP；很多使用者唯一能架設的「自架伺服器」就是一台開 SSH 的機器。

`Backend` 把 `object_store::ObjectStore` 收斂成九個操作，所以 SFTP 後端 = 實作
`ObjectStore` trait，既有程式零改動即可用上。需要滿足的合約（`tests/contract.rs`，
M3 的 GC 安全性依賴它）：原子寫入、`put_if_absent` 對已存在 key 失敗且不動原物件、
`list`/`head` 的 `modified` 一致（一律取整到秒）、冪等刪除。

## 決策

### 1. 架構：russh（SSH 傳輸＋認證）＋ openssh-sftp-client（SFTP 協議層）

評估過三條路：
- **系統 ssh（openssh crate）＋ openssh-sftp-client**：認證全交給系統 ssh 最穩，
  但**沒辦法非互動輸入密碼/passphrase**（`KIST_SFTP_PASSWORD` 的 parity 直接消失），
  且多了一個外部執行檔依賴。
- **russh ＋ russh-sftp**：全 in-process，但 russh-sftp 沒有寫入管線化（64 MiB pack
  在 WAN 上會慢一個量級），也缺 posix-rename 擴充。
  **2026-09-17 補記**：上面評的是 2.4.0；russh-sftp **3.0.0**（2026-09-08，本 ADR
  隔天發佈）讀原始碼確認：寫入有管線化（`Config::max_concurrent_writes` 預設 16，
  `poll_write` 維持 in-flight ack 佇列）、有 `hardlink`／`fsync`／`limits` 擴充的
  偵測與呼叫、有 O_EXCL（`OpenFlags::EXCLUDE`）、建構子接受任意
  `AsyncRead + AsyncWrite` stream（russh channel 與 rclone stdio 都接得上）。仍缺
  `posix-rename` 包裝，但 `RawSftpSession::extended()` 公開，自己送
  `posix-rename@openssh.com` 即可。**未實測**（吞吐、對端 EOF、multi_thread 收尾）。
  暫不遷移：openssh-sftp-client 的懸掛 bug 已找到根因並提交一行修法（見 ADR 014
  上游怪癖段落）；若上游不採納或再踩坑，3.0.0 是可行替代，遷移前需先 probe EOF／
  multi_thread 行為並重跑全套合約測試。
- **russh ＋ openssh-sftp-client（採用）**：openssh-sftp-client 吃任意
  AsyncRead+AsyncWrite stream（文件明言支援任何 SSH 庫當傳輸），同時具備管線化寫入、
  `hardlink`／`posix-rename`／`fsync` 三個 OpenSSH 擴充的偵測與呼叫。

注意 Windows：`russh` 必須用 `default-features = false, features = ["ring"]`——
預設的 `aws-lc-rs` 會拉 aws-lc-sys（Windows 要 CMake＋NASM，正是 ADR 003 選
rustls-ring 時特意避開的）。SSH agent（`$SSH_AUTH_SOCK` 的 Unix socket）是
Unix-only，`#[cfg(unix)]` 閘掉。

### 2. 寫入語意（對齊 Go 參考實作 `internal/backend/sftp.go`）

寫入一律：同目錄暫存檔 `.tmp-<16hex>`（O_EXCL）→ 寫入 → `fsync`（伺服器支援時）→
- `put`（覆蓋）：`posix-rename@openssh.com` 原子換名；
- `put_if_absent`：`hardlink@openssh.com`。link 對已存在目標會失敗，這就是條件寫入的
  原子保證；SFTP 沒有「已存在」狀態碼（OpenSSH 回泛用 FAILURE），link 被拒後用
  stat 判別是「已存在」（→ AlreadyExists）還是別種錯誤。**另有 stat 快路徑**：
  已存在的 key 直接早退，省下整份上傳（每晚對未變 tree 重複上傳的常見案例）。

伺服器缺 `hardlink@openssh.com` 或 `posix-rename@openssh.com` → 連線時直接拒絕並
說明（fallback 的 rename 在不同伺服器上有不同語意，條件寫入不能建立在「看伺服器
心情」的基礎上）。`fsync@openssh.com` 有就同步、沒有就略過。

### 3. 安全性：host key 嚴格驗證，不做 TOFU

只接受 known_hosts（預設 `~/.ssh/known_hosts`，`KIST_SFTP_KNOWN_HOSTS` 可指定）裡
記錄的 key；不在裡面一律拒連，錯誤訊息帶檔案位置。TOFU（第一次就信）會把中間人
直接升級成你的備份伺服器。另外連線前先從 known_hosts 讀該主機記錄的 key 類型做
**演算法預選**——OpenSSH 只從客戶端提案裡挑，不預選的話，known_hosts 只記了
ed25519 的主機可能一直收到 ECDSA key 而被自己拒絕。Go 版用「撥兩次」解同一件事；
russh 可以先讀 known_hosts 再定演算法，一次撥接完成。主機完全沒記錄時給一組保守
標準演算法（ed25519 優先），讓連線走得到 host key 檢查、錯誤訊息才會是
「不在 known_hosts 裡」而不是演算法協商失敗。

### 4. 認證順序（環境變數名對齊 Go）

`$SSH_AUTH_SOCK` agent → key 檔 `KIST_SFTP_KEY`（＋`KIST_SFTP_KEY_PASSPHRASE`）→
密碼 `KIST_SFTP_PASSWORD`。全缺則明確報錯。URL 裡帶密碼（`user:pass@`）直接拒絕：
密碼會進 shell 歷史與 process 清單。key 檔**讀不到或解析失敗**時直接報錯、不降級
到密碼——與 Go 版一致：使用者明確指定的金鑰素材壞掉是設定錯誤，安靜地換下一個
方法會把問題藏到更難查的地方。

### 5. 測試

- `tests/sftp-setup.sh`：atmoz/sftp 容器（OpenSSH sftp-server，三個擴充都有），
  keyscan 產 known_hosts，公鑰用官方機制掛載進 `.ssh/keys/`（entrypoint 開機時
  生成 authorized_keys；docker cp 事後寫會踩 owner 不符的 StrictModes 地雷）。
- `tests/contract.rs` 加 `sftp_backend_meets_the_contract`（合約的第三個後端，
  檔頭當初就預留了）。
- `tests/sftp.rs`：密碼認證 roundtrip＋put_if_absent 已存在不換內容、key 認證
  （不給密碼，證明自己成立）、未知 host key 拒連（known_hosts 放一把格式正確的
  「錯 key」）、無認證素材時的錯誤訊息。
- `tests/url.rs`：`sftp://` 解析（user/port/IPv6/Display 往返/拒絕密碼進 URL）。

## 驗證

- `cargo test --workspace`：238 passed（含 SFTP 合約與四個專屬測試；沒設環境變數時
  SFTP 測試自動跳過）。
- CLI 端到端（對容器）：`init` → `backup`（128 KiB/2 chunks）→ 二次 backup
  `new: 0 B` → `snapshots` → `restore` → `diff -r` 逐 byte 相同；key 認證同流程；
  `prune --dry-run`、`check` 無錯。
- 五個發佈 target（`./dist.sh`）全部重建成功——russh 對 windows-gnu/musl/macos
  都編得過（agent 認證 `#[cfg(unix)]` 閘掉後）。

## 實測（2026-09-07，docker 容器 = 遠端，loopback）

- **全量備份 551 MiB**（249 chunks / 9 packs）：7–14 秒 ≈ **40–79 MiB/s**，
  含 chunk/hash/加密/SFTP 傳輸全程；對照本機後端 page-cache 全熱時約 1 秒。
  loopback 數字無法代表 WAN（WAN 是延遲主導，管線化的效益在那裡才會放大），
  但足以證明沒有粗糙的每請求來回瓶頸（Go 版未開 concurrent writes 時
  loopback 為 61 MiB/s，同一量級）。
- **二次備份（dedup）**：< 1 秒（0 chunks / 0 packs）。
- **兩階段 GC 全流程**：backup → forget（keep-last 1）→ prune 標記 →
  再 backup（抗競態：client 在標記後要有新 snapshot）→ prune 刪除
  `deleted 2 object(s) (8.9 KiB)`，伺服器端物件數 19 → 15（含 marker 清理），
  `check` 無錯。預設 `--clock-skew 1h` 會把刪除擋住一小時，是設計行為
  （時鐘偏快的 client 不得虛報「標記後有新 snapshot」）。
- **暫存檔殘骸**：全部操作後 `.tmp-*` 為 0。
- **restore + `diff -r`**：550 MiB 逐 byte 相同。

## 沒做 / 已知限制

- **斷線不重連**：一個 `Backend` 一條 SSH 連線，斷了之後的操作回錯；CLI 的一次性
  命令與 daemon 的每次 job 重新開後端，影響有限。要長連線心跳與自動重連再議。
- **list 全量在記憶體**：SFTP 沒有分頁；與 Go 版同一取捨（16 萬 packs ≈ 數 MB）。
- `get_ranges` 整讀再切片（物件 ≤ 128 MiB，換直白）；`copy_opts` 也是。
- 沒做 SSH 憑證（certificate）host key——known_hosts 驗證語意不涵蓋，遇到直接拒。
- 沒做壓縮連線、port forwarding 等與備份無關的 SSH 功能。
