# 001 — 專案骨架、module path 與工具鏈基準

- 狀態：已接受（M0）
- 日期：2026-09-04
- 相關：`PLAN.md` M0、`Makefile`、`.github/workflows/ci.yml`、`.golangci.yml`

## 背景

M0 只有一個目的：把之後每個里程碑都要靠的地基釘死——套件邊界、可重現的建置、以及一條「什麼叫做完成」的驗證指令。地基上的每個選擇之後改起來都會波及整個 repo，所以在寫任何格式相關程式碼之前先記錄下來。

## 決策

### 1. Module path 是 `github.com/at-least/kist`

`PLAN.md` 寫的是 `github.com/<owner>/kist`，owner 未定。線索：本 repo 位於 `~/github/at-least/` 之下，同一層的 `shadless` 的 remote 是 `github.com/at-least/shadless`（較舊的 `badcall`、`gavel-mcp` 則是 `github.com/newlix/`）。因此選 `at-least`。

若之後要改成 `newlix`，成本是一次 `go mod edit -module` 加一次 sed；現在只有兩個檔案 import 自己的 module。**這一項待使用者確認。**

同層的 `gavel` 用的是裸 module 名（`module gavel`），這裡不跟：`PLAN.md` 明寫要完整路徑，而且這是要公開發布、會被別人 import 的東西。

### 2. `go.mod` 的 go directive 由相依決定，目前是 1.26.0

`PLAN.md` 說「Go 1.22+」，那是**下限**不是目標版本。M0 當下只有 cobra（下限 1.15），所以先釘在 1.22。M1 加入實際相依之後，`go get` 把它推到 1.26.0：

| 相依 | go directive |
| --- | --- |
| `golang.org/x/crypto v0.56.0` | **1.26.0** ← 決定性的那個 |
| `golang.org/x/term v0.45.0` | 1.25.0 |
| `github.com/klauspost/compress v1.20.0` | 1.25 |
| `lukechampine.com/blake3 v1.4.1` | 1.22 |
| `github.com/fxamacker/cbor/v2 v2.9.3` | 1.20 |
| `github.com/jotfs/fastcdc-go v0.2.0` | 1.14 |
| `github.com/spf13/cobra v1.10.2` | 1.15 |

所以下限是 **1.26.0，由 `x/crypto` 逼出來的**，不是挑的。這比 `PLAN.md` 的 1.22 高，但 1.22 在 2026 年早就沒有安全更新了——為了一個 EOL 版本而把加密函式庫釘在舊版，方向是反的。

CI 的 matrix 跑 `1.26.x` 與 `stable`：go directive 若只寫不測，遲早會變成一句謊話。之後每加一個相依都要重看這張表；升高下限就回來改這一節，不要另開 ADR。

### 3. 分層照 `PLAN.md`，每個 package 先放 `doc.go`

`internal/` 下的 `backend`、`crypto`、`chunker`、`pack`、`index`、`tree`、`snapshot`、`repo`、`cmd` 目前都只有 `doc.go`。這不是佔位符：doc.go 先把每個 package 的職責寫死，之後放程式碼時就有一份可以對照的合約（例如「backend 只看得到密文」、「index 是快取，永遠可以從 pack trailer 重建」）。工程規範本來就要求每個 package 有 doc.go，先寫比後補誠實。

CLI 的進入點是 `cmd/kist/main.go`，內容只有一行 `cmd.Execute()`；命令樹在 `internal/cmd`。這樣命令可以在測試裡用 buffer 執行，不必開 subprocess。

### 4. `version` 是子命令，版本號由連結期注入

`var version = "dev"`，由 `make build` 以 `-ldflags -X` 蓋掉，`VERSION` 預設取 `git describe --tags --always --dirty`，在還不是 git repo 的情況下退回 `dev`（M0 當下就是這個情形）。

做成 `kist version` 子命令而不是只掛 `rootCmd.Version`，是因為 M0 的驗收條件是「`kist version` 可執行」，而子命令才有辦法在單元測試裡驗證輸出。`internal/cmd/version_test.go` 是本階段唯一的測試，但它讓 `go test ./...` 有東西可以失敗——全部 `no test files` 的綠燈不是證據。

