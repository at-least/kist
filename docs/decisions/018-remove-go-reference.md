# 018 — 移除 Go 參考實作，改用凍結的 fixture repo 把關格式

日期：2026-09-27。狀態：已採用。

## 背景

kist 有兩份實作：Rust（產品，repo 根）與 Go（參考實作，`go/`，原本是最早的
實作，M8 時併進 monorepo）。PLAN 的定位是「Go 只用來交叉驗證格式與抓 bug」。
負責人問：留下 Go 對驗證正確性有幫助，還是純粹負擔？

## 證據：git 歷史稽核（2026-09-27）

三路分類（Rust 端 commit、Go 端 commit、PLAN／ADR 敘述）找出 42 個跨語言事件，
每一件由獨立 agent 讀 commit 本文、diff 與雙邊 commit 時間做對抗複核，判定方向、
發現機制，以及「沒有 Go 還抓不抓得到」。

| 沒有 Go 會怎樣 | 件數 |
| --- | --- |
| 抓不到（Go 必要） | 1 |
| Go 有幫忙，但其他管道很可能也抓得到 | 7 |
| 無法判斷（commit 沒寫怎麼發現） | 8 |
| 與 Go 無關（其中 13 件是只有 Go 有的 bug） | 26 |

- **唯一「必要」的是 51be5ed**：Go 的 v3 移植對「剛寫出、卻帶著開始時標記的主體
  樹」補了 touch，Rust 照抄。沒修時的後果是 commit 被錯誤拒絕一次、重試自癒
  （安全失敗方向）。發現管道是 Go 的 prune 情境測試。
- **自動化跨語言檢查歷來抓到 0 件**，而且設計上擋不住 Rust 單邊改格式：兩份向量
  副本各讀各的、沒有測試比對；Rust 的金鑰向量測試直接呼叫 blake3/argon2 並寫死
  字串，不經 kist-crypto 的常數；Go 的金鑰測試還停在 `kist/v2/*`。稽核者在 scratch
  副本把 Rust 的 CBOR 欄位名或 `CTX_HASH_KEY` 改掉並重錄向量，兩邊全部綠燈。
  兩個 CI workflow 都只能手動觸發；雙向 backup→restore E2E 從未腳本化，最後一次
  手動執行是 2026-09-09。
- **兩份實作不是獨立見證**：同一作者、同日寫兩邊，程式常直接互抄。移植把 bug
  複製過去的次數多於抓出來的次數（restore 穿過 symlink、mount 同名項目、Go 抄了
  「逐 chunk」的註解卻沒抄邏輯、Rust 抄 Go 的 symlink 防護反而帶進 TOCTOU）。
  先前被當作 Go 功勞的 SFTP 缺 mtime，是 Go **單一語言**的合約測試抓到的。
- **成本**：Go 28,124 行，約佔 repo 手寫程式 48%；M8 之後 94 個 commit 中 42 個
  動 `go/`。同期 Go 自己出了至少 5 件資料遺失方向的 bug，多數是移植時帶進來的。
  v2 曾為遷就 Go 選了次級的 CBOR 排序（encode 慢 20 倍，當天撤回）。

## 決定

1. 刪除 `go/`（170 個檔）、`.github/workflows/ci.yml`（Go CI）、
   `.github/workflows/release.yml`（Go 的 goreleaser；Rust 的發佈走 `dist.sh`，
   見 ADR 012）、`ci-rust.yml` 的 interop job、`.gitignore` 的 Go 區塊。
2. **補償一：凍結的 v3 fixture repo**（`crates/kist-core/tests/fixtures/v3/repo`，
   約 330 KB）與 `crates/kist-core/tests/fixture_v3.rs`。它接手 Go 唯一有價值的
   驗證角色：抓「Rust 自己前後一致、但格式悄悄變了」。golden 與向量會隨程式
   重錄，這份不會。每次 `cargo test`：開啟、`check --read-data`、兩個 snapshot
   還原比對（內容、mtime、symlink、空目錄、超過 256 chunk 的間接清單）、tree／
   snapshot／config 解碼後重新編碼逐 byte 比對、同內容再備份零新 chunk、parity
   修復、刪主體後改讀 `.r1`。
