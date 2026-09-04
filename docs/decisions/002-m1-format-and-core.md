# ADR 002：M1 本機格式定案 —— 格式、金鑰、backup 流程

- 狀態：已採用（格式於 M1 結束凍結）
- 日期：2026-09-04
- 里程碑：M1

## 背景

M1 要把「資料在 repo 裡長什麼樣」定下來，之後只能透過 `version` 欄位演進。
這份 ADR 用白話解釋幾個最重要的決定。格式的精確規格在 `docs/format.md`。

## 決定

### 1. 物件名稱 = 密文的 hash，不是明文的 hash

repo 裡的 pack / tree / index 都以「寫進去的那串 bytes 的 BLAKE3」命名。
好處有兩個：

- **不用金鑰就能驗證**：任何人下載後算一次 hash 就知道檔案有沒有被改過。
  `check` 不解密就能先抓出被換掉的物件。
- **名稱不洩漏內容**：hash 的是密文，外人從名稱看不出兩個 repo 是否存了同樣的檔案。

代價是「同樣的目錄要能得到同樣的名稱」這件事必須另外處理（見第 3 點）。

### 2. chunk ID 用 keyed hash，而且永遠不當檔名

chunk 的身分是 keyed BLAKE3（key 從 master key 派生），只存在加密過的 trailer /
index / tree 裡。拿到 repo 的人看不到任何 chunk ID，自然無法用「我猜你有這個檔案」
的方式試探。

### 3. tree 用決定性加密，其他物件用隨機 nonce

「目錄沒變就整棵重用」需要同樣的目錄編出同樣的 bytes。但一般 AEAD 每次用隨機
nonce，密文每次都不同，名稱也就不同。解法：tree 的 nonce 不是隨機，而是
`keyed BLAKE3(nonce key, 壓縮後真正被加密的 bytes)` 的前 24 bytes。

這安全嗎？nonce 重複只有在「同一把 key、同一個 nonce、**不同的輸入**」時才危險。
這裡 nonce 是輸入的函數：輸入相同 → nonce 相同 → 密文完全相同（本來就是我們要的，
沒有多洩漏什麼）；輸入不同 → nonce 不同（撞到的機率等於 hash 碰撞）。nonce key
是祕密，外人無法從 nonce 反推明文的任何事。這是 Kopia 也採用的做法。

一個曾經寫錯、審查時抓到的細節：第一版是從「壓縮前的明文」算 nonce。這在 zstd
換版本（同樣明文壓出不同 bytes）時就變成 nonce 重用。現在改從壓縮後的 bytes 算，
代價只是 zstd 升級後 tree 會重新上傳一次。

其他物件（index、snapshot、pack trailer、每個 chunk）沒有重用需求，維持隨機 nonce。

### 4. 物件種類綁進 AEAD 的 AAD

每個 envelope 的前 32 bytes（magic、版本、種類、壓縮、nonce）就是 AAD。
伺服器端若把一個 index 的 bytes 放到 tree 的位置，解密時 AAD 對不上，直接失敗，
而不是解出一個「看起來像 tree」的東西再出事。chunk 的 AAD 則是它的 chunk ID。

### 5. 大目錄分段、大檔案清單外放

- 一個 tree 最多 10 000 個節點，超過就切段，後一段的 `prev` 指向前一段，
  父目錄記錄最後一段。這樣寫入可以串流（不用把整個目錄的節點留在記憶體），
  而且大目錄只改了尾巴時，前面的段照樣重用。
- 檔案的 chunk 清單超過 256 個時，清單本身當資料存進 pack（`Content::Indirect`），
  tree 裡只留幾個 ID。一個 100 GiB 的檔案不會讓 tree 物件變成幾 MB。

### 6. snapshot key 的時間戳不含冒號

原本打算用 RFC 3339（`2026-09-04T15:04:02Z`），但冒號在 Windows 檔名裡不合法，
而本機後端就是拿 key 當檔名。改用 `20260904T150402442463753Z`：字典序仍等於時間序，
三個平台都能當檔名。snapshot 內容裡的 `time` 欄位仍是 RFC 3339 給人看。

### 7. backup 的寫入順序與 parent 快速路徑

寫入順序固定 packs → trees → index → snapshot。snapshot 是唯一的 commit point：
它出現之前 repo 裡多出來的東西都是垃圾，GC 可以收；它出現之後，它引用的一切都在。

同一台機器、同一組路徑的上一個 snapshot 當 parent：檔案的 size 與 mtime 都沒變，
就直接沿用它的 chunk 清單，不重讀檔案。這是第二次 backup 快的主因；即使快速路徑
失效（例如只碰了 mtime），chunk 層級的去重仍保證不寫新 pack。

### 8. CPU 工作全部在 blocking thread

切塊、hash、壓縮、加密都在 `spawn_blocking` 裡跑；async 端只負責 I/O 與排程。
pack buffer 與 index 在檔案之間「移進 closure 再移出來」，不用鎖，也不用複雜的 lifetime。

### 9. 使用者可見文字用英文，註解與文件用中文

`kist` 是開源工具，錯誤訊息與 `--help` 用英文；程式碼註解、ADR、格式文件用中文，
因為讀的人是專案負責人。

## 沒做（留給之後的里程碑）

- uid/gid 的還原（需要 root）、xattr、hardlink：格式已留欄位空間（解碼忽略未知欄位）。
- index 的本地 mmap 快取（M2）；目前每次開 repo 把所有 index 讀進記憶體。
- 額外 key slot（`keys/<id>`）的命令；格式已定義。
- Windows 上 symlink 的還原需要特權，失敗只警告。
