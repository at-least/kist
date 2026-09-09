# ADR 010：M5 fuzzing——target 切分、斷言設計與環境備忘

日期：2026-09-06

## 背景

M5 硬化要求「cargo-fuzz 對 pack parser、CBOR decoder、chunker 連續跑 24h」。
全部 crate `#![forbid(unsafe_code)]`，所以 fuzz 的主要獵物不是記憶體安全，
而是**算術（u64 減法/加法）、配置量（偽造 header 觸發的 Vec 配置）、
斷言的不變量與 panic（含 debug-assertions 開著的 overflow checks）**。

## 決策

1. **打真實路徑，不只打 format 層**：`pack` target 不停在 `kist-format`
   的 bytes 切割，而是帶測試金鑰（`MasterKey::from_bytes([0x42; 32])`）走
   `kist-core::pack::read_trailer` 的完整驗證——trailer「認證過」不等於
   「一致」，有 bug 的 client 寫出的 pack 一樣有有效 tag，這正是要 fuzz
   的帶電面。
2. **四 target**：`pack`（footer 算術 + trailer 驗證 + chunk roundtrip）、
   `cbor`（第一 byte 選結構，全部 metadata 型別 + `parity::parse`）、
   `chunker`（決定論、總和==輸入、每塊 ≤ max、非末塊 ≥ min——最後這條
   由 `fill()` 的「非 EOF 剩餘 ≥ max」不變量支撐）、`parity`（parse 的
   配置前邊界檢查、golden sidecar 對任意損壞 repair、repair 內部已驗
   hash==pack 名所以 fuzz 補驗它沒檢查的「回傳長度 == pack_size」，
   避免恆真斷言的假覆蓋）。
3. **斷言有效性先證明再開跑**：把 chunker 的 max 斷言調小 1 byte 當負向
   對照，libFuzzer 立即在 seed 上抓到並存 artifact——證明崩潰會被看見，
   不是默默吞掉。之後還原。
4. **seed 進版控、corpus 不進**：`fuzz/seeds/<target>/` 放 golden 衍生的
   種子（比從零快一個量級），長出來的 corpus 會無限長大，進 `.gitignore`。
5. **CBOR 深巢狀的第一獵物已被上游防掉**：ciborium 0.2 對深巢狀回
   `RecursionLimitExceeded`（實測 2M 層），`tests/cbor_depth.rs` 把這個
   行為釘住，防 ciborium 升級後防線消失。
6. **看門狗用單次 malloc 上限，不用累計 RSS**（2026-09-06 長跑第一天發現）：
   cbor 在 35 分鐘觸發 libFuzzer 預設 2GB RSS 上限，但 heap profile 顯示
   live heap 只有 36MB；同 corpus 無 ASan 跑 758 萬次執行峰值僅 62Mb。
   成長全是 ASan allocator 對數百萬次小配置的 redzone／碎片頁不還 OS，
   之後飽和（ASan build 的 cbor plateau 在 ~2.5GB）。累計 RSS 檢查在
   ASan build 上量不到 target 的真實記憶體，改傳
   `-rss_limit_mb=0 -malloc_limit_mb=2048`：前者關掉被污染的累計檢查，
   後者保留真正要抓的獵物——偽造 header 觸發的單次巨大配置（觸發時
   abort stack 指向 target 配置點）。`malloc_limit_mb` 預設跟隨
   `rss_limit_mb`，rss 歸零時必須顯式設，兩者都要寫。負向控制驗證過：
   `-malloc_limit_mb=1` 立即在 `malloc(1048576)` abort。代價是失去
   「reachable 但無限成長」的累計守門；target 無全域狀態、LSan exit
   檢查仍在，可接受。

## 環境備忘（sandbox）

- `~/.cargo/config.toml` 的 `rustc-wrapper = "sccache"` 會撿 PATH 上的
  stable rustc 編 nightly 參數（`-Zsanitizer` 被拒）→ fuzz 跑法要
  `CARGO_BUILD_RUSTC_WRAPPER=`（空值）。
- rustup proxy 在 sandbox 會被 argv[0] 改寫弄壞 → nightly bin 放 PATH
  最前面。

操作指令與 24h 長跑寫在 `fuzz/README.md`。

## 後果

- 日常煙霧（每 target 數分鐘）可隨手跑；24h 長跑待執行，找到的崩潰
  經 tmin 最小化後把種子加回 `fuzz/seeds/` 當回歸。
- fuzz crate 獨立 lockfile，libfuzzer-sys 不進主 lockfile。
