# ADR 014：rclone 橋接（`rclone://`，stdio 橋接 + 寬鬆條件寫入）

日期：2026-09-07　狀態：已採用

## 背景

使用者問「有支援 rclone 嗎」。kist 的 `sftp://` 後端（ADR 013）需要伺服器真的實做
`hardlink@openssh.com` 與 O_EXCL 建檔，而實測發現 rclone 的 `serve sftp` **宣稱支援
這兩個擴充、執行卻回 OpUnsupported**——kist 連不上它的寫入路徑。但 rclone 橋接是
restic 生態裡最受歡迎的做法之一：`rclone serve sftp --stdio` 讓工具自己 spawn 一個
子程序、SFTP 走 stdin/stdout pipe，於是任何 rclone 設定好的遠端（Google Drive、
OneDrive、B2、S3、WebDAV…約 70 種）都能當儲存體，不需要開 TCP port、不需要管
host key（rclone 每次啟動重生 host key，strict known_hosts 根本無法用 TCP 模式）。

## 實測（rclone v1.75.0，`serve sftp`，本機目錄後端）

| 操作 | 宣稱 | 實際 |
| --- | --- | --- |
| `create_new`（O_EXCL）建檔 | — | **OpUnsupported** |
| `hardlink@openssh.com` | 擴充清單有 | **OpUnsupported**（OpenSSH sftp CLI 的 `ln` 直接失敗） |
| `posix-rename@openssh.com` | 擴充清單有 | **可用**，覆蓋既有目標成功 |
| create+truncate 建檔 | — | 可用 |
| `fsync@openssh.com` | 無 | 沒有（kist 本來就會略過） |

結論：rclone 上唯一可用的 publish 原語是 posix-rename，**沒有任何原子條件建立**。

## 決策

### 1. 兩個 scheme、兩份語意合約

- `sftp://` **維持嚴格語意不變**（hardlink 守門）。對宣稱支援 hardlink 卻在執行時
  拒絕的伺服器（rclone 就是），錯誤訊息明確指向 `rclone://` 橋接，不再是莫名其妙的
  generic 錯誤。
- `rclone://<remote>/<path>` **明示 opt-in 寬鬆語意**：選這個 scheme 就是同意妥協。
  remote 留空（`rclone:///srv/backups`）= rclone 的本機檔案系統。kist spawn
  `rclone serve sftp --stdio <remote>:<path>`（`KIST_RCLONE_BIN` 可指定路徑），
  `kill_on_drop` + sftp 先 drop（關 stdin 讓 rclone 收尾）。rclone 的 stderr 收進
  4 KiB 尾巴緩衝：不清會塞爆 pipe 卡死協議；收起來才能在啟動失敗（remote 名字打錯
  等）時把 rclone 自己的錯誤訊息回給使用者。remote 名用**白名單**驗證（字母數字
  `-` `_`、不以 `-` 開頭）：remote 與 path 合成一個 argv 元素，而 rclone 的旗標
  解析穿插在位置參數之間，`--password-command=…` 這種「remote」在加密設定檔下會
  被當成 rclone 旗標、其值被 shell 執行（審查指出的注入面），白名單直接封死。
  子程序的 kill 只影響 drop 當下還在飛的請求；kist 的每個操作都等伺服器 ACK 才算
  完成，CLI 正常結束時沒有這種請求。

### 2. 寬鬆模式的寫入語意

- 暫存檔：O_EXCL 仍是第一選擇，**只在 OpUnsupported 時**退回 create+truncate
  （名稱是 64 位元隨機數，撞名機率可忽略；最終 publish 仍是原子的 posix-rename）。
  每個連線只警告一次。統一兩個 scheme 走同一條碼路：哪天 rclone 實做了 create_new，
  橋接模式自動升級。
- `put`（覆蓋）：不變，posix-rename（rclone 真的會動）。
- `put_if_absent`：hardlink 守門退化成「先 stat 再 posix-rename」。race 視窗 =
  兩個寫入者同時通過 stat，後 rename 的贏（檔案仍是原子的完整內容）。

### 3. 為什麼這個妥協可接受（逐 put_if_absent 呼叫點）

| 呼叫點 | key 語意 | race 的最壞結果 |
| --- | --- | --- |
| GC 標記（prune.rs） | 內容固定 magic bytes | 同 key 必同內容，race 無害 |
| parity（backup.rs） | content-addressed | 同 key 必同內容，race 無害 |
| snapshot commit（repo.rs） | `snapshots/<client>/<ts>` | 同 client、同秒、同時 backup：後寫的贏，先寫的消失；最終狀態仍是一個合法 repo |
| config（repo.rs init） | repo 唯一可覆寫物件 | 兩個 init 同時跑：輸家的金鑰對不上 → 之後操作報解密錯誤 |

