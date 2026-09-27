# 019 — 拿掉 Go 之後：Rust 端重設計盤點

日期：2026-09-27。狀態：**已採用**（負責人 2026-09-27 裁定「全做」，見文末「裁定」；
盤點與複核本身未改任何程式，實作逐項另 commit）。

## 背景

移除 Go 參考實作（ADR 018）之後，負責人說：

> 沒有了 go 的包袱，rust 可以完全依照最適合 rust 的方式重新設計

「格式與整個架構重來一次該怎麼做」的檢討，ADR 017 前一天已經做過（六個提案、
每案三個對抗審查者），結論是不整個重來，只把 v4「樹進 pack」留給負責人裁定。
本文**不重做**那份檢討，也不重開 017 裡理由與 Go 無關的決定（抗勒索、GC 競態、
資料遺失方向）。本文只回答三件事：

1. 哪些 Rust 設計是因為要配合、對齊或移植 Go，才長成現在的樣子？
2. 哪些格式中立（不改 on-disk bytes）的 Rust 重設計值得做？
3. ADR 017（或其他 ADR）裡哪些結論的理由靠 Go、跨語言向量或雙實作撐著，
   拿掉 Go 之後要重新推理？

分桶：

- **A**：格式中立。改完之後 `crates/kist-core/tests/fixture_v3.rs` 的測試照樣會過。
- **B**：會改 on-disk bytes，而且當初是為了遷就 Go 而選的；只能隨格式升版，
  併入 017 的 v4 清單。
- **C**：ADR 裡某個保留或拒絕的結論，理由靠 Go／跨語言向量／雙實作。

依規則，凍結的格式常數（FastCDC gear 表、Reed-Solomon 矩陣、CBOR 欄位表順序、
金鑰派生字串、AAD 規則）就算當初來自 Go，也不算 Go 包袱；改它們一律是 B。

## 做法

1. **分範圍找**：九個 finder 各看一塊——git 歷史與 5cf1197 改寫的 hunk（history）、
   ADR／PLAN／format.md（adr）、backup、prune／forget／reach／check（prune）、
   repo／pack／index／cache／restore（repo）、kist-format／kist-crypto／kist-chunker
   （format）、kist-backend（backend）、kist-app／kist-cli／kist-mount（app）、
   跨 crate（crosscut）。說「Go 造成」必須附證據（commit、ADR 行、或 5cf1197 的
   hunk），沒有證據就不算。
2. **去重**：原始 74 條，合併重複後 45 條；再由一個完整性審查者補 3 條
   （critic-1～3），共 48 條進入複核。
3. **每條三個鏡頭**：事實與因果（file:line、commit、Go 因果成不成立）、值不值得與
   適不適合（成本是真的還是口味、合不合 PLAN 與負責人的偏好）、分類（A／B／C、
   會不會改 bytes、有沒有 PLAN 衝突）。前兩個是反駁者：兩個都沒駁倒＝CONFIRMED；
   恰好一個駁倒＝PLAUSIBLE；兩個都駁倒＝REFUTED。
4. **結果**：CONFIRMED 28、PLAUSIBLE 19、REFUTED 1、UNVERIFIED 0。

PLAUSIBLE 其實是兩群：17 條是「值不值得」被駁（事實成立，但照原提案做不划算，
複核通常給出縮小版）；2 條是「事實」被駁，而且只駁 Go 因果（adr-1、adr-7，
問題本身仍在）。

分類器與 finder 的分桶在 48 條上完全一致，沒有需要改桶的；分歧都在「是不是 Go
造成的」，見「不是 Go 包袱」一節。唯一要特別註明的是 A13（backend-2）：分類器
判 A，但標了「會改 bytes」（見該條）。

**執行紀律**：整個盤點沒有修改 repo（`git status` 自始至終只有既有的
`?? mcp_server.log`，HEAD 43fd4b6）。凡寫「實測」的，出自 finder 或複核者在
scratchpad 副本執行並留有原始輸出的結果；彙整這一步沒有重跑任何東西，只用
`git status` 與 `sed` 核對了部分 file:line。其餘都是讀碼所得，執行期行為的主張
一律標 UNVERIFIED。

寫進 repo 前另外獨立重跑了兩條（HEAD 43fd4b6 的乾淨副本，repo 本身未動）：

- **A1 的機制**：object_store 0.14.1（Cargo.lock 的版本），`Path::from` 把
  `notes.txt~`、`report#1.txt`、`photos[2024]/a.jpg` 編成 `notes.txt%7E`、
  `report%231.txt`、`photos%5B2024%5D/a.jpg`，`Path::parse` 保持原樣；
  `plain/a.txt` 兩者相同。探針有斷言，exit 0。只驗了路徑字串，端到端的「備成空
  目錄」沒有重跑。
- **C1**：複核者的 `vr1_repack_gate.rs` 探針（走真 prune repack，32 回）。修正前
  `total gate failures: 11 / 32`，其中離線 client 的 5 回在第二輪 prune 之後重跑
  仍失敗（`blocked=4`）；把 backup.rs:589 換成 `load_index_for_backup(..)` 之後
  `total gate failures: 0 / 32`，同一次執行 fixture_v3 4 過、backup_gc_rules 10 過、
  gc_touch_replica 4 過。scratchpad 是多個平行 agent 共用的，有幾位複核者撞到別人
改過的副本，事後都改在獨立副本重跑；本文引用的是重跑的結果。

## 結論

- **沒有 B**。唯一為遷就 Go 而選的格式設計（CBOR map key 排序）v2 當天就撤回了
  （63d2c9d、ADR 008 修訂），今天只剩 `kist-format/src/lib.rs:10` 一行過時註解（A33）。
- **Go 真正留下的程式形狀不多**：Entry 攤平成 u8 常數（A35）、restore 的路徑式 symlink
  防護（A4）、parity 模組自帶 key（A40）、mount 的「每次 lookup 配新節點」（A6）、
  幾個照 Go 取的欄位名（A27）。更常見的是反過來：**Go 做對了、Rust 沒跟上**
  （A5、A12、A7、A6、A29）。
- **真正值得做的是一批與 Go 無關、格式中立的修正**，其中幾件已實測到資料遺失或
  還原失敗方向：遠端來源名稱含 `~ # [ ]` 的子樹被靜默備成空目錄（A1）、restore
  失敗會刪掉使用者原有的檔（A2）、restore 中途換掉中間目錄就寫到目標外（A4）、
  從 mount 拷檔會漏（A6）、commit gate 誤拒而且重跑無效（C1）。
- 「完全照最適合 Rust 的方式重寫」這個方向，複核後大多縮成**補測試加小修**。
  被駁的 17 條多半是口味，或原提案會變差（例如把錯誤變成靜默截斷）。
- **ADR 017 的結論不必因 Go 移除而改**；要改的是幾句理由，以及把 canonical rank
  從 v4 的決定 2 搬到決定 3（格式中立、現在就修）。
- 沒有任何一項需要「需解鎖技術選型」。要負責人點頭的是 kist-format 的改動
  （PLAN.md:85）、一個新的直接依賴（rustix，已是間接依賴）、幾個對外 JSON 形狀，
  以及 restore 的契約，見最後一節。

## A 桶（格式中立，現在就能做）

42 條（CONFIRMED 27、PLAUSIBLE 15），分三級：第一級是實測或讀碼可證的錯誤行為，
依資料安全、還原可靠的順序排；第二級主要是補測試；第三級是可讀性與介面整理，
其中被「值不值得」駁倒的只列複核縮小後的殘項。**「提案」一律寫複核修正後的版本**，
不是 finder 的原稿；原稿裡會變差的部分列在「考慮過但駁回」。

重疊的幾組：A2／A3／A4 是同一條 restore 修正線；A1／A13／A36 同根（來源位置）；
A7／A23／A37 都在 backup 走訪；A9／A27／A28 都動對外 JSON；A5／A24／A26 都是
設定與預設值。

### 第一級：資料安全與還原可靠

**A1　遠端來源：名稱含 `~ # % [ ]` 的檔讀不到，目錄被靜默備成空的（backend-1）**

- **現況**：`ObjectStoreSource::to_store_path`（kist-backend/src/source.rs:276-285）
  與 root（:252）用 `Path::from`，它會把 `~ # % [ ]` 等字元百分比編碼；清單端用不
  編碼的 `Path::parse`（sftp.rs:668、1380；object_store client/s3.rs:43、81）。同一個
  名字，列出來和讀的時候是兩個字串。SFTP 開目錄遇 NotFound 回空清單
  （sftp.rs:1352-1357），走訪端只在 Err 時記帳（backup.rs:1222-1231）。
- **提案**：來源內部的 store 路徑一律改用 `Path::parse`（root 也是），parse 失敗回
  `BackendError::Source`，由走訪端記進 skip 帳；補 InMemory 的 list→read 往返測試
  （`a~1.txt`、`d[1]/x`）。約三行加一個測試。
- **代價的證據**：實測（kist CLI 對 `rclone serve sftp`）：`photos[2024]` 整棵子樹被
  備成空目錄，結束碼 0，還原出來是空的；`notes.txt~`、`report#1.txt` 讀不到被略過，
  結束碼 3。`rclone serve s3` 也重現（kist 送出雙重編碼的 `%257E`）。對真實 AWS：
  UNVERIFIED（請求 URL 由用戶端組成，推論相同）。
- **工作量**：S
- **複核**：CONFIRMED。非 Go 所致（Go 的 join 用原始字串）。這一項是 A13 的前置；
  非 UTF-8 名稱經 lossy 轉換、BadName 靜默略過是另外的問題，這次不涵蓋。

**A2　restore 就地覆寫：失敗會刪掉使用者原有的檔（critic-1）**

- **現況**：restore_file 以 write＋create＋truncate 直接開正式檔名
  （restore.rs:367-372），邊讀邊寫；出錯時呼叫端無條件 `remove_file`
  （restore.rs:268-272）。間接內容先讀清單才開檔（restore.rs:347-358），清單讀失敗
  時檔案根本沒被碰過，照樣被刪。契約是「既有檔案會被覆寫」（restore.rs:146）。