3. **補償二：改寫移植唯一「必要」案例的 Go 測試**：
   `prune.rs::backup_and_prune_recover_from_a_crashed_sweep`。起點取自 Go 的
   `TestPruneRewritesTheIndexAfterACrashedSweep`（pack 已刪、標記還在、index 沒動），
   另把根 tree 也刪掉，並讓 backup 在第二輪 prune **之前**跑。這是有意的改寫：
   Rust 的標記隨物件一起刪，照 Go 的順序（prune 之後才 backup）走不到 51be5ed 的
   touch 分支。它釘住三件事：51be5ed 的 touch、孤兒標記清除、重寫 index 丟掉消失的
   pack。第三件在 HEAD 原本沒有任何測試釘住（複核的變異 M3 逃過整個套件）。
   它**沒**釘住 Go 的第三個斷言，也就是「清掉孤兒標記之後、單一 pack 的情況下，
   下一次 backup 必須重傳」。對應的變異 M1（拿掉「有幽靈 pack 就重寫 index」）
   由既有的 `phantom_packs_do_not_steal_canonical_from_real_holders` 抓。
   另外，Rust 的 `execute()` 先寫 index 再刪物件（`prune.rs` 的順序註解：新 pack →
   新 index → 刪物件 → 刪標記），所以單一 Rust prune 當機產生不了這個狀態；它來自
   pack 被外力刪掉（手動或儲存端遺失）。
4. 原本共用的向量檔（`tree-canonical.hex`、chunker 邊界、parity golden、
   `poc_keys.rs` 的值）留下，當凍結的 golden。測試名稱（`interop_*`）不改。
5. Rust 原始碼裡指向 Go 的註解改成直接陳述規則。`docs/format.md` 只改流程敘述
   （單一實作、fixture 條目），**規範性規則未動**；§0 與 §19 的歷史表格保留。
6. 歷史 ADR（001–016）與 PLAN 的歷史紀錄不改寫。

## 證據：補償確實能失敗

依「證據必須能失敗」的規則，每個補償都做了反向對照（改動後跑測試、再還原）：

| 改動 | 結果 |
| --- | --- |
| 拿掉 51be5ed 的 touch 分支 | `backup_and_prune_recover_from_a_crashed_sweep` 失敗：`TreeMarked(...)` |
| Entry 的 CBOR 欄位 `mk` 改名 `mK` | fixture 3 個測試失敗（開啟還原、重新編碼、parity／副本） |
| `CTX_HASH_KEY` 改成 `kist/v3/HASH` | fixture 3 個測試失敗（開啟還原、零新 chunk、parity／副本） |
| gear 表改一個值（`…b3` → `…b2`） | `same_content_backs_up_with_zero_new_chunks` 失敗：`chunks_new: 63` |

還原後全部轉綠。

## fixture 的規則

- **永不重生**，只在格式升版時換，且在一個只含 fixture 的 commit 裡換。重生指令
  寫在 `fixture_v3.rs` 檔頭，需 `KIST_WRITE_FIXTURE=1` 並加 `--ignored`。
- 升版（例如 ADR 017 的 v4）時，v3 這份不刪：改當反向向量，新版讀取端必須以
  版本／`min_reader` 錯誤拒絕它；另凍結一份新版的。
- `.gitattributes` 把 fixture 目錄標成 `-text -diff`，git 永不改動這些 bytes。
- 限制：它抓不到 fixture 寫出當天就已存在的 bug，只保證「今天的程式讀得懂，而且
  會寫出同樣的 bytes」。它也不是獨立實作，無法發現「規格與實作一起錯」。

## 放棄了什麼

- **獨立還原路徑**（目標順序第 2 的還原可靠性）：沒有 Rust 執行檔時，已沒有第二個
  工具能讀 repo。稽核時這條路本來就沒在跑（最後一次手動 E2E 是 2026-09-09）。
  若日後需要，可從 git 取回 Go 當唯讀解碼器的起點。
