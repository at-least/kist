# ADR 012：發佈二進位（linux-musl / macOS / Windows，全從一台 Linux 主機產）

日期：2026-09-06　狀態：已採用

## 背景與目標

M5 收尾項：`release：cargo-dist 或 GitHub Actions 產出 linux(musl)/macos/windows binary`。
本 repo **沒有 git remote**，CI 是 workflow_dispatch-only 且從未跑過，所以
「真的能交到使用者手上的 binary」必須能從這台開發機（CachyOS/Arch，x86_64）本機產出；
CI 設定只是給未來接上 remote 時用。

## 決策

### 1. 版本 0.1.0

workspace 版本從 0.0.0 升到 0.1.0：產物檔名與 `kist --version` 不該是 0.0.0
（看起來像壞掉的）。repo 從未發佈過，直接定 0.1.0。

### 2. 五個 target，全部從 Linux cross

| target | 產物 | 煙霧測試 |
|---|---|---|
| x86_64-unknown-linux-musl | tar.gz | 本機原生全流程 |
| aarch64-unknown-linux-musl | tar.gz | qemu-user-static 全流程 |
| aarch64-apple-darwin | tar.gz | 僅建置 + Mach-O 檢查 |
| x86_64-apple-darwin | tar.gz | 僅建置 + Mach-O 檢查 |
| x86_64-pc-windows-gnu | zip | 僅建置 + PE32+ 檢查 |

工具：`zig` + `cargo-zigbuild` 統一處理所有 C 依賴（zstd-sys、ring）與連結；
Linux 選 musl 是為了靜態連結、免 glibc 綁定。Windows 選 **gnu** 而非 msvc：
依賴全數支援（ring/zstd/tokio/rustls），msvc 要下載 Windows SDK + clang-cl，
對 CLI 工具沒有額外價值。若未來 ring 的 asm 在 zig-cc 下出問題，
退路是裝 mingw-w64-gcc（同一 target triple，只改 linker）。
windows-gnu 建置時 rustc 會印
`warning: linker stderr: ignoring deprecated linker optimization setting '1'`
——這是 zig linker 對舊版 `-O` 參數的無害提示，不用處理。

### 3. 修掉的第一個阻塞：xattr 在 Windows 編不過

`xattr` crate 只有 Unix 實作（原始碼無 `cfg(windows)` 分支），而
kist-core 對它的依賴原本是無條件的 → Windows target 連編譯都不過
（`error[E0433]: cannot find 'unix' in 'os'`）。程式碼本來就有非 Unix 的
`read_xattrs` 空實作（fsmeta.rs），所以只需把依賴移到
`[target.'cfg(unix)'.dependencies]`。修復前先重現失敗、修復後同一處通過。

### 4. cargo-zigbuild 的 exported_symbols_list bug 與 shim

darwin 的 cdylib（object_store 拉進來的 `crc-fast`）連結失敗：
`unable to read exported symbols list '-dead_strip'`。

根因（用最小重現驗證）：rustc 對 cdylib 傳分離形式的
`-Wl,-exported_symbols_list` `-Wl,<file>`；cargo-zigbuild 0.23.4 為了支援
whole-archive，會把「`-Wl,` + 存在檔案」的參數改寫成 positional 輸入檔並
排到選項之後，symbols list 檔案中招，lld 便拿下一個選項（`-dead_strip`）
當 list 檔名。逗號形式 `-Wl,-exported_symbols_list,<file>` 不符合改寫規則，
可直通（實測通過）。

解法：`scripts/zigcc-apple.sh`——把相鄰的 `-Wl,-exported_symbols_list -Wl,<file>`
合併成逗號形式再轉手 `cargo-zigbuild zig cc`；透過
`CARGO_TARGET_*_APPLE_DARWIN_LINKER` 環境變數掛上（cargo-zigbuild 用
`add_env_if_missing` 裝自己的 wrapper，所以 config.toml 的 linker 設定會被
它蓋掉，必須走環境變數）。上游修好後，此 shim 與 dist.sh 裡的兩行 env 可刪。