- **提案**：同目錄以 `O_CREAT|O_EXCL|O_NOFOLLOW` 建暫存檔，寫完後在同一個 fd 上
  套 xattr、時間、mode（即 A3），最後 `rename` 成正式名；用一個約 15 行、實作 Drop
  的小 struct 在失敗時刪暫存檔，取代手動 `remove_file`。不需要新 crate。
- **代價的證據**：實測（scratch clone）：間接內容的 pack 全刪後還原，目標處原有的
  「PRECIOUS user data」檔被刪（還原還沒開檔就失敗）；寫到一半中斷，3 MiB 的檔只
  寫了 19625 bytes 卻留在正式檔名；擋路的 symlink 被刪、檔也沒還原；同一 snapshot
  還原兩次到同一目標，第二次對 mode 444 的檔得到 EACCES 後把 kist 自己剛還原的檔
  刪掉；直接內容缺 chunk 時原內容被毀。
- **工作量**：M
- **複核**：CONFIRMED。更正與取捨：硬連結分支的 `link(2)` 本身是原子的（目的地
  已存在回 EEXIST），視窗在失敗後改走複製的那條路；覆寫大檔時新舊兩份同時佔空間；
  CLI restore 沒有 Ctrl-C 處理（main.rs:580-610），SIGINT 時 Drop 不會跑，會留下
  隱藏的暫存檔（比正式檔名下的半截檔好）；還原到先前還原出來的唯讀目錄（0555）時
  建暫存檔會 EACCES，現行原地寫入反而成功（實測），實作要選退回原地寫或明確報錯。
  非 Go 所致（「殘檔移除」來自 25600ac）。

**A3　restore 的 mode 與時間以路徑套用，會跟隨被換上的 symlink（repo-2）**

- **現況**：restore_file 寫完就把 File drop 掉（restore.rs:413），之後
  `fsmeta::apply` 以路徑呼叫 `filetime::set_file_times`（fsmeta.rs:267）與
  `std::fs::set_permissions`（fsmeta.rs:278，含 setuid／setgid 位）；目錄同樣以路徑
  套用（restore.rs:335-336）。
- **提案**：metadata 一律經已開的 handle 套用：`File::set_times`、
  `File::set_permissions`、`xattr::FileExt::set_xattr`，保留 xattr → 時間 → mode 的
  順序；目錄以 `O_RDONLY|O_DIRECTORY|O_NOFOLLOW` 開 handle。只用 std 與既有 crate。
  與 A2 同一次做。
- **代價的證據**：實測（探針，非 kist 本身）：寫完、drop、把路徑換成指向外部檔的
  symlink 後以路徑套用，外部檔被改成 mode 4755、mtime 1000000；改用 fd 則外部檔
  不受影響。目錄變體已實測可行。路徑被換成 FIFO 時 set_file_times 會卡住（實測
  1.5 秒後仍 blocked）；整個 restore 因此掛住：UNVERIFIED（只測了 filetime 呼叫）。
- **工作量**：S
- **複核**：CONFIRMED。更正：setxattr 不會跟隨（xattr crate 走 lsetxattr，實測回
  EPERM），實際跟隨的只有 chmod 與設時間；filetime 0.2.29 的 set_file_times 是先
  `File::open` 再 futimens，fsmeta.rs:265-266 的註解因此過時。以 fd 開目錄遇到
  symlink 回的是 ENOTDIR，不是 ELOOP，錯誤對應要另外處理。

**A4　restore 的中間路徑元件仍有 TOCTOU（repo-3）**

- **現況**：create_dir_nofollow（restore.rs:85-123）逐段 lstat、缺的就 create_dir，
  檢查完就結束；之後以完整路徑 `path.join(name)` 開檔（restore.rs:364-384），
  O_NOFOLLOW 只管最末段。這段演算法是 850c10f 照 Go 的 mkdirAllNoFollow 移植的
  （「行為變更（刻意，向 Go 端看齊）」），但 Go 另有「目標必須為空」的前提，Rust
  沒有（restore.rs:146、159；restore_twice_into_same_target_succeeds 釘住）。
- **提案**：以目錄 fd 為錨走訪：每層持有已驗證的目錄 handle，用 `mkdirat`、
  `openat(dirfd, name, O_NOFOLLOW…)`、`symlinkat`、`linkat`，不再二次解析完整路徑。
  `forbid(unsafe_code)` 下用 safe 封裝：rustix（已由 xattr、tempfile、gethostname 間接
  帶進 Cargo.lock）改成 kist-core 的直接依賴。非 unix 分支維持兩步檢查。硬連結表
  （restore.rs:161、239）改存（目錄 handle, name）。
- **代價的證據**：實測（kist 本身，同 uid）：來源目錄 2000 個檔，還原途中把中間
  目錄換成外指 symlink，1999 個檔寫到目標之外，restore 只報 1 個錯。另一次實測：
  目標外既有的檔被 O_TRUNC 蓋成 snapshot 的內容，summary 還把它算成還原成功。
  跨使用者利用：UNVERIFIED（需要攻擊者對目標之下某層有寫權限，例如 root 還原到
  使用者擁有的既有目錄樹，或預先建好 `/tmp/out` 這類可預測路徑）。
- **工作量**：L
- **複核**：CONFIRMED。更正：Go 同樣以完整路徑開檔，中間段 TOCTOU 在 Go 也存在；
  「目標必須為空」只縮小攻擊面、關不掉它（攻擊者擁有的空目標一樣通過）。
  ADR 018:34-35 指的是另一件已修的事。
  【需負責人同意：新增直接依賴 rustix（不是換技術選型）；「只接受空目標」的替代案
  會改產品契約】

**A5　`kist run` 的 backup 閘門不看 `[prune].grace`（backup-8）**

- **現況**：jobs.rs:134 只讀 `b.gc_grace.unwrap_or(DEFAULT_GC_GRACE)`；prune 走
  `PruneSection::options`（config.rs:273-284）；validate（config.rs:159-216）沒有交叉
  檢查。只把 `[prune] grace` 設短時，閘門仍用 72h，屬「壞 snapshot」方向
  （ADR 005 §6b）。ADR 018「稽核順帶發現」第一條已列。
- **提案**：kist-app 加唯一解析點 `Config::gc_grace()`
  （`backup.gc_grace.or(prune.grace).unwrap_or(DEFAULT_GC_GRACE)`），jobs.rs:134 走它；
  validate 只在 `backup.gc_grace > prune.grace` 時報錯（有方向），或直接取兩者較小值。
  順手改 ADR 005 §8 與 §6b 矛盾的「不一致是安全失敗那一邊」，以及 BackupTooLong
  訊息裡只寫 CLI 旗標名的地方。
- **代價的證據**：實測（scratch 副本變異）：轉發之後
  `run_once_executes_backup_forget_prune_in_order` 以 BackupTooLong 失敗，坐實
  ADR 018 的推論；有方向的檢查加轉發後 37 個測試 36 過，只有預期的那個失敗。
  Go 端早已轉發（80b89ed:go/internal/run/run.go:212-221）。
- **工作量**：S
- **複核**：CONFIRMED。更正：finder 提的「兩者都設且不同就報錯」會擋掉安全方向的
  設定，實測讓 `config_parses_and_validates` 失敗。觸發條件比 finder 寫的窄：單一
  daemon 本身串行；實際路徑是外部 cron 疊跑 `run --once`（第二次的 backup 拿不到
  client 鎖，但 forget、prune 照跑）或同一份設定部署到多台（UNVERIFIED，只讀碼）。

**A6　mount 每次 lookup 都配新 inode、永不回收（adr-4）**

- **現況**：InodeTable::insert（kist-mount/src/corefs.rs:164-185）每次都配新號，
  lookup 的四個 insert 點（363、434、480、538）沒有依（父 ino, 名稱）去重，map 從不
  移除；fuse.rs 沒實作 forget。ADR 015:63-64 在「語意取捨（對齊 Go）」下寫「記憶體
  上限＝瀏覽過的條目數」，與程式不符。
- **提案**：InodeTable 加一張 `HashMap<(u64, Vec<u8>), u64>`，同名再 lookup 回同一個
  ino；既有的存在檢查照舊先跑。不引入 nlookup 計數。
- **代價的證據**：實測（真的 FUSE 掛載，20000 個檔的目錄 `cp -r`）：cp 結束碼 1、
  略過 57 個檔（「replaced while being copied」）、diff 不一致、421 秒、mount 行程 RSS
  51 MB → 1.9 GB；只加去重表後 20000／20000 一致、69 秒、RSS 57 → 66 MB。機制：
  頂層 entry TTL 1 秒（corefs.rs:26），過期重新 lookup 拿到新號，整棵已快取子樹作廢。
- **工作量**：S
- **複核**：CONFIRMED。Go 關聯：「每次 lookup 生新節點」的形狀照 Go 移植，但
  go-fuse 會在 kernel FORGET 時回收節點（本機 go-fuse v2.11.0 fs/inode.go:434-438），
  Rust 換成 fuser 低階 API 後沒補上。Go 版當年有沒有同樣的漏檔：UNVERIFIED。

**A7　走訪時 kind 與 metadata 分兩次觀察：條目消失會讓整個 backup 失敗（backup-4）**

- **現況**：LocalListing::next_item lstat 一次只拿來判 kind（kist-backend/src/source.rs:170、
  189-197，FIFO／socket／裝置都歸 File），走訪端再 lstat 一次取 posix
  （kist-core/src/backup.rs:1071-1077）。第二次失敗時 posix 為 None，symlink 與目錄
  條目不填 mode，tree validate 拒收（tree.rs:267-274），錯誤一路傳到最上層。特殊檔
  靠 `posix.mode & 0o170000` 魔數過濾（backup.rs:1363-1372）。
- **提案**：LocalListing 把已有的 lstat 結果放進條目（`posix: Option<Box<PosixMeta>>`，
  遠端為 None），加 `SourceItemKind::Special`，刪第二次 lstat 與魔數。最小替代（S）：
  `mk == POSIX && posix.is_none()` 時一律 skip 並回 `Ok(None)`，不動 kist-backend 的
  公開 API。兩種都要補「條目在走訪中途消失」的回歸測試。