### 5. 不帶 cgo 建置；`-race` 是測試專用建置

`PLAN.md` 禁止 cgo，而 `-race` 需要 cgo（實測：`CGO_ENABLED=0 go test -race ./...` 直接報 `-race requires cgo`）。解法是把兩者分開：

- `make build` / `make test`：`CGO_ENABLED=0`，跟出貨的建置一致。
- `make test-race`：`CGO_ENABLED=1`，只跑測試，不產出任何要發布的檔案。

CI 三個 OS 都跑 race。一開始 Windows 是跳過的（不想讓測試套件相依於那台 runner 的 C 工具鏈），後來依使用者要求改成全跑：Go 的 data race 是靜默的記憶體損毀，對備份工具而言那等於靜默的資料損毀，值得為它多裝一個編譯器。**注意：Windows 上的 `-race` 從來沒有在這台機器上實際跑過**，只有交叉編譯與 `go vet` 驗過。

### 6. golangci-lint 釘在 v2.13.2，設定用 v2 schema

v2 的設定檔跟 v1 不相容（需要 `version: "2"`、`linters.default`、獨立的 `formatters` 區塊）。版本同時釘在 `Makefile` 與 CI，兩邊要一起改。

在 standard 集合之外多開的 linter 都對應到這個專案的具體風險，不是湊數：`errorlint`（錯誤比較要吃 `%w` 包裝）、`nilerr`（err 非 nil 卻回 nil 是資料遺失的起點）、`bodyclose`（S3 後端漏 body 就是漏連線）、`gosec`（這支工具經手金鑰和別人的資料）、`revive` 的 `package-comments` 與 `exported`（工程規範要求）。`errcheck` 開了 `check-blank`：`_ = err` 也算吞錯，例外只有兩類，都寫在設定檔裡並附理由——(1) 清理路徑上的 `Close`/`Remove`，那裡已經在回傳或已經在失敗，錯誤無處可去；(2) 寫到命令自己的 stdout/stderr 的 `fmt.Fprint*`，管線斷了就沒有第二條通道可以回報。列表是按具體型別列的，透過其他介面呼叫的 `Close` 仍然要真的檢查或附 nolint 理由。

`exhaustive` 檢查 switch 與 map 的列舉窮盡性，且 `default-signifies-exhaustive: false`——重點正是「加了一個新的 `NodeType` 或壓縮演算法而某個 switch 沒跟上」時要被告知，而格式演進靠的就是加列舉值。

### 7. `make fuzz` 逐一列舉 fuzz target

`go test -fuzz` 一次只吃一個 package，`go test -fuzz=. ./...` 會直接報錯。`fuzz` target 因此用 grep 找出所有 `func FuzzXxx`，逐個 package、逐個 target 跑 `-run '^$' -fuzz '^Name$'`。目前沒有 target 時印 `no fuzz targets yet` 並以 0 結束，這樣 M5 之前這個 target 也是可以跑的。

### 8. CI 直接呼叫 `go`，不透過 `make`；lint 也跑三個 OS

`windows-latest` 上有沒有 make 不是值得賭的東西。CI 的每個步驟都是一行 `go ...`，Makefile 則是同一組指令的本機版本。代價是兩邊要手動保持同步。

lint job 跟 test job 一樣跑滿 Linux / macOS / Windows。理由不只是照 `PLAN.md` 的字面：golangci-lint 只分析 build constraint 符合當下 GOOS 的檔案，只跑 Linux 的話，M4 之後的 `_windows.go`（VSS）與 FUSE 相關程式碼會被無聲跳過。

## 後果

- `make verify`（build + vet + lint + test + test-race）是「這個改動算不算做完」的單一判準。
- 之後每加一個相依套件，都要確認它的 go directive 不高於我們的下限，否則就是被迫升級。
- Module path 與 Go 下限這兩項若之後改動，要回來更新這份 ADR，不要另開一份。