### 5. macOS 需要 SDKROOT（zig ≥ 0.14 不再附 framework tbd）

主程式連結需要 `Security`/`CoreFoundation` framework——reqwest 0.13 的
`rustls-no-provider` feature 強制拉 `rustls-platform-verifier`，macOS 上
走 security-framework。zig 0.14 起不再附 macOS framework 的 tbd stub，
所以必須給真 SDK：`SDKROOT=~/.cache/macos-sdk/MacOSX11.3.sdk`
（phracker/MacOSX-SDKs 的 11.3 版，對應 `-mmacosx-version-min=11.0`；
SDK 本體不進版控，dist.sh 會檢查並提示取法）。

### 6. 本機 dist.sh 是發佈入口；cargo-dist 設定只到 plan 驗證

- `dist.sh`：重建五個 target → `dist/kist-v<version>-<target>.tar.gz|.zip`
  （binary + 雙 LICENSE）→ `dist/SHA256SUMS`。zip 用 bsdtar 產（系統沒有 zip）。
- cargo-dist（已改名 `dist`，0.32.0，2026-05 仍在活躍維護）：`dist-workspace.toml`
  釘同樣五個 target、kist-cli 以 `[package.metadata.dist] dist = true` 標記
  （`publish = false` 會讓 dist 跳過它）、生成的 `.github/workflows/release.yml`
  給未來有 remote 時用（tag 驅動）。本機只用 `dist plan` 乾跑驗證設定，
  不追 `dist build` 端到端——沒 remote 時它的 CI 價值是投機的，且
  tag-based 版本流程只會增加摩擦。注意 CI 的 artifact 命名（`kist-cli-*`）
  與本機 dist.sh（`kist-v*`）不同，是兩條獨立管線。

### 7. 順手清掉：clippy --all-targets 其實沒過

上個 session 回報「clippy 0 errors」是錯的：以 CI 的命令
`cargo clippy --workspace --all-targets -- -D warnings` 重跑，
HEAD（7179258）就有 13 個 error（kist-crypto 的 chacha20poly1305
deprecated API、kist-format 測試的 dead-code、kist-core 測試的
`% == 0`/clone-on-Copy 等，以及 cache.rs 兩個 unused-mut 是 PruneIndex
refactor 的殘留）。本次全部清掉（deprecated trait 改用 `AeadInOut`，
行為不變、測試全綠；其餘為機械性修改），`--all-targets` 現在乾淨。

## 驗證

- x86_64-musl（本機原生）：`kist 0.1.0` → init → backup（300 KiB / 3 chunks）
  → 二次 backup `new: 0 B in 0 chunks, 0 packs written` → snapshots →
  restore → `diff -r` 逐 byte 相同 → prune --dry-run 正常。
- aarch64-musl（`qemu-aarch64-static`）：同一流程全過、diff 相同。
- windows-gnu / darwin ×2：`cargo zigbuild --release` 通過 + `file`
  確認 PE32+ console x86-64 / Mach-O arm64 / Mach-O x86_64。
- `dist plan`：v0.1.0、五 target 的 artifact 矩陣與 checksum 全部列出。
- 產物抽驗：解壓縮確認 binary + LICENSE，zip 內 `kist.exe` 為 PE32+。
- 全 workspace 232 tests 綠、`cargo clippy --workspace --all-targets -- -D warnings`
  乾淨。

## 沒做 / 已知限制

- macOS 與 Windows binary 只驗證「能建、格式正確」；沒有對應機器可跑
  （wine/qemu-x86 對 Windows 是可選的後續）。
- 建置機相依：darwin 產出依賴本機 SDK 與 shim；在乾淨機器上要先照
  dist.sh 說明備齊。未來若接上 remote，tag 驅動的 dist CI 會接手。
- 本機 dist.sh 產 tar.gz（較快），dist CI 產 tar.xz——兩條管線刻意不統一。