- **代價的證據**：實測（真 LocalSource，symlink／目錄反覆建刪）：symlink 模式 300 次
  中 3 次、目錄模式 300 次中 1 次，整個 backup 以
  `posix entry requires mode/uid/gid/mtime` 失敗。這是 72530c0 的回歸（它把 posix 從
  SourceItem 拿掉）。validate 在 put 之前，不會寫出壞樹；受影響的是排程備份的可靠性
  （RPO），不是還原。真實環境命中率 UNVERIFIED。雙重 lstat 實測每次約 1.1 µs，
  效能不是理由。
- **工作量**：M（最小替代 S）
- **複核**：CONFIRMED。Go 的 SourceItem 在列舉時就帶 Posix（80b89ed:go/internal/source/source.go），
  Rust 在 72530c0 為記憶體分岔。Box 有必要：遠端清單仍整批物化。
  【結構修法要重跑 ADR 011 的記憶體 bench B 集】

**A8　用 `head().is_err()` 判斷「不存在」，暫時性錯誤被當成不存在（prune-1）**

- **現況**：check.rs:145（孤兒副本）、check.rs:156（副本缺失）、prune.rs:758
  （第 13 步 parity 孤兒）把任何錯誤都當不存在；038bf3e 修過同檔同類的第 14 步，
  這三處沒修。`Backend::exists()`（kist-backend/src/lib.rs:350-356）早已區分三種結果。
- **提案**：check 的兩處改三路 match：存在就略過、不存在照舊回報、其他錯誤把真實
  錯誤記進 report。**不能用 `?`**：這一步在資料校驗與 parity 修復之前，一個 `?`
  會丟掉整份報告，main.rs:733 也就什麼都不印。prune.rs:758 改
  `!exists(key).await?`，與 038bf3e 修第 14 步的方式一致。prune.rs:790 至少記一條
  warn。`delete_if_exists`／`head_opt` 兩個 helper 可選（口味）。
- **代價的證據**：實測（本機後端，chmod 0o000 注入 EACCES）：主體樹檔實際存在，
  check 卻報「replica exists but its primary tree is missing (possible data-loss event…)」；
  副本存在卻報「tree replica is missing (repairable)」。prune.rs:758 在並發 backup
  加暫時性錯誤時刪掉活 pack 的 parity：UNVERIFIED（沒有注入接縫，只讀碼；損失的是
  冗餘，主資料不受影響）。
- **工作量**：S
- **複核**：CONFIRMED。check 的兩處現在就能用 gc_touch_replica.rs:281-291 的 chmod
  手法寫 RED→GREEN 測試。

**A9　監控：備份結果先抹成 JSON，再用字串路徑取值（app-1）**

- **現況**：jobs.rs:144、160、172 以 `serde_json::to_value(..).unwrap_or_default()` 把
  BackupSummary 等轉成 `serde_json::Value`（jobs.rs:59）；metrics.rs:167-204 用
  `["report","bytes_stored"]` 這類字串路徑取值，取不到就不設 gauge、不報錯。
- **提案**：`#[serde(untagged)] enum JobDetail { Backup(..), Forget(..), Prune(..), None }`，
  metrics 改 match 直接讀欄位；webhook 與 `--json` 的 JSON 值不變。先寫一個「空備份
  讓 gauge 歸零」的會紅測試。
- **代價的證據**：一次已出貨的靜默失效：f256642（09-08）把計數搬到 report 底下，
  metrics 的字串路徑沒人改，到 4e2c0b2（09-19）才修，訊息自承三個 gauge「自始沒更新
  過」；同一個 commit 裡型別化存取的 jobs.rs 被編譯器逼著改了。另一個現存缺陷
  （實測，真 kist-core）：SnapshotStats 的零值欄位不序列化
  （kist-format/src/snapshot.rs:171-181），空來源的備份（例如掛載點沒掛上）errors=0、
  狀態 Success，`kist_backup_files`／`kist_backup_bytes` 卻停在上一次的 2 與 5005——
  正是 ADR 006 §3 說監控最該擋下的情況。
- **工作量**：M
- **複核**：CONFIRMED。更正：「壞過兩次」多算一次（08198b8 的改名與修補在同一個
  commit）；jobstate 不存 detail；`run --json` 的 key 順序會從字母序變宣告序（值不變），
  外部是否依賴順序 UNVERIFIED。

**A10　解碼、版本檢查、validate 分三段由呼叫端接：ChunkList 至今不檢查版本（format-1）**

- **現況**：kist-format 只給泛型 `cbor::decode`（cbor.rs:41），版本與 validate 由各
  呼叫點自己接。ChunkList（tree.rs:568）沒有 validate，三個解碼點 restore.rs:354、
  restore.rs:433、corefs.rs:699 都不比 `version`，不符 format.md:440。PackTrailer 的
  「開啟→解碼→版本→validate_trailer」在 pack.rs:208-225 與 rebuild.rs:98-139 各抄一份，
  錯誤先帶佔位 key `"<pack trailer>"` 再改寫。tree 與 snapshot 的版本各比兩次
  （repo.rs:388 與 tree.rs:58；repo.rs:644 與 snapshot.rs:64）。
- **提案**（縮小版）：照 `parity::parse`（parity.rs:110-114）的樣式，每個型別一個直白
  函式，`cbor::decode` 維持 pub、不用 trait。先做兩件：ChunkList 單一解碼函式（含
  v==3），三個呼叫點共用；單一 trailer 解碼函式給 read_trailer 與 rebuild 共用，錯誤
  直接帶真 key。tree／snapshot 的包裝與刪重複比對順手做。
- **代價的證據**：「某個邊界忘了接驗證」已修至少 5 次：63d2c9d、c73aac2、75a5bbb、
  3bf554a（「snapshot 漏了同一刀」）、a7ce568（版本錯謊報成密碼錯）。實測泛型 decode
  接受 v=4 的 ChunkList；在 scratch 副本的 restore.rs 兩個解碼點加版本檢查後
  fixture_v3 4 項全過，標記顯示檢查確實被 restore 與 `check --read-data` 走到。
  restore.rs:417 的 resolve_chunks 也被 reach.rs:143、backup.rs:1496 呼叫，所以 check、
  prune、parent 快速路徑同樣接受 v≠3 的清單（讀碼）。
- **工作量**：S（縮小後）
- **複核**：CONFIRMED。Go 因果被推翻（見「不是 Go 包袱」）：kind 應為 idiom。
  d5f0c63 是 validate 少一條規則，不屬「忘了接」，不計入。
  【需負責人確認：kist-format 公開 API 改動（PLAN.md:85）】

**A11　密碼檔「取第一行」寫兩份，`\r` 處理不同（app-9）**

- **現況**：CLI（kist-cli/src/password.rs:15）不去 `\r`；kist-app（config.rs:248）去掉
  結尾 `\r`，而且整檔 String 沒包 Zeroizing（config.rs:244），和 ADR 007:40 的清零
  要求不一致。
- **提案**：kist-app 的 read_password_file 改 pub、整檔也包 Zeroizing，CLI 的檔案分支
  呼叫它（kist-cli 已依賴 kist-app，main.rs:17-18 已有同樣用法）。**統一方向必須用
  kist-app 的規則**（去 `\r`），反過來會讓 daemon 寫下的 repo 打不開。補 `\r` 回歸測試。
- **代價的證據**：實測（HEAD 編出的 binary）：密碼檔內容 `secret\r`（後面沒有
  `\n`），`kist run --once` 備份成功；同一個檔交給 `kist snapshots --password-file`
  回「wrong password」。也就是 daemon 每晚備份正常，出事那天拿同一個密碼檔跑 CLI
  還原，會被告知密碼錯。觸發條件少見（第一行結尾有 `\r` 且後面不是 `\n`）。
- **工作量**：S
- **複核**：CONFIRMED。一次性相容破壞：用 CLI `--password-file` 建立、密碼檔尾帶
  孤立 `\r` 的 repo，統一後要改用 KIST_PASSWORD 或修密碼檔，需附遷移說明。

**A12　parity 份數超出 0..=8 時，靜默變成沒有冗餘（backup-7）**

- **現況**：`BackupOptions.parity: u8`（backup.rs:118），backup 本身不驗；CLI
  （main.rs:455）與設定檔（config.rs:170）各寫字面值 `> 8`，沒用 MAX_PARITY_SHARDS
  （parity.rs:32）。超出時 parity::encode 回錯，只記 warn（backup.rs:1724-1733）。
- **提案**（縮小版）：backup 入口檢查 `usize::from(opts.parity) > MAX_PARITY_SHARDS`，
  回 `CoreError::Usage`（backup.rs:487 驗 gc_grace 已是這樣）；兩處字面值改用常數；
  補「parity: 9 必須回錯且不寫任何物件」的測試。ParityShards newtype 是口味，不列。
- **代價的證據**：實測（直接呼叫 kist_core）：parity=9 與 255 時 backup 成功、parity
  物件 0；對照組 2 與 8 各 1 個。兩個正式呼叫端今天都已驗範圍，所以目前碰不到；
  兩處檢查都沒有測試釘住。Go 在 7cd0456 補過 repo 層的同一道防線，5cf1197 刪 Go 時
  一起消失。
- **工作量**：S
- **複核**：CONFIRMED。

**A13　SFTP 來源借用 repo 端的 SftpStore（backend-2）**

- **現況**：ObjectStoreSource 同時服務 S3 與 SFTP；SFTP 來源是 SftpStore::open_source
  （sftp.rs:904-926），再經 ObjectStore::list_with_delimiter（sftp.rs:1338-1400）轉型。
  repo 命名空間與來源規則共用一個 SftpInner，靠 `list_all` 這個 bool 區分
  （sftp.rs:704-709、959-979）。名稱先 lossy 再 Path::parse，BadName 靜默丟掉
  （sftp.rs:1383、1389）。open_source 還會呼叫 `ensure_dir`（sftp.rs:924），開來源就在
  來源主機上建目錄。
