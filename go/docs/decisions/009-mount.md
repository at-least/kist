# 009 — mount：唯讀 FUSE

- 狀態：已接受（M4）
- 日期：2026-09-05
- 相關：`internal/mount/`、`internal/repo/read.go`、`internal/cmd/mount_unix.go`

## 問題

`PLAN.md` M4 要 `mount`（FUSE，先 Linux/macOS）。格式 v1 凍結了，而 v1 有一個對隨機讀取不利的事實：**tree entry 只記 chunk ID 的列表，pack entry 的 `Length` 是密文長度**，哪裡都沒有 chunk 的明文長度。`ReadAt(off)` 要知道 `off` 落在哪個 chunk，只能把前面的 chunk 一路解到那裡。

## 決策

### 1. 前綴和表，懶惰地填

每個檔案節點有一張 `ends[i]`（chunk i 的明文結尾偏移）表，解到哪填到哪，所有打開這個檔的 handle 共用。循序讀（`cat`、`cp`、`diff`、kernel readahead）本來就依序碰到 chunk，多付零成本；往後大跳一次要解掉中間的 chunk，之後表就會回答。代價說白：**讀一個 10 GiB 檔案的最後一個位元組，要解一次 10 GiB**。v2 的修法是在 chunk ID 旁邊記明文長度；v1 不加欄位。

### 2. 一條解密路徑

`repo.ChunkSource` 從 restore 的 `chunk()` 抽出來：index 查 pack、每個 pack 一個開著的 reader（有鎖，開 reader 在鎖外）、`pack.Reader.Chunk` 解密解壓。restore 改成用它，mount 也用它。沒有第二份解密程式碼。

### 3. 解碼後的 chunk 放一個小 LRU

預設 8 個，chunk 上限 8 MiB，所以上限 64 MiB。跨檔案共用；同一 chunk 被兩個檔引用時只解一次。

### 4. 不變的東西告訴 kernel 它不變

snapshot 底下的所有東西都是不可變的：entry 與 attr 的 cache timeout 給 24h。最上面兩層（client、時間戳）每次 Lookup/Readdir 都重新列，timeout 1s。這是免費且最大的效能槓桿。

### 5. mount 之後才落地的 snapshot

mount 時載入的 index 不認識之後寫進來的 pack。最上面兩層每次列舉時記住看過的 snapshot key，看到新的就先 `repo.Refresh`（重載 index blob）再服務。`TestMountSeesANewSnapshot` 用另一個 repo handle 在 mount 之後備份，從掛載點讀回來逐位元組相同。

### 6. mode 轉換是個陷阱，所以有專門的測試

`Entry.Mode` 是 Go 的 `fs.FileMode`（型別在高位、setuid 等是旗標）；FUSE 要 `S_IFREG|S_IFDIR|S_IFLNK` 加 `S_ISUID/S_ISGID/S_ISVTX` 加權限。一個函數、五個案例（0644、setuid、setgid、sticky 目錄、symlink）。

### 7. 其餘

- attr 來自 entry：size、mtime、ctime、uid、gid。`Nlink` 一律 1：硬連結呈現為各自獨立的檔案，mount 服務的是位元組與 mode，不是 inode 身分。
- xattr 透過 `Getxattr`/`Listxattr` 原樣提供——資料在那裡，mount 講的是保真。
- 唯讀：`O_WRONLY`/`O_RDWR` 回 `EROFS`；mount option `ro`。沒有 `--allow-other`。
- `mount` 指令只在 `linux || darwin` 的檔案裡註冊（`platformCommands`），Windows build 不含 `internal/mount`。
- 依賴 `hanwen/go-fuse/v2`，純 Go，`CGO_ENABLED=0` 仍然成立。

## 測試

`TestMountServesTheSnapshot`：真的掛載（`/dev/fuse` 打不開就 skip 並說原因），走遍來源樹比對每個檔案的內容、mode（含 setuid/sticky）、mtime、size；symlink 的 target；6 MiB blob 上六組 `ReadAt`（最後一位元組、中間、回到開頭、跨 chunk 邊界、越過結尾）；寫入被拒；不存在的名字與 client 是 `ENOENT`。unmount 在 `t.Cleanup` 裡最先註冊，失敗就退回 `fusermount3 -u`——洩漏的 mount 會讓 `TempDir` 刪不掉，毒到之後每一次測試。全部在 `-race` 下跑。

## UNVERIFIED

- macOS：只有交叉編譯與 vet；需要 macFUSE 才能跑。
- ~~CI 的 ubuntu runner 有沒有 `/dev/fuse`~~：推上去後確認了：ubuntu runner 有，FUSE 測試在 CI 真的跑並通過（run 33903651553）；macOS runner 沒有 macFUSE，那邊 skip。macOS 上的 mount 仍然沒有驗證過。
