# M1 驗收腳本

在 repo 之外的目錄跑（資料量約 25 GiB，不要放 tmpfs）：

```sh
mkdir -p ~/kist-acceptance && cp tests/acceptance/*.py ~/kist-acceptance/
cd ~/kist-acceptance
python3 gen.py src                                   # 10 萬檔、約 10 GiB
python3 -u run.py /path/to/kist-rs/target/release/kist   # backup → restore → diff -r → check → 破壞偵測
```

`run.py` 每一步都有斷言，任何一步失敗就非 0 結束。`rss.py` 可以單獨包住一個命令量它的峰值 RSS。
`m1-acceptance-2026-09-04.log` 是 M1 驗收當天的完整輸出。