- **提案**：專用的 SftpSource 直接走 openssh-sftp-client（已是依賴）的 open_dir／
  read_dir，名稱存原始 bytes、略過的條目照實記帳、不 mkdir；SftpStore 拿掉
  list_all、open_source、list_with_delimiter。**順序**：先做 A1；再寫 Docker-gated 的
  SFTP 來源測試，今天應在四件事上變紅（含 `[` 的目錄、dotfiles、伺服器送 `.`／`..`、
  來源路徑不存在不得 mkdir）；最後才拆。保留 be89cd6、ec2392e 的敵意伺服器加固。
  sftp.rs 拆成五個檔是口味，不列。
- **代價的證據**：這一層已出過三件 bug：01fd9b6 第 3 項（雙重前綴，列出 0 條目）、
  6b8c537（沿用 repo 的 dot-skip，每次都靜默漏掉 .bashrc、.ssh/）、9b0ee93（拿掉
  dot-skip 時順手拿掉 `.`／`..` 防護）。HEAD 沒有任何 SFTP 來源整合測試。ensure_dir
  的後果（路徑打錯時備出空樹不報錯）：UNVERIFIED（沒有 SFTP 伺服器）。
- **工作量**：M
- **複核**：CONFIRMED。**分類器判 A，但標了會改 bytes**：非 UTF-8 檔名、含控制字元
  的名稱（今天整棵被丟），以及今天讀不到的特殊字元檔名，改完後寫出的樹會跟今天
  不同。這是修正遺漏，仍是合法 v3（Entry.n 本來就是 bytes，LocalSource 也存原始
  bytes），不需要升版；一般 UTF-8 檔名的 bytes 不變；fixture 不涵蓋 SFTP。
  子目錄降記成 GENERIC、mtime 只取秒、symlink 一律當 File 等現行記錄方式要照舊。

**A14　§9 locator 切段規則有三份（history-4）**

- **現況**：fsmeta.rs:56-82 的 locator_to_relative 與 kist-mount/src/vpath.rs:121-144 的
  locator_components 各實作一次同一條規則；corefs.rs:396-401 內嵌第三份（不剝
  scheme、不映射 `..`、不查 NUL）。「恰好一個非 DIR entry 且名稱＝末段」的落點判別
  也有兩份（restore.rs:173-176、corefs.rs:410-418）。fsmeta.rs:48 的
  bytes_to_relative_path 沒有呼叫端。vpath.rs:263 的註解含原始 NUL byte，grep 會整檔
  跳過。
- **提案**：純函式放在 kist-core fsmeta（kist-mount 已依賴 kist-core；放 kist-format
  就變成需確認），restore、vpath、corefs 都呼叫；locator_to_relative 名稱與行為保留。
  **先補表格測試**（含 `..`→`__parent__`、`s3://bucket/` 的現行行為），再搬呼叫端；
  合併時逐位元保留現有程式行為，不要照文件去「修」。寫入端 backup.rs:830-840、878
  的「末段」規則不要動（動了會改 tree bytes）。
- **代價的證據**：兩次漂移已修：5cb6ecd（09-19）core 補 NUL、mount 補 `..`；73ff6a6
  （09-21）mount 才拒 NUL，中間差兩天。兩次都以 Go 為對照點之一，Go 移除後沒有
  第三方對帳。實測（探針逐字複製兩段演算法）：九個正常 locator 的末段一致，只有
  `s3://`、`sftp:///`、`/a/..`、含 NUL 會分歧。
- **工作量**：S
- **複核**：CONFIRMED。fixture_v3.rs:166-167 用 locator_to_relative 自己算期望值，
  是自我參照，抓不到規則改動；真正釘住映射的是 source_backup.rs、restore_hardening.rs
  與 vpath.rs 的單元測試。

### 第二級：補測試與防呆

| 編號 | 項目 | 現況 | 提案（複核後） | 代價的證據 | 量 | 複核 |
| --- | --- | --- | --- | --- | --- | --- |
| A15 | snapshot 列表規則兩份（critic-2） | find_parent 自己列、自己濾 `.r1`（backup.rs:947-985，第 963 行）；snapshots.rs:40-51 另一份 | 先補測試：replicas=1 連做兩次 backup，斷言有 parent 且 files_reused==files；find_parent 改呼叫 snapshots.rs 的列表函式。若加 SnapshotKey 型別，格式不符的 key 仍須讓 prune／check 報錯 | 01fd9b6^ 沒濾，本機預設 replicas=1，約 9 小時內每次增量都沒 parent；實測刪掉第 963 行後全 workspace 333 項全綠 | S | CONFIRMED。損失是效能，不是資料；CLI、mount 改用型別屬口味 |
| A16 | client id「先鎖後建」只靠註解（crosscut-4） | lock 與 load_or_create 是兩個 pub 函式，順序在 main.rs:477、jobs.rs:124-126 各寫一次 | `client_id::acquire -> ClientLease`（持鎖檔與 id，drop 時釋放），CLI 與 daemon 都改用它 | 963fbab「daemon 與 CLI 兩處同改」（GC 安全方向）。實測兩處都改回舊順序，kist-cli 與 kist-app 53 項全綠 | S | CONFIRMED。原提案的快取預設、來源分類、human_bytes 剔除（刻意設計或口味；daemon 預設開快取反有還原風險） |
| A17 | 金鑰向量不經 kist-crypto（format-3） | poc_keys.rs 自建 argon2／blake3、字串寫死（:33-36、:45-51）；chunker 的 interop.rs:69-109 再算一次，dev-dep（Cargo.toml:19-20）只為它 | 加兩個測試：以凍結 SEALED_MASTER 呼叫 `unlock_key_slot`；`RepoKeys::from_master(..).chunk_id` 對上凍結 HASH_SUB。hex 不動 | 實測 CTX_HASH_KEY 改成 `kist/v3/HASH`：kist-crypto、kist-chunker 全綠，只有 fixture_v3 紅；新測試會紅且指名子金鑰 | S | CONFIRMED。Go 造成（08198b8 為共用 testdata 建的形狀）。價值偏低：fixture 已擋。刪 chunker 那份需修訂 ADR 018 決定 4 |
| A18 | 壓縮框架兩份（repo-6） | ZSTD_LEVEL、1/16 門檻、有上限的解壓在 pack.rs:21-87 與 repo.rs:20-172 各一份 | 抽 `unframe(payload, limit, what)`，共用常數與門檻判斷；寫入函式各留一份。附測試釘住等級 3 與 1/16 邊界 | dfc5853 修好 chunk 路徑後，index 路徑仍無上限，87 分鐘、14 個 commit 後 d2439fe 才補。實測等級改 19、門檻改 len/2，fixture_v3 仍全過 | S | CONFIRMED。實測抽出後 fixture_v3 全過。frame_streaming 抽象剔除 |
| A19 | kist-mount 缺 PLAN 規定的 lint（app-6） | kist-mount/src/lib.rs:1-31 沒有 forbid(unsafe_code)、deny(unwrap_used, expect_used)；corefs.rs:218 非測試碼 expect | 補兩行屬性；corefs.rs:218 改 `if let`；offsets.rs:55、vpath.rs:188、fuse.rs:372 三個測試模組加 allow | 實測 CI 的 `-D warnings` 對它 exit 0；少了測試模組 allow 會多 12 個錯 | S | CONFIRMED。那個 expect 實際不會觸發；成本是違反 PLAN.md:181、ADR 010:8 不實 |
| A20 | SftpStore 的死碼（backend-6） | get_ranges override（sftp.rs:1227-1276）、copy_opts（:1402-1441）沒有呼叫者，「整讀＋take 封頂」三份 | 兩者都改回 NotImplemented（照 sftp.rs:1151-1160），刪 ADR 013:119 那行 | 同一道加固分兩次落地（be89cd6、ec2392e「reviewer 跟進」）。copy_opts 忽略 CopyMode，create 模式會靜默覆寫：UNVERIFIED | S | CONFIRMED。不要改走 trait 預設 get_ranges：實測 start 越界會 panic |
| A21 | fixture 沒有 s3／sftp 條目（adr-1） | 來源只有本機 posix（fixture_v3.rs:80-93、324-360）；etag、vern、mk=1／3 只剩 tree-canonical.hex 守，`UPDATE_VECTOR=1` 會就地改寫它 | 另凍結一份小 fixture（新目錄，不是重生 v3/repo）：FakeSource 搬到 tests/common、支援 vern，產生 mk=1、mk=2；做解碼再編碼逐 byte 比對。ADR 018 補一行 | 實測 etag 改名：333 項只有 interop 紅，UPDATE_VECTOR=1 後變綠。實測 SFTP、S3 的 mk 值互換，validate 拒收凍結向量；舊 s3 snapshot 還原失敗 UNVERIFIED | M | PLAUSIBLE。事實鏡頭駁倒 Go 因果與「xattr、硬連結唯一守門」（golden.rs:105-141 也守）；拆 ignore 產生器屬口味 |
| A22 | 三條 GC 安全規則沒有測試（prune-2） | prune.rs:435（grace 等待）、:650（同秒 `>=`）、:672（touch 復活） | 不重構。把複核者寫的三個整合測試移進 tests/：filetime 撥 mtime、分段呼叫 prune_plan／execute、注入 PruneOptions.now，不 sleep | 實測三個變異在 kist-core 下全存活；三個探針在原碼全過，各殺掉一個變異 | S | PLAUSIBLE。值不值得被駁：重構成純函式不必要，呼叫點的變異單元測試殺不掉 |
| A23 | Source 的 sync／async 邊界（backup-3） | 同步 `Source::list` 內部 `Handle::block_on`（source.rs:299、367），呼叫端靠註解手包（backup.rs:872-877、1215-1224） | list 與 head_root_file 改 async fn（async_trait 已是依賴），刪兩處 block_on 與兩處手包；LocalSource 的 read_dir 放 spawn_blocking、仍回惰性迭代器（ADR 011）；補 InMemory 回歸測試 | 01fd9b6「實機第一次就 panic」。實測刪掉兩處手包，kist-core 全綠（FakeSource 不 block_on） | S–M | PLAUSIBLE。值不值得被駁：原提案把整個走訪搬到 blocking 執行緒，block_on 變十幾處、佔掉 max_blocking_threads(4) |
| A24 | 設定檔時間長度靠逐欄屬性（critic-3） | 五個欄位各手寫 `with = "crate::duration::serde_opt"`（config.rs:75-109） | 補測試：解析 `[prune] inactive_after = "30d"`，最好整份解析 config.rs 模組文件的範例。不加 newtype | 9625758 修過 clock_skew 漏屬性。實測拿掉 config.rs:108 的屬性，53 項全綠、文件寫法解析失敗 | S | PLAUSIBLE。值不值得被駁：探針證明 HumanDuration 擋不住同類錯（漏寫型別照樣編得過） |
| A25 | `run --once` 結束碼優先序沒測試（app-5） | 判斷在 main.rs:329-348 | 在 kist-app 測試裡從 on_outcome 回呼送出 shutdown，釘住「先失敗再停止」必須非零 | 6ca7f2a「reviewer 抓回的順序錯誤」。實測現在兩個方向都對（失敗＋SIGINT exit 1） | S | PLAUSIBLE。值不值得被駁：Rust 不取消進行中的工作，stopped 不影響結束碼；OnceVerdict 會改 `--json` 形狀 |
| A26 | CLI 預設值重抄 core（app-8） | clap 寫死 `72h`、`30d`、`1h`、`50`（main.rs:63-72、148） | 補測試：`Cli::try_parse_from(["kist","prune"])` 得到的 options 等於 `PruneOptions::default()`；可併入 A5 | 今天數值相同、沒有漂移紀錄、沒有測試 | S | PLAUSIBLE。值不值得被駁：改 Option 會失去 clap 自動顯示的 `[default: 72h]`（實測） |