前兩個本來就安全；後兩個用**一次性的讀回驗證**緩解：init 的 config 寫入改走
`Backend::put_if_absent_verified`（寫入成功後重讀比對，不一致 → `ConcurrentWrite`
錯誤）。讀回攔得住大多數交錯，但**不是鎖**——對方的 rename 若落在自己讀回之後，
兩邊都會回報成功；要把殘餘視窗歸零需要後端不具備的原子原語，這正是寬鬆模式的
定義。snapshot 的 race 不做讀回（兩個結果都是合法 repo，讀回救不回也無意義）；
它需要「同一台機器同一秒同時 backup」，而同一台機器本來就有 client id 鎖，實際上
是另一台機器仿冒同一 client id 才會踩到。嚴格後端上讀回只是便宜的確認（hardlink
守門本來就保證一致）。

### 4. 不做的事

- **不做自動降級**：任何伺服器 hardlink 壞掉就自動切寬鬆——拒絕。`sftp://` 的合約
  不該被伺服器行為悄悄改寫；要寬鬆請明確選 `rclone://`。
- **不支援 `rclone serve s3`**：實測 PUT 被接受但**靜默不落盤**（立即 NotFound），
  且 rclone 自己的 ListBuckets XML 都解析不了——rclone 的實驗品，不是 kist 能修的。
- **`rclone mount` + 本機後端**：不安全（非原子的可覆寫檔案系統 + FUSE 快取）。

## 驗證

- `crates/kist-backend/tests/rclone.rs`（`KIST_TEST_RCLONE=1`）：stdio 往返（put/
  overwrite/put_if_absent/range/list/delete）、同連線連續併發操作、bogus remote
  錯誤附 rclone 原訊息；`rclone_bin.rs`：缺 binary 時錯誤指名 `KIST_RCLONE_BIN`。
  這些測試同時是 hardlink/posix-rename「宣稱 vs 實際」的執行驗證。
- `tests/verified.rs`：config 讀回驗證——竄改讀取內容的包裝 store 觸發
  `ConcurrentWrite`；一致與 AlreadyExists 路徑不受影響。
- `tests/rclone-setup.sh`：確認 rclone 在 PATH 並 export `KIST_TEST_RCLONE=1`
  （stdio 模式不用起伺服器）。
- CLI 端到端（rclone:// + 本機目錄 remote）：init → backup ×3（dedup：第二輪僅
  11 B 新增）→ snapshots → restore（`diff -r` 完全相同）→ `check` 全綠 →
  forget + `prune --grace 1s` 兩階段：標記 2 物件 → 「2 held back by active
  clients」（anti-race 規則）→ 補一輪 backup 後真刪除（deleted 2 objects、gc 標記
  清空、零 `.tmp-*` 殘骸）→ 最終 `check` + restore 一致。
- `sftp://` 嚴格路徑不變：docker OpenSSH（atmoz/sftp）合約測試與認證測試全數通過；
  `sftp://` 打 rclone 得到明確的「use the rclone:// bridge」錯誤。

## 限制與備註

- 寬鬆語意只存在於 `rclone://`；`check`、GC、加密與 on-disk 格式完全不受後端影響。
- 遠端若不支援 rclone 的 VFS hardlink（多數雲端），行為與本機目錄 remote 相同——
  寬鬆模式本來就不用 hardlink。
- S3 相容的目標（B2、R2、Wasabi、MinIO…）有 S3 endpoint 時優先用原生 `s3://`
  （完整嚴格語意）；`rclone://` 的價值在**沒有** S3 endpoint 的遠端。
- **上游怪癖（以 select! 繞過；已回報上游
  [openssh-sftp-client#183](https://github.com/openssh-rust/openssh-sftp-client/issues/183)）**：
  openssh-sftp-client **0.15.8** 的
  `Sftp::new` 在 tokio **multi_thread** runtime 上，對端在版本交換前就退出
  （stdout EOF）**不會**喚醒等待中的版本交換——future 無限懸掛；current_thread
  runtime 不受影響（~25 ms 正常回 EOF 錯誤）。最小重現（與 kist 程式無關）：

  ```rust
  #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
  async fn repro() {
      let mut child = tokio::process::Command::new("rclone")
          .args(["serve", "sftp", "--stdio", "--log-level", "ERROR", "no-such:x"])
          .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped())
          .kill_on_drop(true).spawn().unwrap();
      let sftp = openssh_sftp_client::Sftp::new(
          child.stdin.take().unwrap(), child.stdout.take().unwrap(), Default::default());
      sftp.await.unwrap(); // multi_thread：永久懸掛；current_thread：立即 Err(EOF)
  }
  ```

  kist 的繞法：`tokio::select!` 同時等三件事——版本交換完成、`child.wait()`
  （rclone 退出立即觸發；`Child::wait` 是 cancel safe）、30 秒保險絲（kill + wait）；
  三個分支都回帶 stderr 尾巴的明確錯誤。select 站點的程式註解指向本節，別把
  wait()/保險絲兩臂「簡化」掉。**已知限制**：保險絲只蓋啟動階段；會話中途 rclone
  死亡的行為未完整驗證，觀察到後續操作可能短暫假成功（請求排進死管線）後才失敗。
