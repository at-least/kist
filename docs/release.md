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

| | build | vet | 測試 | 備註 |
| --- | --- | --- | --- | --- |
| linux/amd64 | ✓ | ✓ | 全部，含 `-race`、FUSE、Docker suites | 開發機 |
| linux/arm64 | 交叉編譯 | ✓ | 無 | |
| darwin/* | 交叉編譯 | ✓ | 無 | `mount` 需要 macFUSE，**UNVERIFIED** |
| windows/* | 交叉編譯 | ✓ | 無（`-race` 從未在 Windows 上跑過） | 沒有 `mount` |