### 第三級：可讀性與介面整理

| 編號 | 項目 | 現況 | 提案（複核後） | 代價的證據 | 量 | 複核 |
| --- | --- | --- | --- | --- | --- | --- |
| A27 | 報告欄位名照 Go、與實際量的不符（backup-1） | chunks_read（backup.rs:133）只數沿用的；packs_revived（:136）逐 chunk 累加 | 改名 chunks_reused、chunks_revived，改 doc、backup_gc_rules.rs:435-466 測試名與 format.md:298-300 | 實測首次備份讀了 32 個 chunk 報 0，快速路徑沒讀卻報 32；標記 1 顆 pack 報 14。名字來自 08198b8 改用 Go 的名稱 | S | CONFIRMED。只影響 `backup --json`、`run --json`、webhook；metrics、jobstate 不讀。【改對外 JSON】 |
| A28 | forget 的 JSON 形狀依入口而異（app-4） | CLI 包成 `{snapshot, reasons}` 加 dry_run（main.rs:903-941），daemon 直接輸出 tuple（jobs.rs:160） | kept 改 `Vec<Kept { snapshot, reasons }>`，ForgetSummary、PruneReport 帶 dry_run；刪 CLI 包裝；修訂 ADR 006 §1；補 daemon 路徑測試 | 實測兩個入口形狀不同，daemon 那份正是 ADR 006 判定「太脆弱」的 tuple | S | CONFIRMED。webhook 預設只送 failure／incomplete。【改對外 JSON】 |
| A29 | CheckOptions 兩個 bool（prune-3） | repair 隱含 read_data 只在 core 內改寫（check.rs:55-62），CLI 用原始 bool 印摘要（main.rs:746） | 最小：main.rs:746 改成讀 `read_data` 或 `repair`，補 CLI `--repair` 測試；CheckLevel enum 可選 | 實測（HEAD binary）`--repair` 確實讀了資料，摘要卻少了「(data read)」；CLI 從沒測過 `--repair` | S | CONFIRMED。Go 端本來就是兩者取或，移植漏了 |
| A30 | Corrupt 被當萬用錯誤（repo-5） | 本機 symlink 擋路（restore.rs:87-90、377-380、389-392、489-492）與 xattr 失敗（fsmeta.rs:236-240）都報「object … is corrupt」 | 加一個本機擋路變體（例如 `RestoreBlocked { path, reason }`）替換四處；xattr 保留 Io 與 source。讀 repo bytes 的點維持 Corrupt（repo.rs:346-347 的 `.r1` 退路靠它） | 實測使用者自己的 symlink：檔案層報 corrupt、結束碼 3；目錄層整個 restore 中止 | S | CONFIRMED。Compress 變體、結構化 errors 剔除；Go 因果被推翻 |
| A31 | mount 把錯誤抹成 EIO 且不記 log（app-11） | corefs.rs:319、387、510、659、728 一律轉成 `FsError::Io`；corefs 只有兩處 tracing | 私有 `fn io(e: CoreError) -> FsError`，先 `tracing::warn!` 再回 Io；修 corefs.rs:77、427 過時註解 | 讀碼：後端、解密、缺 chunk 都只剩 EIO；restore.rs:62-72 每 60 秒只印一次「repack may be in progress」，會誤導。掛載實測 UNVERIFIED | S | CONFIRMED。kist-app 的 String 錯誤部分剔除（改帶 source 的變體可能把 webhook token 帶回 log） |
| A32 | fsmeta 兩份（crosscut-3） | PosixMeta（kist-backend/src/fsmeta.rs:11-22）與 FsMeta（kist-core/src/fsmeta.rs:113-125）八欄相同，backup.rs:1457-1466 逐欄抄 | `pub use kist_backend::fsmeta::PosixMeta as FsMeta`；source.rs:316-321、400-405 兩份 chrono→ns 合一；補 restore 時間轉換的負值測試 | 1970 年前 mtime 被夾成 0 的同類 bug 修過兩次（4d97250、b03ee7d），前者訊息「兩份漂移正是夾 0 沒被發現的原因」 | S | CONFIRMED。時間 helper 模組、0 哨兵改 Option 剔除；Windows lossy 分岔 UNVERIFIED |
| A33 | kist-format 頂端文件說 CBOR 會排序（crosscut-7） | lib.rs:10「map keys 排序」；實際是欄位表順序、不排序（cbor.rs:4-6、PLAN.md:32） | 改成「struct 依欄位表順序、不排序；唯一例外 xattrs 依 bytes 排序（format.md §4 第 5、7 條）」 | 實測編碼器照宣告順序輸出。08198b8 加入；63d2c9d 撤回排序、5cf1197 清 Go 註解都漏了這行 | S | CONFIRMED。只改註解 |
| A34 | PLAN「儲存格式」段仍是 v2（adr-7） | PLAN.md:39-62 寫 `kist/v2/*`、master AAD 綁 repo_id，清單沒有 touch/、`.r1`；format.md:77-80、91 是 v3 | 縮成指向 format.md v3 加三到五行摘要；format.md:246 的「參考實作」改「實作」 | 入口文件與權威規格矛盾；照 PLAN 寫錯常數會被 fixture_v3 擋下 | S | PLAUSIBLE。事實鏡頭駁倒 Go 因果：這是 v3 遷移漏改，只有段名幾個字是 Go |
| A35 | Entry 的 kind、mk、ct 是 u8 常數（format-2） | tree.rs:81-110；全零 subtree 當缺席（:145）；tree.rs:246、336 兩個 `unreachable!` | 兩個 `unreachable!` 改成 `return bad(..)`；需要時加只讀的 `fn node(&self) -> NodeKind` 檢視（wire 與接受範圍不變） | 讀碼：分支都被 repo.rs:398 的 validate 擋在前面，git 無 bug 史。探針證明 enum 版逐 byte 相同（611 bytes、fixture 10 棵樹） | S | PLAUSIBLE。值不值得被駁：enum＋Option 版拿不到宣稱收益、會改讀端接受範圍。Go 因果成立（08198b8 為對齊 Go 刪掉 enum）。【kist-format】 |
| A36 | 來源位置沒有型別（backend-7） | open_source 只認 sftp:// 與 s3://，其餘一律當本機路徑（source.rs:496-502） | 只修真缺陷：仿 lib.rs:133 拒絕未知 `xxx://`（約三行），CLI 的遠端判斷改成任何 `xxx://`；Root.path 維持原始字串 | 實測 cwd 有 `gopher:/x` 目錄時 `gopher://x` 開得起來；source.rs:572-575 的測試是碰巧綠 | S | PLAUSIBLE。值不值得被駁：型別化與 backup 單一輸入屬口味、要動 162 行呼叫 |
| A37 | 走訪用 Box::pin 遞迴（backup-2） | backup.rs:1193-1283、reach.rs:76-88、restore.rs:202-337；界限 MAX_TREE_DEPTH=256 | 不重寫。修正 lib.rs:116-117 不實的「debug 也在堆疊預算內」；要餘裕就在 main.rs 把 block_on 放到大堆疊執行緒 | 實測 debug 2 MiB 在 50 層就 stack overflow；release 255 層 2 MiB 可完成。產品在 main thread 跑（main.rs:282-293） | M | PLAUSIBLE。值不值得被駁：代價只在 debug 與測試；手寫堆疊要在寫入主路徑重排切段、prev 鏈 |
| A38 | Chunks 的兩條註解不變量（format-4） | take_buf 後不可迭代、出錯後「終止」只寫在註解（kist-chunker/src/lib.rs:309-310、331、479） | 改 lib.rs:309-310 的文件使之符合實際；`into_buf(self)` 可選。**不要**做出錯後 fuse | 實測中途 take_buf 後 next() panic（lib.rs:458）；唯一產品呼叫端第一個 Err 就 break | S | PLAUSIBLE。值不值得被駁：實測 fuse 會把「重試、內容完整」變成靜默截斷 |
| A39 | kist-crypto、kist-format 的死碼（format-6） | kist-crypto 為 TEMPORARY PoC 掛 zstd（Cargo.toml:18）；`CryptoError::Compression`、`KdfParams::default_params` 沒人用 | 只刪死碼：zstd 移 dev-deps（或連 poc_tree_naming.rs 一起刪，與 A17 重疊）、刪 Compression、刪 default_params | 實測刪掉後 workspace check、fixture_v3 過，Cargo.lock 不變 | S | PLAUSIBLE。值不值得被駁：口味；KDF 上限兩份檢查守不同信任邊界，合併會實測壞兩個測試。【default_params 在 kist-format】 |
| A40 | parity 的 key 定義兩份（format-5） | parity.rs:26、42 與 keys.rs:29、64；寫入用 keys::parity（backup.rs:1741），讀取與清掃用 parity::key（check.rs:300、prune.rs:687、743） | 刪 parity.rs 的 PREFIX 與 key()，統一用 keys.rs；tests/parity.rs:253 改斷言 keys::parity；可與 017 決定 2 改 prune 時一起做 | 實測兩個方向的漂移都被現有測試抓到；實測收斂後相關測試全過 | S | PLAUSIBLE。值不值得被駁：口味。Go 因果只到 API 形狀照抄（512640b）。【kist-format】 |
| A41 | mount 快取的「0＝8」註解（app-7） | corefs.rs:30-31 寫「0 = 8」，Lru::new 是 `cap.max(1)`（:197） | 只改註解：「最少 1 顆；預設 8」 | 實測傳 0 得 1 顆；只有 `MountConfig::default()` 被建構，使用者碰不到 | S | PLAUSIBLE。值不值得被駁：搬過來的只有 Go 的一行註解，NonZeroUsize 不划算 |
| A42 | mount 的 runtime 手動收尾（app-10） | kist-mount/src/lib.rs:117、124 手寫 `shutdown_background`，Drop 另一種寫法（:61-72） | 不改；要補就補「壞掛載點回 Err 而不 panic」的測試 | 探針證明 Drop 守衛可行；這段自 e6e1543 後沒動過 | S | PLAUSIBLE。值不值得被駁：假設性風險，已有註解與 ADR 015:45-48 |

