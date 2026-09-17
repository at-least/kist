# Fuzzing（M5 硬化）

`cargo-fuzz` + libFuzzer，target 對準所有吃外部 bytes 的解析路徑。
fuzz crate 獨立於主 workspace（自己的 lockfile），seed corpus 進版控、
長出來的 corpus 與 artifacts 不進（見 `.gitignore`）。

## Targets

| target   | 吃的輸入                          | 走的路徑                                                                 |
|----------|-----------------------------------|--------------------------------------------------------------------------|
| `pack`   | 整個 pack / trailer 明文 / chunk  | `kist-core::pack::read_trailer`（footer 算術、magic、密封、trailer 一致性驗證）、`compress_chunk`→`decode_chunk` roundtrip |
| `cbor`   | 任意 CBOR（第一 byte 選結構）     | `kist-format::cbor::decode` 對全部 metadata 結構 + `parity::parse`        |
| `chunker`| 任意 bytes                        | FastCDC 切塊；斷言：決定論、總和==輸入、每塊 ≤ max、非末塊 ≥ min          |
| `parity` | sidecar bytes / 損壞 pack bytes   | `parity::parse`（配置前邊界檢查）、golden sidecar 對任意損壞做 `repair`（repair 內部驗 hash==pack 名；fuzz 補驗它沒檢查的「回傳長度==pack_size」）、encode→parse→repair roundtrip |

斷言有效性用負向對照驗證過：把 chunker 的 max 斷言調小 1 byte，
libFuzzer 立刻在 seed 上抓到並存 artifact（2026-09-06）。

## 怎麼跑

一律用 `fuzz/smoke.sh`（封裝環境坑、固定 corpus 在 seeds 前面、收尾驗
seeds 沒被寫入）：

```sh
sh fuzz/smoke.sh <target> 300        # 煙霧：5 分鐘
```

24 小時長跑（M5 驗收；每 target 一次，序列）：

```sh
nohup sh fuzz/longrun.sh $((24*3600)) > /tmp/kist-fuzz-24h.log 2>&1 < /dev/null &
```

`longrun.sh <每 target 秒數> [target ...]` 依序呼叫 smoke.sh，逐 target
落時間戳與成敗結算、檢查 artifact，結束碼 0 = 全部乾淨。脫離 session 跑，
回來看 log 結尾與 `fuzz/artifacts/`。跑之前確認機器閒置（96h 佔一個核、
數 GB RSS），也不要跟記憶體量測類的驗收同時跑。

### CPU 讓路（內建 nice）

長跑預設以 nice 19 跑每個 target（`KIST_FUZZ_NICE` 可覆寫：0–19，0 =
關閉讓路；未設/留空 = 19），sh → cargo → rustc → libFuzzer 全樹繼承，
忙時讓給日常操作、閒時全速。注意 `-max_total_time` 是掛鐘時間：被搶時
每小時 exec 數下降，24h 的總覆蓋率比深夜獨跑少——這是取捨不是失效。

讓路效果依核心排程設定（autogroup、cgroup 排程都會影響 nice 的實效），
換核心後用這個 5 秒協議重驗：兩個 busy loop 釘在同一核（`taskset -c N`），
一個 nice 0、一個 nice 19，量 `/proc/<pid>/stat` 的 utime+stime ticks 差。
2026-09-10（CachyOS 核心 7.2.0：autogroup 開、但 cgroup v2 的 cpu
controller 作用中把它繞過）實測同 scope 與跨 systemd user scope 都是
nice0 佔 99.4%、nice19 佔 0.6%——nice 直接有效，不需要 systemd-run 或
寫 autogroup。

### 看門狗參數（不要改回預設）

smoke.sh 固定傳 `-rss_limit_mb=0 -malloc_limit_mb=2048`。預設的累計 RSS
檢查會被 ASan allocator 的頁保留行為騙到：2026-09-06 cbor 在 35 分鐘時
假性 OOM（RSS 2GB，但 libFuzzer heap profile 顯示 live heap 只有 36MB）；
同 corpus 用 `--sanitizer none` 跑 120 秒、758 萬次執行，峰值僅 62Mb——
成長全是 ASan 對數百萬次小配置的 redzone／碎片頁不還 OS，不是 target
leak。所以關掉累計檢查，改用單次 malloc 上限抓真正要抓的東西（偽造
header 觸發巨大配置；觸發時 abort stack 指向 target 配置點）。注意
`malloc_limit_mb` 預設跟隨 `rss_limit_mb`，rss 歸零時必須顯式設，否則
守門一起消失。兩者都驗證過：負向控制 `-malloc_limit_mb=1` 立刻在
`malloc(1048576)` abort；正式參數下 10 分鐘 run RSS 1.4GB 正常完成。

找到崩潰時，artifact 會落在 `fuzz/artifacts/<target>/`，
用 `cargo fuzz run <target> <artifact 檔>` 重現、
`cargo fuzz tmin` 最小化，修好後把最小輸入加進 `fuzz/seeds/<target>/`
當回歸 seed。

## 本機環境備忘（sandbox 的坑）

- `~/.cargo/config.toml` 的 `rustc-wrapper = "sccache"` 會把 PATH 上的
  stable rustc 撿去編 nightly 參數 → 跑 fuzz 時要 `CARGO_BUILD_RUSTC_WRAPPER=`
  （空值）關掉。
- `cargo +nightly` 需要 rustup proxy；sandbox 會改 argv[0] 弄壞 proxy →
  把 nightly bin 放最前面：
  `export PATH="$HOME/.cargo/bin:$HOME/.rustup/toolchains/nightly-x86_64-unknown-linux-gnu/bin:$PATH"`。

## 種子

`fuzz/seeds/`：golden CBOR（pack_trailer、index、tree、snapshot、config，
前面加 selector byte）、parity Go golden sidecar（原檔/截斷）、固定與
隨機圖樣。新增格式結構或 golden 時，記得在這裡補對應種子。
