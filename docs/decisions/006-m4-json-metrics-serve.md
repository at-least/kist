# ADR 006：M4 —— `--json` 輸出、Prometheus metrics、`kist serve`

日期：2026-09-05。狀態：已採用。

## 背景

M4 的「可用性」里程碑裡，除了設定檔與排程（M4-a 已完成），還有兩件事是給機器看的：
腳本要能消費命令結果（`--json`），監控要能抓工作狀態（Prometheus metrics）。
PLAN 也指定了 Web UI 的技術（axum + htmx + askama），它需要一個常駐的 HTTP 端口——
這個端口先拿來服務 `/metrics`，Web UI 之後長在同一個 router 上。

## 決定

### 1. `--json` 是全域旗標，輸出核心型別的 serde 形狀

- 旗標放在根命令（`global = true`），`kist snapshots --json` 與 `kist --json snapshots` 都行。
  每個命令自己宣告的話，會出現十個語意相同、細節不同的旗標。
- stdout 只放結果 JSON（pretty，方便人也讀）；日誌與錯誤照舊走 stderr；結束碼不變
  （0 / 1 / 3）。JSON 消費者用結束碼判斷成功與否。
- 核心的 summary / report 型別在 M4-a 已經加上 `Serialize`（webhook 先用了），`--json`
  直接序列化它們，不另做一份「輸出格式」：同一個結果，webhook、metrics、CLI 三個消費者。
  例外是三個包裝（見下）。
- `init` / `version` 沒有值得機器消費的結果，不支援 `--json`。

三個包裝（都在 CLI 層，動的只是輸出形狀）：

1. **id 在 JSON 是 hex**：`kist-format` 的 id 型別序列化原本固定是 bytes（CBOR 對），
   JSON 會變成一串數字陣列，沒人讀得懂。改為 `is_human_readable()` 時輸出 hex 字串：
   serde_json 是 false→bytes、true→hex，repo 格式（CBOR）完全不動，golden tests 守著。
2. **snapshot 路徑**：格式裡是 OS bytes（為了支援非 UTF-8 檔名），JSON 必須是合法 UTF-8，
   以 lossy 轉換（非法位元組 → U+FFFD），並在 CLI 定義輸出結構把 key 拆出
   client / timestamp 欄位。
3. **`forget` 的 kept**：內部是 `(key, reasons)` tuple，序列化成 `[key, [reasons]]`，
   位置語意太脆弱；包成 `{snapshot, reasons}`。`forget` / `prune` 的輸出另加 `dry_run`
   欄位——「removed」是已刪還是會刪，消費者光看欄位名分不出來。
4. **`forget --prune --json`**：stdout 只能有一個 JSON 值，所以包成
   `{"forget": ..., "prune": ...}`，不輸出兩份文件。
5. **`--json` 沒有覆蓋 `init` / `version`**：它們的結果沒有機器消費的價值，維持文字。

### 2. `kist serve` = 排程 daemon + HTTP，與 `kist run` 並存

`serve` 與 `run` 共用 `Daemon`，差別只在有沒有 HTTP 端口。不合併成 `run --http`：
兩者注定分歧（serve 之後要有 Web UI、認證、可能 TLS），塞進 `run` 會污染旗標空間、
耦合生命週期。daemon 的建構（config → Daemon → 列下次執行時間 → ctrl-c → run）兩邊一致，
命令層各只有幾行。

路由只有 `GET /metrics`、`GET /healthz`、`GET /`（指路）。axum 放在 `kist-app`
（跟 daemon、metrics 同層），CLI 只是薄薄一層。HTTP server 半路掛掉（accept 錯誤等）時，
watcher task 記下錯誤並發 shutdown 讓 daemon 把目前的工作做完，然後以錯誤結束整個 process——
不能讓 process 帶著一個死掉的 `/metrics` 繼續跑，那樣 monitoring 只會看到 scrape 失敗，
backup 卻還在跑，兩邊講的事不一致。