- **第二個規格讀者**：規格有歧義時，少一份獨立實作把它逼出來。對策是把 format.md
  §18 登記的向量真正生成出來（目前多數還是「待生成」）。
- Go 的測試情境：其中安全相關、Rust 沒有對應的，見下節清單。

## Go 測試情境缺口（報告，未移植）

刪除前盤點了 Go 的 289 個測試情境（`go/internal` 的 `^func Test`，不含 fuzz 與
TestMain），逐一找 Rust 的對應測試。沒有完整對應的，分級為：高 13、中 23、
低 50、不適用 31（例如 Go 專屬的 API 或平台）。13 個高優先項各由一個獨立 agent
複核（讀碼，部分在 scratch 副本跑變異或既有測試），全部確認是缺口。

「變異」欄是 2026-09-27 在工作樹上跑的，每次改一處、跑 `cargo nt --workspace`，
跑完還原。「存活」表示整個套件（333 個測試）全綠，也就是這條規則被拿掉時沒有
任何測試會紅。對照組是 clock-skew 變異（`*t - skew` → `*t - skew * 0`）：它被
`gc_touch_replica::reused_marked_tree_survives_because_touch_refreshes` 抓到
（`332 passed, 1 failed`），證明變異確實有編進去。

| Go 測試 | 情境 | Rust 現況 | 變異 |
| --- | --- | --- | --- |
| `TestPruneRaceRevivalDuringSweep` | sweep 決定刪某棵過期的 tree 之後、真正刪之前，有 backup 重用它並 commit：pack 照刪，tree 的標記取消、tree 不刪 | 無。既有的 revival 測試都在 plan 階段就復活，走不到 execute 的 touch 檢查 | `if revived_by_touch` → `false &&`：存活 |
| `TestTreeRewrittenInTheMarkSecondRevives` | 物件在標記的同一整秒被重寫，要算「標記後重寫」並復活（安全側 `>=`） | 無。所有 prune 測試都先 `sleep(1100ms)` 避開同秒 | `>=` → `>`：存活 |
| `TestPruneMarksThenSweepsAfterTheGrace` | 標記後、grace 未到時的第二輪 prune 必須全數保留、什麼都不刪 | 部分。沒有測試在 grace 內跑第二輪 prune，`report.waiting` 從未被斷言 | 拿掉 grace 等待：存活 |
| `TestBackupRefusesToCommitAfterTheGracePeriod` | 跑超過 GC grace 的 backup 拒絕 commit（`BackupTooLong`），不寫 snapshot | 無。只有 `gc_race` 把它列為可接受的錯誤 | 拿掉閘門：存活 |
| `TestS3EtagFastPathReusesWithoutReading` | s3 來源 etag 變了（大小相同）必須重讀、存新 chunk | 部分。只測了 etag 不變時重用的那一半 | 忽略 etag、只比大小：存活 |
| `TestLoadRejectsAKeyThatContradictsTheObject` | snapshot 用正確的 key 封裝，但內文時間與 key 的時間不符，要當損壞拒絕 | 無。既有測試走的是 AEAD 失敗或結構驗證的路徑 | 拿掉 `time_ns` 比對：存活 |
| `TestPruneHoldsWithinTheClockSkew` | 時鐘差之內的 snapshot 使 prune 保留，差值夠小時才刪 | 沒有專門的測試。所有 prune 測試都把 `clock_skew` 設為 0 | skew 歸零：被抓（見上） |
| `TestPruneKeepsDataOfIndirectChunkLists` | 資料 pack 只經由間接 chunk 清單被引用，prune 必須解開清單，把資料 pack 算成活的 | 無。產生 Indirect 檔的測試都不跑 prune | 未跑 |
| `TestPruneRevivesAMarkedTreeTouchedByALaterBackup` | 標記過期的 tree 被進行中的 backup 重用（刷新 touch），backup commit 前跑的 prune 只能憑 touch 保留它 | 部分。`gc_touch_replica` 的對應測試在 plan 階段就以活引用保留，沒有測到 touch | 同第一列（prune 裡唯一的 touch 檢查）：存活 |
| `TestADamagedIndexBlobIsRepairable` | index blob 翻一個位元，外加一個亂名物件：開啟仍可用、check 回報、rebuild-index 修好，之後乾淨 | 無。沒有測試驗證 rebuild 之後的恢復 | 未跑 |
| `TestLoadAllSkipsUnreadableBlobs` | 一個解不開的 index blob 不影響其他 blob 載入 | 部分。只斷言有錯被回報，沒斷言好的 blob 仍載入 | 未跑 |
| `TestRunnerForwardsPruneGraceToTheBackupGate` | 同一份設定有 backup 與 prune 時，prune 的 grace 要傳給 backup 的閘門 | 不只缺測試，行為也不同（見「稽核順帶發現」第一條） | 不適用 |
| `TestPruneRewritesTheIndexAfterACrashedSweep` | 見決定 3 | 已改寫移植 | M3 被抓；M1 由另一測試抓 |

