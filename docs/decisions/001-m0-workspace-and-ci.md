# ADR 001：M0 骨架 —— workspace 切分與 CI

- 狀態：已採用
- 日期：2026-09-04
- 里程碑：M0

## 背景

M0 的目標是「有一個能編譯、能跑 `kist version`、CI 會擋住低級錯誤」的骨架，
還不需要任何備份邏輯。這份 ADR 記錄骨架階段做的幾個決定。

## 決定

### 1. 一個 workspace、六個 crate

按 PLAN.md 的結構切成 `kist-format`、`kist-crypto`、`kist-chunker`、
`kist-backend`、`kist-core` 五個 library，加上產出 `kist` 執行檔的 `kist-cli`。

為什麼一開始就切開：crate 邊界就是依賴方向的護欄。`kist-format` 不依賴任何
其他自家 crate，所以「只有 format 能決定 on-disk 長什麼樣」這件事由編譯器保證，
不是靠自律。日後改格式時，編譯錯誤會直接告訴我們哪些地方受影響。

### 2. 版本與相依集中在 workspace

所有 crate 的 `version` / `edition` / 相依套件版本都寫在最上層 `Cargo.toml` 的
`[workspace.package]` 與 `[workspace.dependencies]`，子 crate 只寫
`clap.workspace = true`。這樣升級套件只要改一個地方，也不會出現兩個 crate
用到同一套件的不同版本。

所有 crate 都標 `publish = false`：現在還不想（也不該）不小心發布到 crates.io。

### 3. Lint 直接寫在每個 lib.rs

每個 library crate 的 `lib.rs` 開頭有：

```rust
#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used)]
```

`forbid(unsafe_code)` 表示這個 crate 不能出現 `unsafe`，而且不能被子模組用
`allow` 繞過。`unwrap`/`expect` 在遇到錯誤時會直接讓程式 panic（整個程式當掉），
對備份工具來說太粗暴——我們要的是回報錯誤、保住資料。這兩個是 clippy 的
「restriction」類 lint，預設不開，所以要明寫 `deny`，而且只有跑
`cargo clippy` 時才會檢查（`cargo build` 不會）。CI 兩個都會跑。

測試程式碼不在此限；未來 lib 內的 `#[cfg(test)] mod tests` 會加
`#[allow(clippy::unwrap_used, clippy::expect_used)]`。

### 4. 錯誤處理：lib 用 thiserror，bin 用 anyhow

library 要讓呼叫者能分辨錯誤種類（例如「檔案不見了」vs「解密失敗」），
所以用 `thiserror` 定義具名的錯誤型別；`kist` 這個執行檔只需要把錯誤印給人看，
用 `anyhow` 一路往上拋比較省事。

### 5. CI：三平台 + 四道關卡

GitHub Actions 在 ubuntu / macOS / Windows 各跑一次 `cargo fmt --check`、
`cargo clippy -D warnings`、`cargo test`；另外用
`EmbarkStudios/cargo-deny-action` 在 ubuntu 上跑一次 `cargo deny check`
（相依套件的已知漏洞與授權）。

用 `Swatinem/rust-cache` 而不是 sccache：它就是一個 action、零設定，
對這種規模的專案已經足夠，少一個會壞掉的零件。

觸發條件寫 `on: [push, pull_request]`、不限制分支名稱——本地目前的分支是
`master`，若寫死 `main` 會變成 CI 永遠不跑卻沒人發現。

### 6. 先寫測試

`crates/kist-cli/tests/version.rs` 是先寫、先看它失敗、再寫實作的。
測試用 `env!("CARGO_BIN_EXE_kist")` 直接執行編譯出來的 binary，
不需要額外套件，三個平台都能跑。

## 影響

- 之後每加一個命令，就先加一個像 `version.rs` 這樣的行為測試。
- 授權（LICENSE）還沒決定，`deny.toml` 暫時以 `private = { ignore = true }`
  跳過自家 crate；選定授權後要拿掉這行並補上 LICENSE 檔。
