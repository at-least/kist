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

24 小時長跑（M5 驗收；每 target 一次）：

```sh
for t in pack cbor chunker parity; do sh fuzz/smoke.sh $t $((24*3600)); done
```

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