## B 桶（格式層，併入 v4 清單）

**無。**

- adr、backend、history 三個 finder 各自找過，都沒有找到為遷就 Go 而選、至今仍
  影響 on-disk bytes 的設計。ADR 008 修訂（008:82-95）已逐條重審 v2，唯一的一項
  （CBOR 排序）當天撤回。其餘來自 Go 的格式內容都是凍結常數，依規則不算。v3 是
  Rust 先設計、Go 後移植（ADR 016:108-110）。
- 最接近的是 A13：改完之後邊角檔名寫出的樹 bytes 會不同，但那是修正今天的遺漏，
  仍是合法 v3，不需要升版。
- 所以 v4 清單不因本文增加任何項目。

## C 桶（017 等 ADR 裡要改寫理由的結論）

**C1　canonical rank 單一來源，以及 commit gate 誤拒（repo-1，CONFIRMED）**

- **原理由**：017 決定 2（017:165-167）「canonical rank 函數單一來源加向量：同一條
  規則在兩份實作裡以不同方式出錯」，舉 Rust 2026-09-13 與 Go 185ed78 兩件，綁在 v4。
- **拿掉 Go 後**：「兩份實作」的理由沒了，但 Rust 內部自己就有四套 chunk→位置的
  決勝規則：add_pack 取名稱最小、不看標記（index.rs:327-340，無快取的 load_index
  用它，repo.rs:529-541）；add_pack_ranked 取（是否標記, 名稱）（index.rs:348-363，
  只有 load_index_for_backup 用）；prune 另手寫一份（prune.rs:207、210）；快取重建
  留先出現者、增量合併新的贏（index.rs:99-103、cache.rs:166-167、283-296）。format.md
  §10 規定的是「未標記優先＋名稱最小」。backup 開始時用有 rank 的版本挑去重位置，
  commit gate（backup.rs:589）卻用 `self.load_index()` 重新解析，於是會解析到一顆
  被標記、已過 grace 的 pack 而拒絕 commit。
  實測：finder 與兩位複核者各自重現。以真 prune repack、不手放標記的探針：無快取
  時 8 回失敗 3 回，下一輪 prune 刪掉舊 pack 後自癒；離線 client 情境 8 回失敗 7 回，
  第二輪 prune 被活躍 client 規則擋住（blocked=4），重跑仍失敗，要等該 client 超過
  inactive_after（預設 30 天）才解開（時長 UNVERIFIED）。無快取不是冷門路徑：
  kist-app 的 cache_dir 預設 None（config.rs:54），CLI 有 `--no-cache`（main.rs:969）。
  把 backup.rs:589 改成 `load_index_for_backup(&marks.keys().copied().collect())`
  一行之後：32／32、12／12 全過，backup_gc_rules 10、gc_race 1、gc_touch_replica 4、
  fixture_v3 4 全過。方向是安全失敗（拒絕 commit，不遺失資料）。
- **結論變不變**：結論（rank 單一來源）不變，理由改成「Rust 內部重複，而且已實測
  誤拒」，並從決定 2 搬到決定 3：格式中立，現在就做。範圍是那一行修正、把探針轉成
  正式回歸測試、一個 rank 函式給 add_pack_ranked 與 prune 共用；HolderRank 大重構
  與統一快取規則部分是口味，快取的「新的贏」要不要改另案。另外，017:77 的 v4
  時間線假設「commit gate 確認 T 解析到 M2」，v3 的 chunk gate 目前做不到，採 v4
  之前就該有這一行修正。
- **複核**：CONFIRMED。Go 因果只到規則起源：名稱最小是 v2 與 Go 統一時定的
  （ADR 008 決定 8）；gate 用未排名 loader 不是 Go 造成（185ed78 反而寫 Go 的 gate
  「與 Rust 的 load_index 相同」）。改走 load_index_for_backup 對有快取的部署多一次
  完整 index 讀取，效能與記憶體 UNVERIFIED（未量測）。

**C2　v4 的主要數據「四件在樹的 mtime 機制，兩件出在 Go 端」（history-9，PLAUSIBLE）**

- **原理由**：017:25-28，六件資料遺失方向的 bug 有四件在「樹的存活靠後端 mtime」
  這一塊，其中「兩件出在 Go 端」，用來支撐 v4。
- **拿掉 Go 後**：事實查證成立：c30fd81 只改 Go（Rust prune 從 38074ee 起就是
  `>=`）；SFTP 零 mtime 在 Go 是真 bug，在 Rust 是沒爆的同型隱患（2d357b5、
  PLAN.md:423-426）；另兩件是設計審查攔下與設計層視窗。這四件裡，Rust 端沒有一件
  真的觸發過。
- **結論變不變**：不變。值不值得的鏡頭推翻「應重寫理由段」：017 與 Go 移除是同一個
  commit（5cf1197）寫成的，理由句本身就寫了「兩件出在 Go 端，但它們全在同一個機制
  裡」；機制複雜度的論點與語言無關；同一機制在 Rust 也出過 51be5ed（ADR 018 唯一
  「沒有 Go 抓不到」的案例）與 2d357b5；ADR 018:119-121 的三條規則拿掉後整個套件
  仍全綠。Go 移除去掉的是唯一抓過 Rust 在這個機制犯錯的管道，**加重**而不是削弱
  v4 的理由。可選的小改：理由段註明那兩件是 Go 單邊的實作錯誤，並把 018 的變異
  存活列為 Rust 端的證據；另註 SFTP 零 mtime 這一類在 v4 仍影響 pack 的標記年齡
  （017 決定 2 已保留四處 mtime）。
- **複核**：PLAUSIBLE（值不值得的鏡頭駁倒：照原提案改寫，會讓負責人低估一套沒被
  測試釘住的 mtime 機制）。

**C3　以「跨語言向量」「兩個實作」為理由的保留、拒絕與條件（adr-8，PLAUSIBLE）**

- **原理由**：017:38 保留密碼學、CBOR 等「有跨語言向量與長時 fuzz」；017:39
  「tree-canonical 向量原樣存活」；017:199 拒絕 Entry core＋blob「會作廢 tree-canonical
  向量」；017:112-114 條件 5「已移除的 Go 實作是列 packs/ 並逐顆讀 trailer……把 Rust
  的做法寫進規格並掛向量」；ADR 016:15-17 與 format.md:19 的 stats 判準「兩個實作對
  同一棵樹會數出同一組數字」；snapshot.rs:14、165 仍寫「兩個實作」；017:131、222
  「省重錄金鑰向量」。
- **拿掉 Go 後**：保護來源換成凍結 fixture（ADR 018 反向對照表：CBOR 欄位改名、
  CTX_HASH_KEY 各讓 fixture 3 項紅）、長時 fuzz 與無 bug 史。tree-canonical.hex 依
  018 決定 4 留作凍結 golden，017:39 那句到今天仍成立；017:199 的主理由是「純口味」，
  而那個改動會連 v3 fixture 一起作廢，拒絕理由只更強。stats 判準的另一條理由（計數
  依 GC 狀態與去重順序而變）本來就與 Go 無關。「省重錄金鑰向量」與 Go 無關（見
  待決 4）。
- **結論變不變**：全部不變。值得動的只有：snapshot.rs:14、165 改直陳規則（018
  決定 5 的收尾雜務）；017:38 的「跨語言向量」在負責人裁定 017 時順手換成「凍結
  fixture」。條件 5 的缺口是真的（format.md §10、§13 沒寫持有者集合取自 index blob、
  phantom 不參加選正本），可以不等 v4 先寫進規格，但要連 young-object／grace 規則
  一起寫，而且 018:100-101 要的是向量，不要改成只靠整合測試。016 與 format.md
  §0／§19 受 018 決定 6 保護，只能加後記。
- **複核**：PLAUSIBLE（值不值得的鏡頭駁倒：成本只在用字，finder 自評口味；5cf1197
  已清掉多數位置）。【kist-format 的註解與規格文字，照 PLAN.md:85 要負責人確認】

**C4　format.md §18「每條規範性規則至少掛一個向量」的前提（adr-2，PLAUSIBLE）**

- **原理由**：§18 的要求源自跨語言（e0b7c63 原文「跨語言 conformance 向量（兩邊
  testdata 各持一份……）」、format.md:26 的 §0.9、ADR 016:108-110「Rust 錄、Go 對」）；
  018:100-101 把「把 §18 的向量生成出來」當成失去第二個規格讀者的對策。