### 3. metrics 內容與持久化

- **在記憶體的部分**（runs counter、最後一次的 gauge）重啟歸零是標準做法：歷史在
  Prometheus 端，工具自己不存時間序列。
- **例外是 `job_last_success_timestamp_seconds`**：持久化到
  `cache_dir/jobstate-<repo hash>-<job>.json`（一個 repo + 一種工作一個檔，內容只有一個秒數，
  暫存檔帶 pid + rename），daemon 啟動時種回。否則 daemon 重啟後 gauge 消失，
  「多久沒成功備份」的告警就失去判準——掛掉又重啟的備份看起來很健康，這是監控最需要擋的情境。
  一個 repo 一個檔（不是一個 daemon 一個檔），是因為同一個 cache 目錄可能被多個設定共用
  （同一台機器備份兩個 repo）；單值覆蓋沒有 read-modify-write，兩個 process 同時寫也只是
  後寫的贏（時間較新）。放 cache_dir 而不是 config 旁邊：cache 目錄本來就是 kist 的本地狀態
  （index 快取也在那），不需要寫入 `/etc` 的權限；最壞情況（檔案被清掉）只是退回不持久化的行為，
  不值得 fsync。
- **「成功」的定義是全然的 Success**：incomplete（backup 有略過、prune 有刪不掉）不算——
  否則「永遠 incomplete」的 backup 不會觸發「多久沒成功」的告警。incomplete 的監控走
  `kist_job_runs_total{status="incomplete"}` 的增量告警或 webhook。
- 新增 `kist_process_start_time_seconds`：讓監控分辨「剛重啟還沒資料」與「跑了很久沒成功」。
- 監控的正確用法是對「多久沒看到成功」告警（README 有 PromQL 範例），不是對事件告警；
  事件級的通知是 webhook 的職責。
- backup / prune 的統計 gauge 是「最後一次」語意，不累加；累加量只有
  `kist_job_runs_total` 與 `kist_prune_deleted_bytes_total`。

### 4. `/metrics` 無認證、預設綁 loopback

metrics 內容含 repo 位置、機器名稱與備份排程，對攻擊者有偵察價值（勒索軟體會想刪備份，
知道備份時程就能挑時間）。決定：預設 `127.0.0.1:9898`，綁非 loopback 必須顯式給
`--http`，此時印警告。這在「監控通常跑在同一台機器或內網」的前提下是合理取捨。

**給之後的 Web UI（未來 ADR）：** `/metrics` 是唯讀；一旦加上會觸發工作的端點
（「跑 backup」按鈕），認證必須同一批交付，並且要擋 DNS rebinding（驗 Host header）。
按鈕可以沒有、認證不能晚於按鈕。

## 沒做的

- metrics 不經過 Prometheus pushgateway / StatsD：pull 模式已涵蓋目標。
- `run`（無 HTTP）不輸出 metrics：沒有端口就沒有消費者；需要監控就用 `serve`。
- Web UI、SFTP、`mount`：M4 後續 session。
- OpenMetrics 與 text/plain 0.0.4 的內容協商：固定回
  `application/openmetrics-text; version=1.0.0`（prometheus-client 的輸出格式），
  舊抓取器把 `#` 行當註解，讀得一樣。

## 驗證

- `kist-app` 測試：record 後 render 的完整內容、空 registry 的預設值、
  jobstate 種回、`serve_http` 的三個路由（port 0）。
- `kist-cli` 測試：`--json` 走過 backup / snapshots / restore / check / forget / prune /
  rebuild-index / run --once 並解析每個輸出；`kist serve` 用真 binary 起 daemon、
  從 stderr 取得 `--http 127.0.0.1:0` 的實際位址、抓 `/metrics`。
- `kist-format` 測試：id 在 JSON 是 hex、在 CBOR 仍是 bytes（golden 不動）。
