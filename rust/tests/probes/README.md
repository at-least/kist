# 審查者的探針（M3）

兩位獨立 reviewer 各留下一支重現資料遺失路徑的程式（ADR 005 §3、§3b），修正後改成斷言「資料還在」，
對最終程式碼全部通過。它們不是 workspace 成員，各自是一個小 crate（path 依賴指向 `crates/*`）：

```sh
cd tests/probes/reviewer1 && CARGO_TARGET_DIR=../../../target cargo run --release --bin a   # 同理 b、c
cd tests/probes/reviewer2 && CARGO_TARGET_DIR=../../../target cargo run --release -- two-prunes   # 或 rebuild
```

- reviewer1 `a`：prune 走訪之後才 commit 的 snapshot 引用到 repack 丟掉的 chunk → 現在 `NO DATA LOSS`。
- reviewer1 `b`：tree 在 HEAD 與 DELETE 之間被重 put → commit 以 `TreeMarked` 安全失敗。
- reviewer1 `c`：同一 chunk 在過期標記的 pack 與新 pack 都有時 commit 不會假失敗（8/8）。
- reviewer2 `two-prunes` / `rebuild`：兩個 index 寫入者重疊造成的幽靈 pack → 不搶正本、下一輪從 index 消失、
  restore 與 check 都乾淨。

`crates/kist-core/tests/{prune,backup_gc_rules}.rs` 裡有對應的（較小的）回歸測試。
