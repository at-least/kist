# Release 流程

## 什麼算「可以發」

`make verify` exit 0（build、vet、lint、test、`-race`），加上三個要外部服務的 suite 在 HEAD 上綠：

```console
$ make verify
$ make test-s3      # MinIO in Docker
$ make test-sftp    # OpenSSH in Docker
$ make fuzz         # 每個目標 FUZZTIME（預設 30s）；發版前跑 make fuzz-long（24h）
```

`fuzz` 找到的 crash 會落在各 package 的 `testdata/fuzz/` 裡，**留著**，那是回歸測試。

24 小時的 campaign 別讓它吃滿機器，也別掛在一個會被回收的 shell 底下：

```console
$ systemd-run --user --unit kist-fuzz-long -p MemoryMax=6G -p Nice=19 \
    -p StandardOutput=append:$HOME/.cache/kist-fuzz-long.log -p StandardError=append:$HOME/.cache/kist-fuzz-long.log \
    --working-directory=$PWD --setenv=PATH=$PATH --setenv=HOME=$HOME --setenv=GOMEMLIMIT=768MiB \
    /usr/bin/env make fuzz-long FUZZPARALLEL=3      # 16 核的 20%
$ systemctl --user show kist-fuzz-long.service -p ActiveState -p MemoryCurrent
```

`FUZZPARALLEL` 是每個目標的 worker 數（空 = 每核一個）。chunker 的 seed 故意只比 `MinSize` 大一點：MiB 級的 seed 會讓整個 corpus 都是 MiB 級，三個 worker 一起長就把記憶體吃掉了。

## 版本號

git tag `vX.Y.Z`。binary 裡的版本來自 link 時的 `-X github.com/at-least/kist/internal/cmd.version=…`；`kist version` 印出來。沒 tag 的 build 是 `dev`。

儲存格式的版本（`docs/format.md` 的 v1）跟程式版本無關：程式可以升很多版而格式不動。格式一動就要有 ADR，而且是新的格式版本號。

## 建置

[goreleaser](https://goreleaser.com) v2，設定在 `.goreleaser.yaml`：`CGO_ENABLED=0`、`-trimpath`、linux/darwin/windows × amd64/arm64、tar.gz（Windows zip）、sha256 checksums、git changelog。

本機驗證設定檔，不需要 tag、token 或網路：

```console
$ make release-snapshot     # = go run github.com/goreleaser/goreleaser/v2@v2.18.0 build --snapshot --clean
$ ls dist/
```

正式發版是 CI 在 tag 上跑 `goreleaser release`；**尚未接上**（CI 從未推送過，見 README 的 UNVERIFIED）。接上的時候只需要 `GITHUB_TOKEN`，release 預設是 draft。

## 平台

CI（`.github/workflows/ci.yml`）在每次 push 跑：三個 OS × Go 1.26.x / stable 的 build、vet、test、`-race`；三個 OS 的 golangci-lint；ubuntu 上的 MinIO 與 OpenSSH suite。2026-09-05 首次推送後全綠（run 33903651553）。

| | build | vet | 測試 | 備註 |
| --- | --- | --- | --- | --- |
| linux/amd64 | ✓ | ✓ | 全部，含 `-race`、FUSE、Docker suites、兩個規模的驗收 | 開發機 + CI |
| linux/arm64 | 交叉編譯 | ✓ | 無 | |
| darwin/amd64 | ✓ CI | ✓ CI | `-race` 全套 CI；`mount` 的 FUSE 案例需要 macFUSE，runner 沒有 → skip，**UNVERIFIED** | |
| windows/amd64 | ✓ CI | ✓ CI | `-race` 全套 CI（兩個依賴 POSIX chmod 的測試 skip） | 沒有 `mount`。CI 抓到一個真 bug：NTFS 父目錄列表裡的子目錄 mtime 會滯後，unchanged subtree 因此不去重——已修（stat 目錄本身） |

正式的 `goreleaser release`（tag 上跑、發 draft release）**尚未接上**。