- **拿掉 Go 後**：產品自己錄的向量是回歸 golden，補的是規則覆蓋，補不回第二個讀者。
- **結論變不變**：§18 的要求不變。值不值得的鏡頭推翻 finder 的「撤掉逐條向量要求、
  改分兩類」：fixture 只有本機 posix 條目，碰不到 STATS、FAST、mk=1／2／3、`s3://`
  映射，這些規則在 byte 層只有產品錄的向量守；018:119、123 記錄的 V3-GC-1、V3-FAST-2
  變異存活，正是撤掉要求會放棄的地方；「照規格手算」由同一作者做，與 Go 有同樣的
  非獨立問題。只改 018:100-101 一句：「產品錄製的向量補的是規則覆蓋；第二個規格
  讀者目前沒有替代」。另可順手把 golden/*.cbor 等既有檔標上 V3 ID（S，017:133-136
  已提）。
- **複核**：PLAUSIBLE（值不值得的鏡頭駁倒：主要提案對資料安全淨負）。

**C5　ADR 015「對齊 Go」的 mount 語意（app-12，PLAUSIBLE）**

- **原理由**：ADR 015 §1「佈局與快取（對齊 Go 參考實作）」（015:15）、§5「語意
  取捨（對齊 Go）」（015:57）列出唯讀、硬連結各自成檔（nlink=1）、LRU 8 顆、inode
  永不重用；015:74 的 `#[cfg(unix)]`「與 Go 的 build tag 同步」。fuse.rs:63 的 nlink
  註解是 Go setAttr 註解的逐字翻譯。
- **拿掉 Go 後**：各條本來就有自己的理由：唯讀（mount 不寫 repo）、TTL（015:19-20，
  內容定址）、LRU 8 顆＝64 MiB 上限（corefs.rs:30-31）、`#[cfg(unix)]`（fuser 不支援
  Windows，015:97）。nlink=1 的理由是範圍取捨「mount 服務 bytes 與 modes，不做 inode
  身分」，不靠 Go。inode 永不重用與程式不符，另見 A6。
- **結論變不變**：不變。018 決定 6 規定 001–016 不改寫，要修訂就另寫新 ADR。硬連結
  共用 ino 是功能需求而非債；照 finder 原提案以（dev, inode）當鍵會跨 snapshot 撞號：
  實測就地改寫的硬連結檔（dev, ino, nlink）不變、內容已不同，kernel 又快取 24 小時，
  可能拷出另一個 snapshot 的 bytes（UNVERIFIED，未實作）。真要做，鍵至少要含
  snapshot。需要重建連結的使用者應該用 restore（restore.rs:161-163、234 已做）。
- **複核**：PLAUSIBLE（值不值得的鏡頭駁倒：文件那半已被 018 決定 6 排除，程式那半
  沒有需求支持，而且原提案有正確性風險）。

## 不是 Go 包袱（容易誤判的）

### 凍結的格式常數（依規則不算）

- FastCDC gear 表與邊界函式（format.md §12）、Reed-Solomon 矩陣與 parity sidecar
  排版、CBOR 欄位表順序、`kist/v3/*` 派生字串、AAD 規則。
- 同樣凍結、在 Rust 端沒有可觀察成本的：pack 頭尾 magic 與大端序（ADR 008 決定 6）、
  snapshot key 的時間格式、後端 mtime 截秒（format.md §13）。改它們只會作廢 fixture。

### 出處在 Go，但理由自足

- `KIST_SFTP_*` 環境變數名（sftp.rs:163-179）：使用者介面，改名只有遷移成本。
- ADR 013 §2 原子條件寫入、§4、§118 list 全量；ADR 009 §4 的 sidecar 語意：各有與
  Go 無關的理由，重新推理結論不變。
- sftp URL 手寫解析（sftp.rs:84-146）：「@ 只在 authority 才算 userinfo」符合
  RFC 3986，換 url crate 反而會改行為。
- 解碼前的結構守門 reject_noncanonical（cbor.rs:65-142）：原動機有「同一份 bytes
  兩個實作不同判斷」，但 format.md §4 第 8 條、敵意 repo、64 層深度上限都自足。
- `clock_skew: Option<Duration>`（缺席＝預設、0＝字面值）：比 Go 的零值語意好，
  是 Rust 原生寫法。

### finder 說是 Go 造成、複核推翻的

| 條目 | finder 的說法 | 複核結果 |
| --- | --- | --- |
| A10（format-1） | 驗證邊界照 Go 的 Encode／load／ReadTrailer 擺 | Go 反而把驗證綁在格式層入口（tree.go:451-453、475-500；readTrailer；ChunkList 在 read.go、restore.go 都比版本）。Rust 的三段分離是自己的 crate 切法；commit 裡的「Go 同款」是 Go 當對照組抓到漏洞 |
| A30（repo-5） | symlink 擋路報 Corrupt 與 Go 同源 | Rust 在 08198b8（09-06）就用 Corrupt；Go 的 ErrCorrupt 在 bdabd4b（09-19）才出現；非目錄擋路 Go 回的是一般錯誤 |
| A21（adr-1） | UPDATE_VECTOR 自我改寫是為 Go 錄製而設 | 同款的 UPDATE_GOLDEN 在 8e79abd（M1-a，09-04）就有，是 Rust 原生慣例；xattr、nlink 也由 golden.rs 守 |
| A34（adr-7） | PLAN 儲存格式段是 Go 包袱 | v3 遷移漏改（9f0290c 只改了 PLAN:11），只有段名幾個字是 Go |
| A37（backup-2） | MAX_TREE_DEPTH=256 是為與 Go 一致 | 81435b7 兩邊同時引入，lib.rs 的理由寫的是堆疊預算；Go 關聯只剩兩邊同步的鎖步約束 |
| adr-6（駁回） | 關 default features 的重驗成本因 Go 而高 | 9abc1ab 當時 parity golden 測試已在 Rust 端，重驗一直是一次 `cargo test` |
| C3 第 (5) 點（adr-8） | 「省重錄金鑰向量」靠 Go | Go 的金鑰向量從沒到 v3（仍是 `kist/v2/*`），要重錄的四行 hex 是 Rust 的 poc_keys.rs:56-59 |
| C2（history-9） | Go 端那兩件 bug 是移植產物 | c30fd81 與 SFTP.Stat 漏設 Modified 都是 Go 自己的錯 |
| A4（repo-3），部分 | Rust 少了 Go 的「目標必須為空」前提 | 演算法照 Go 移植屬實；但 Go 同樣以完整路徑開檔，中間段 TOCTOU 在 Go 也存在，空目標只縮小攻擊面 |

### Go 做對、Rust 沒跟上（移植遺漏，不是包袱）

- A5：Go 把 `[prune].grace` 轉給 backup 閘門（run.go:212-221）。
- A12：Go 在 repo 層驗 0..=8（7cd0456）。
- A7：Go 的 SourceItem 在列舉時就帶 Posix。
- A6：go-fuse 在 FORGET 時回收節點。
- A29：Go 的 check 用 `readData || repair` 決定摘要。
- A10：Go 的兩個 ChunkList 解碼點都比版本。
- A25：Go 把 `--once` 的分類放在函式庫 run.Once，並有測試（ac682f2）。

## 考慮過但駁回

### REFUTED

| 條目 | 提案 | 駁回理由 |
| --- | --- | --- |
| adr-6 | 關掉 reed-solomon-erasure 的 default features，刪 deny.toml 對 RUSTSEC-2024-0384 的 ignore，改寫 PLAN.md:374-381 | 兩個反駁者都成立。實驗本身屬實（instant 消失、parity 8 項與 fixture_v3 全過），但 Go 因果不成立：9abc1ab 當時重驗就只是一次 `cargo test`，拿掉 Go 沒改變成本；代價只是 deny.toml 一條 ignore，PLAN 列的三個移除條件都沒達成，也不是 Rust 程式設計。可另列雜務由負責人自行決定 |

### 去重時因與 ADR 017 重複而丟掉的

無（`[]`）。

### 原提案裡會變差、不要照做的部分（有實測或讀碼證據）

- A8：check 的兩處用 `!exists()?`——一次暫時性錯誤就丟掉整份報告，連資料校驗與
  parity 修復都跳過。
- A5：「兩者都設且不同就報錯」——擋掉安全方向的設定，實測讓既有測試失敗。
- A15：SnapshotKey 解析失敗就靜默略過——實測 snapshot 被改名後 prune 由拒絕變成
  寫出 6 個 gc 標記，check 看不到，之後還原 6 個檔全失敗。
- A38：出錯後 fuse——實測 `.flatten()` 由內容完整變成靜默截斷（225100／1000000）。
- A20：get_ranges 改走 trait 預設——實測 start 越界會 panic。
- A39：kdf() 改呼叫 KdfParams::validate——實測兩個測試失敗、錯誤變成
  BadWrappedPayload。
- A23：整個走訪搬到 blocking 執行緒——block_on 從 3 處變十幾處，而且在資料寫入主路徑上。
- A24：HumanDuration newtype——探針證明擋不住同一類錯。
- C4：撤掉「每條規則至少一個向量」——放棄 fixture 碰不到的規則的唯一 byte 層防護。
- C5：以（dev, inode）共用 ino——可能跨 snapshot 拷出錯的 bytes。
- C2：把 v4 理由改成強調「Rust 沒出過事」——低估一套沒被測試釘住的機制。

## 複核順帶發現（不在三桶內）

- restore 會套用 setuid 位（fsmeta.rs:278 的 `mode & 0o7777`），但從不 chown
  （fsmeta.rs:4「M1 不做」）：root 還原攻擊者擁有的 4755 檔，會得到 root 擁有的
  setuid 檔。Go 在 restore.go:431-447 先 chown 再 chmod。UNVERIFIED（需要 root，
  只讀碼）。複核者認為優先序高於 A3，建議另立一條。
- format.md:283-284 與 vpath.rs:53-56 說 `s3://bucket/` 切不出組件、內容直接落在
  target；探針顯示兩份實作都切出 `["bucket"]`。文件與程式不符，改哪一邊交負責人
  （A14 合併時要保留現行程式行為）。
- fsmeta.rs:265-266 的註解說 set_file_times「不需要打開檔案，0o000 目錄也設得了」；
  filetime 0.2.29 會先 open，實測對既有的 0o000 目錄回 EISDIR。
- kist-core/src/lib.rs:8 寫「所有 I/O 是 async（tokio）」，與 LocalSource 的同步
  syscall 不符（A23）；lib.rs:116-117 的堆疊預算說法在 debug 下不成立（A37）。
- ADR 005 §8「不一致是安全失敗那一邊」與 §6b 矛盾（A5）。
- HEAD 的 `cargo deny check advisories` 目前是紅的：2026-09-27 執行（cargo-deny
  0.20.2），`error[vulnerability]` RUSTSEC-2026-0285「TLS 1.3 handshake messages
  incorrectly accepted across encryption level boundaries」，rustls v0.23.43 經
  hyper-rustls → reqwest 進 kist-app、kist-backend，advisory 給的解法是升到
  `>=0.23.45`（`cargo update -p rustls`），exit 1。對 kist 實際可利用性 UNVERIFIED
  （未讀 advisory 全文）。**已修**：e9e06e6 升到 0.23.45，deny 四項 ok。

## 這份盤點的假設

| 假設 | 標記 | 依據 |
| --- | --- | --- |
| 目標順序：資料安全 > 還原可靠 > 抗勒索 > 效能 > 功能數量 | stated by user | PLAN，017 沿用 |
| 負責人不熟 Rust，程式要保守、直白 | stated by user | PLAN.md:5 |
| 技術選型固定；kist-format 改動要負責人確認 | stated by user | PLAN.md「技術選型」、PLAN.md:85 |
| kist 還沒上線、沒有真實使用者資料 | stated by user | 017 背景的問題設定 |
| fixture_v3 是格式的唯一閘門，而且確實會因格式變動而失敗 | measured | ADR 018 反向對照表；本輪多位複核者的變異 |
| A 桶各條「改完之後 fixture_v3 仍會過」 | 多數 guessed | 真的在 scratch 副本套用改動並跑過 fixture_v3 的只有：A10 的 ChunkList 檢查、C1 的一行修正（寫入前另重跑過）、A18、A38、A39、A40（A35 只驗了逐 byte 重編碼）；其餘是讀碼推論 |
| 複核者留下的原始輸出可信 | A1 機制與 C1 measured；其餘 guessed | 寫入前只獨立重跑了這兩條（見「做法」），其餘只核對了部分 file:line |
| 對外 JSON／webhook 沒有外部消費者依賴欄位名、形狀或 key 順序 | guessed | workspace 版本 0.1.0、本地 tag 只有 m1–m5；GitHub 上有沒有 release：UNVERIFIED |
| A4 的競態在真實部署可被利用 | 同 uid measured；跨使用者 guessed | 跨使用者需攻擊者對目標之下有寫權限，未實測 |
| 效能影響小（雙重 lstat、gate 改讀完整 index、合併壓縮框架） | guessed | 只有 lstat 量過（每次約 1.1 µs）；其餘未量測 |
| 名稱以 `~` 結尾等特殊字元在真實來源常見 | guessed | 編輯器備份檔的命名慣例，未統計 |
| scratchpad 共用造成的污染已排除 | measured | 撞到污染的複核者都在獨立副本重跑並註明 |

## 待負責人決定

1. **A 桶先做哪些、依什麼順序**（建議，每項先寫會紅的測試）：
   - 第一批（小、資料安全方向）：A1 遠端來源編碼；A5 grace 轉發（有方向的檢查）；
     C1 的一行修正加回歸測試；A8 check 三路 match 與 prune.rs:758；A6 mount inode
     去重；A10 的 ChunkList 版本與 trailer 共用解碼；A12 parity 入口檢查；A11 密碼檔
     （以 kist-app 規則統一，附遷移說明）。
   - 第二批（restore 一條線）：先定第 5 點的 restore 契約，再做 A2＋A3（暫存檔、
     fd 套 metadata、rename）；之後決定 A4（L，需同意 rustix 直接依賴）。
   - 第三批（backup 與監控）：A7（先做 S 的最小修法，結構修法等 ADR 011 bench）；
     A9；A13（在 A1 之後，先補 Docker-gated 的 SFTP 來源測試）；A14。
   - 第四批：第二級 A15–A26，多半是把複核者已寫好的探針轉成正式測試。
   - 第五批：第三級 A27–A42 零碎做；被駁的 PLAUSIBLE 項只做表中縮小後的殘項，
     或不做。
2. **B 是否併入 v4**：本輪沒有 B，v4 清單不變。
3. **ADR 017 待決 1（是否採 v4）**：本盤點不改變它的論證，只補三點：(a) 理由段的
   「兩件出在 Go 端」不削弱 v4，Go 移除反而拿掉了唯一抓過 Rust 在該機制犯錯的管道
   （C2）；(b) C1 顯示 v3 的 commit gate 做不到 017:77 時間線假設的「確認 T 解析到
   M2」，不論採不採 v4 都先修；(c) canonical rank 單一來源不必等 v4。採不採仍由
   負責人決定。
4. **ADR 017 待決 3（派生字串留 `kist/v3/*` 或改 `v4`）**：「省重錄金鑰向量」這個
   理由從來就與 Go 無關——Go 的金鑰向量一直停在 `kist/v2/*`，要重錄的是 Rust
   poc_keys.rs 的四行 hex——所以 Go 移除不改變它的份量，它本來就小。A17 做完之後
   向量經 kist-crypto，重錄只是改一個測試檔。剩下要權衡的是「字串與版號一致」對
   「升版後金鑰分屬不同域」的價值，後者沒人分析過（UNVERIFIED）。派生字串是凍結
   常數，改它會改 bytes，只能隨 v4。
5. **其他要負責人點頭的事**（都不是技術選型解鎖）：
   - kist-format 改動（PLAN.md:85）：A10、A35、A39、A40；A33 與 C3 只改註解與規格
     文字，照字面也算。
   - 新增直接依賴 rustix（A4；已是間接依賴）。
   - restore 契約：擋路的 symlink 改成「取代」還是維持「拒絕」（A2）；是否恢復
     「目標必須為空」（A4 的替代案，會改 restore_twice_into_same_target_succeeds 與
     CLI 說明）。
   - 對外 JSON：A27 欄位改名、A28 forget 形狀（修訂 ADR 006 §1）、A9 的 key 順序；
     要不要過渡期。
   - ADR 018 決定 4：A17 若刪 chunker 的 interop_key_derivation 或改名 `interop_*`。
   - ADR 017 草稿的改寫：C1 把 rank 從決定 2 搬到決定 3；C3 的條件 5 是否先寫進規格。
   - 行為變更的公告：A5 之後，只縮短 `[prune] grace` 的使用者會開始遇到
     BackupTooLong；A11 之後 CLI 讀密碼檔的規則改變。
   - 複核順帶發現的「setuid 但不 chown」要不要另立一條。

## 裁定（2026-09-27）

負責人對上面五節待決的回覆是「全做」。依此：

1. **A 桶全部做**，一律照「提案（複核後）」的版本，不照 finder 原稿；「原提案裡會變差、
   不要照做的部分」一條都不做。複核結論是「不改、只補測試或註解」的 PLAUSIBLE 項
   （A22、A24、A25、A26、A37、A41、A42 等），就只做那個殘項。每項先寫會紅的測試
   （能寫的話），一項一個 commit，fmt／clippy／nextest 全綠才 commit。
2. **C 桶**：C1 的一行修正、回歸測試、rank 單一函式照做，ADR 017 把 canonical rank
   從決定 2 搬到決定 3（017 仍是草稿，可改）；C2 在 017 理由段加註；C3 做
   snapshot.rs 註解、017:38 用字，以及把 prune 持有者集合的規則（連同 young-object、
   grace）寫進 format.md 並掛向量；C4 改 018:100-101 那一句；C5 不動（018 決定 6）。
3. **新增 A43：restore 的 setuid／setgid**（原「複核順帶發現」第一條，升格）。
4. rustls 已升（e9e06e6）。

負責人沒有逐條回答的選擇題，採以下預設（**代為選定**，負責人可隨時推翻）：

| 問題 | 選定 | 理由 |
| --- | --- | --- |
| A2：目標處擋著一個 symlink | 維持「拒絕」：回報錯誤、**不刪** symlink、不跟隨、不寫檔 | 現行契約與 restore_hardening 測試都是拒絕；今天的 bug 是失敗後把 symlink 刪掉，修那個就好。rename 前的 lstat 仍有視窗，但輸掉競態的後果只是取代 symlink 這個目錄項本身（rename 不跟隨），不會寫到外面 |
| A4：fd 錨定 vs 恢復「目標必須為空」 | fd 錨定；rustix 改成 kist-core 的直接依賴；**不**恢復空目標規則 | 空目標只縮小攻擊面、關不掉它（C 節複核）；恢復它會改產品契約 |
| A43：setuid／setgid | euid 為 0 時：entry 帶 uid／gid 就先 fchown 再套完整 mode；沒有 uid／gid 或 fchown 失敗，就清掉 0o6000 並記節點錯誤。非 root 維持現狀 | 與已移除的 Go restore.go:431-447 同順序（先 chown 再 chmod）；非 root 還原出的檔屬於還原者本人，setuid 指向自己不構成提權 |
| A9／A27／A28：對外 JSON 改形狀 | 直接改，不設過渡期 | 版本 0.1.0、未發佈（guessed，見假設表）；改動寫進 commit 訊息與 README |
| A11：密碼檔規則 | 以 kist-app 的規則統一（去結尾 `\r`），README 補遷移說明 | 反方向會讓 daemon 寫下的 repo 打不開 |
| A17：chunker 那份重複的金鑰向量 | 刪重複計算與只為它存在的 dev-deps；向量檔與 `interop_*` 測試名不動 | 018 決定 4 保護的是向量檔與名稱，不是重複的算法 |
| `s3://bucket/` 的落點：文件與程式不符 | 改文件（format.md:283-284、vpath.rs:53-56）以符合程式 | 既有 repo 是照程式行為寫的；s3 不在 fixture 內，改程式就是改既有 snapshot 的還原落點 |
| ADR 005 §8 與 §6b 矛盾 | 不改 005（018 決定 6：001–016 不改寫），矛盾記錄於此 | — |

