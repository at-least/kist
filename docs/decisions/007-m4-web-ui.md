# ADR 007：M4 —— Web UI（`kist serve` 上的 htmx 介面）

日期：2026-09-05。狀態：已採用。

## 背景

ADR 006 結尾留了約定：Web UI 一旦有會觸發工作的端點，認證必須同一批交付，並且要擋
DNS rebinding——「按鈕可以沒有、認證不能晚於按鈕」。這個 ADR 兌現它。

功能範圍按 PLAN 收得很小：看快照 / 跑 backup / 進度 / 排程。它是自架單人工具的儀表板，
不是多使用者產品。

## 決定

### 1. 模板引擎用 maud（對 PLAN 的一次偏離）

PLAN 指定 askama；實作時負責人指示改用 maud 0.27，在此記錄。差別與理由：

- maud 的模板就是 Rust 函式（`html!` 巨集）：元素與屬性在編譯期檢查，**沒有執行期
  render 錯誤**——askama 的 `render() -> Result` 與「失敗回 500」分支整個消失。
- 跳脫是預設、`Markup` 對 axum 有現成的 `IntoResponse`（自動帶
  `text/html; charset=utf-8`），handler 只剩 `(status, markup)`。
- 三個模板變成 `server.rs` 裡三個函式；原本 index 模板的 `{% include %}` 直接變成函式
  呼叫（同一份 status partial 嵌進首頁、也給 htmx 輪詢換進來）。
- 已知的輸出差異：無縮排空白（對 htmx 無差別）；屬性值一律雙引號、內部的 `"` 跳脫成
  `&quot;`——HTML 解析後等價，`htmx-config` 的 JSON 實測經 HTML parser 解回後
  `json.loads` 成功。on-disk 格式與 PLAN 其他技術選型不受影響。

### 2. 三道門：Host → Basic auth → POST 規則（`ui_guard` 中介層）

只包 UI 的路由（`route_layer`），`/metrics`、`/healthz` 不過門——它們唯讀、ADR 006
交付時就無認證，既有 scrape 設定不能壞。

1. **Host header 逐字比對**（防 DNS rebinding）：白名單 = 綁定位址、`localhost:<port>`、
   `127.0.0.1:<port>`、`[::1]:<port>`、`[serve] allowed_hosts`。不折疊大小寫、不補預設埠；
   不對 → 421。放在 auth 前面：攻擊者连認證 oracle 都摸不到。攻擊面是真實的——攻擊者網頁
   可以對 `http://127.0.0.1:9898` 發請求並讀回應（DNS rebinding），而 UI 能觸發工作。
2. **HTTP Basic auth，只在設了 `[serve] password_file` 時**：帳號任意、只比密碼。
   比對是兩邊各算一次 `blake3::hash` 再比（`Hash` 的 `==` 常數時間、長度也藏起來），
   輸入密碼與解碼後的 header 都用 `Zeroizing` 擦掉。UI 密碼**不是** repo 密碼：repo 密碼
   解得開所有資料，不該放進瀏覽器與 proxy log。沒設密碼 = UI 唯讀。
3. **POST（改狀態的請求）**：沒設密碼一律 403；CSRF 不用 token，靠兩個 header——
   `HX-Request: true`（htmx 一定送；跨站頁面沒有 CORS 就加不了自訂 header）加上
   不能是 `Sec-Fetch-Site: cross-site`。

### 3. 進度：callback 在 core，輪詢在 UI

- `kist-core` 的 `BackupOptions` 加 `progress: Option<ProgressCallback>`：每處理完一個
  目錄項目呼叫一次，結尾的 flush / verify / commit 各有一次（phase 標明）。`None` 時
  零成本，CLI 與既有測試不受影響（probes 只補 `progress: None`）。
- daemon 把 callback 收進 `RunningJob`，UI 不直接掛 callback 到 HTTP：瀏覽器每 2 秒
  `GET /ui/status` 抓 daemon 狀態的快照。輪詢 + 快照讓斷線、關掉分頁都不影響 backup，
  重連就看到現況；進度表只顯示已累計的數字，不承諾 ETA。

### 4. 路由與資料流

- `/` 整頁；`/ui/status` 是 htmx 每 2 秒輪詢的 partial；`/ui/snapshots` 在頁面載入時抓
  （也可按 Refresh）；`POST /ui/jobs/backup` 觸發備份。狀態變化只換
  `<div id="status">` 的內容，整頁不重載。
- **列 snapshots 要真的開 repo**（Argon2 每次 0.2–1.5 s）：用 semaphore 一次只允許一個
  請求開 repo、用完即丟、key 不留在記憶體；請求彼此排隊而不是疊上來。
- 觸發已有工作在排隊 → 409 + 帶 notice 的同一個 partial。htmx 2 預設不把 4xx 換進頁面，
  首頁用 `<meta name="htmx-config">` 的 `responseHandling` 把 409 打開——不然「已排隊」
  這個最重要的回饋使用者看不到。
- 靜態檔（htmx.min.js、style.css、htmx.LICENSE）用 rust-embed 包進 binary：維持
  「單一 binary 就是完整部署」的原則。
- 「Run backup now」是 UI 唯一的寫入操作。forget / prune 不給按鈕：破壞性操作不該一鍵
  觸發，而且抗勒索設計本來就讓 backup 主機沒有 Delete 權限。

## 沒做的

- HTTPS：UI 預設綁 loopback；要遠端存取請放在 reverse proxy 後面。Basic auth 走明文
  HTTP，README 有警告。
- 密碼猜測的速率限制 / 鎖定：密碼比對是 blake3 對 blake3，沒有 KDF 成本，非 loopback
  綁定時線上猜測速度只受網路限制。預設部署（loopback）不受此影響；要遠端存取的人本來
  就得放 reverse proxy 後面——連率限制也一併交給它（例如 proxy 層的 per-IP limit）。
- 帳號系統 / session：單人自架工具，Basic auth + 密碼檔夠用。
- 即時推送（SSE / WebSocket）：2 秒輪詢的即時感已經夠，複雜度不值得。
- 刪 snapshot / 跑 forget / prune 的按鈕：見上。

## 驗證

- `kist-app/tests/ui_http.rs`（10 條，真 TCP 請求）：Host 檢查 421、Basic auth 401（含
  壞 header）、CSRF 403 且真的沒觸發工作、靜態檔的 Content-Type 與快取 header、整頁內容、
  唯讀模式（沒密碼：按鈕變說明文字、POST 403）、409 + notice、真跑一輪 backup 後
  snapshots 列出 3 個檔案的 snapshot。
- `kist-app/tests/daemon_trigger.rs`：觸發與進度在 daemon 側的行為。
- `kist-core/tests/progress.rs`：callback 的呼叫點與 phase。
- 四道關卡全綠：`cargo fmt --check`、`cargo clippy --all-targets -D warnings`、
  `cargo test --workspace`、`cargo deny check`（maud 進 lockfile 後）。
- 實機煙霧測試：`kist serve` + curl 三個頁面，輸出與設計一致；`htmx-config` 屬性值
  經 HTML 解析後為合法 JSON（409 swap 規則在）。