中優先 23 項（名稱留作日後的索引，盤點時的判定）：

- repo／GC：`CommitGateResolvesChunksNotPacks`（chunk 在重載的 index 裡找不到的分支）、
  `PruneRaceBackupInFlightAtSweepRefusesToCommit`、`PrevChainIsWalkedIteratively`
  （15,000 段 prev 鏈）、`HardLinksAreStoredOnceAndRestoredAsLinks`、
  `RetentionPolicySkipsEmptyBuckets`、`S3BackupPolicy`
- 後端／app：`S3ListCrossesPageBoundary`、`LocalIgnoresLeftoverScratchFiles`、
  `SFTPSourceListsDotfiles`、`*PutIfAbsentHasOneWinner`（並行）、conformance 的
  delete（刪不存在的 key 不算錯；刪後可再 put）、`MountSeesANewSnapshot`、
  `OnceClassifiesCancellationAgainstRealFailures`、
  `SchedulerRunsJobsInOrderWithoutOverlap`、`PruneOptionsResolveDefaults`
- 格式層：`ValidateRejectsImpossibleTrees`、`ReaderRejectsInconsistentTrailer`、
  `ReaderRejectsDamagedPacks`、`SaveNeverOverwrites`、
  `DuplicateChunkResolvesToTheSmallestPackID`、`LoadAllRejectsAMisnamedBlob`、
  `BoundariesDoNotDependOnReadSizes`、`ResetChunksLikeAFreshChunker`

Go 原始碼可用下節的指令取回，逐一對照。

## 取回 Go

最後一個含 `go/` 的 commit 是 `80b89ed`：

```sh
git checkout 80b89ed -- go
```

## 稽核順帶發現（未在本次處理）

- `kist run` 的 backup gate 只看 `[backup].gc_grace`（`crates/kist-app/src/jobs.rs:134`），
  不會退回 `[prune].grace`，兩者也沒有交叉檢查。同一份設定檔只把 `[prune] grace`
  設短時，backup 仍以 72h 判斷，屬「壞 snapshot」方向。這條待辦原本只記在 Go 的
  commit 2dfdd59 本文裡。runtime 已由既有測試證實：
  `app.rs::run_once_executes_backup_forget_prune_in_order` 的設定是
  `[prune] grace = "0s"`，而且斷言 backup 成功。grace 為 0 時閘門是
  `elapsed + 0 >= 0`，恆真，所以若 prune 的 grace 有傳進 backup，backup 必然
  `BackupTooLong`。2026-09-27 執行：`cargo nt -p kist-app --test app
  run_once_executes_backup_forget_prune_in_order` → `1 test run: 1 passed`。
- PLAN.md M7 寫 interop 金鑰向量已對齊 v3，實際 Go 端還是 v2；PLAN 寫 CI 首次真正
  執行在 09-19，`gh run list` 顯示 09-13 就跑過。兩處都是歷史紀錄，不改寫。
