# 004 — Tree、Snapshot 與命名

- 狀態：已接受（M1，格式凍結）
- 日期：2026-09-04
- 相關：`internal/tree/`、`internal/snapshot/`、[docs/format.md §6–§7](../format.md)

## 決策

### 1. Tree 以**明文** hash 命名，pack 以**密文** hash 命名

看起來不一致，其實是同一條規則的兩面：**名字要跟著「不變的東西」走。**

- pack 的內容就是它的位元組。密文 hash 剛好給了「不持金鑰也能驗證」這個額外好處。
- tree 的內容是那個目錄。密文每次密封都不同（nonce 是隨機的），所以**沒改過的目錄每晚都會換一個名字**，「未變動的子樹整棵重用」——這個專案儲存效率的核心——就永遠不會發生。

所以 tree ID = keyed BLAKE3(hash key, canonical CBOR 明文)，AAD 用這個 ID。keyed 而非 unkeyed，理由跟 chunk ID 一樣：能猜到目錄內容的攻擊者不該能用 repo 裡的名字確認猜測。

`TestUnchangedTreeKeepsItsName` 用兩個完全不同的 nonce 來源存同一個目錄，證明得到同一個名字。

### 2. Entry 編碼前排序，而且 `validate` 會拒絕沒排序的物件

排序讓「一個目錄」對應「一種編碼」對應「一個名字」，跟 client 用什麼順序走它無關。兩個 client 若因為 `readdir` 順序不同而算出不同的 tree ID，去重會在第一個目錄就停住。

`validate` 除了排序，還拒絕：空名稱、`.`、`..`、含 `/` 或 NUL 的名稱、重複名稱、沒有子樹的目錄、帶 chunk 的目錄或 symlink、沒有目標的 symlink、帶子樹或目標的檔案、未知型別。這些物件都能正確解碼，但都不能表示任何東西。

### 3. Chunk list 內嵌，並記下它何時會不夠用

100 GiB 的檔案 ≈ 5 萬個 chunk ID ≈ 1.6 MiB，tree 扛得住。restic 這樣跑了十年。

不夠用的情境是**一個目錄裡有很多超大檔案**：動一個檔就要重寫一個很大的 tree。那是 tree v2 加一層 indirection blob 的時機。現在**刻意不做**——為一個還沒出現的使用模式增加一層間接，是拿確定的複雜度換不確定的收益。寫在這裡，是為了讓它是一個已知的取捨，不是一個驚喜。

### 4. Snapshot 的 key 格式：定寬、無冒號

`20060102t150405.000000000z`。

- **定寬**所以字典序就是時間序，列表不必先全部解析再排序。
- **沒有冒號**所以在 Windows 上是合法檔名。RFC 3339 的 `2026-01-02T03:04:05Z` 會讓本機 repo 在 Windows 上直接壞掉，這不是理論問題。

### 5. Snapshot 的寫入是條件式的，撞了就往後推一奈秒

這是 repo 裡**唯一順序有意義、也唯一不能默默覆蓋**的寫入。兩個 client 剛好選到同一奈秒，那是兩次備份，不是一次蓋掉另一次。上限 1000 次重試：撞到那個數字代表出問題的不是碰撞（時鐘卡住了，或有人在灌），失敗比繼續迴圈好。

AAD 是完整的 key path。所以 snapshot 物件被搬到另一個 client 的命名空間、或改成另一個時間，就打不開了。

`Load` 還會檢查「key 說的 client 和時間」與「物件內容說的」是否一致。**key 只是名字，物件才是紀錄**；只信一邊的 reader 可以被一次改名騙過。

### 6. clientID 是持久化的隨機值，不是 hostname

hostname 在一個機群裡會重複、改機器名就會變、而且會把不必要的資訊寫進 repo 的檔案列表。改用 16 B 隨機值，存在 `$XDG_CONFIG_HOME/kist/clients/<repoID>`——每個 repo 一個，所以一台機器在 A repo 的身分不會洩漏它在 B repo 的身分。hostname 改放在 snapshot 內容裡當人看的標籤。

**已知代價**：每次都從空白檔案系統啟動的容器，每跑一次就鑄一個新 client，repo 會累積一堆只有一個 snapshot 的 client。這正是 M3「所有已知 client 在標記後都有新 snapshot」條件的難處。M3 的答案會是「超過 N 倍 grace period 沒有新 snapshot 的 client 視為已遺忘」。`--client-id` 提供給容器場景明確指定。

### 7. 跳過的檔案型別會發出警告

socket、FIFO、device node 被跳過。忠實還原它們需要還原程序不該假設有的權限，而且它們的「內容」從來不是使用者想存的東西。

但**跳過一定要說**。備份工具默默做得比它宣稱的少，比大聲失敗更糟。`BackupOptions.Warnf` 是這個目的，`TestBackupSkipsUnsupportedFileTypes` 檢查警告確實有提到那個檔案。

同樣的理由，`restore` 對「以目前權限設不了 ownership」、「xattr 還沒實作」、「硬連結退化成複製」都會警告而不是靜默。

### 8. 檔案大小記錄的是「實際讀到的位元組」

不是 `stat` 當下看到的大小。備份途中還在長大的檔案，如果記 `stat` 的大小，chunk 覆蓋不到那麼多，還原時會得到一個**短檔案而且沒有任何抱怨**。記實際讀到的長度，還原時 `written != entry.Size` 就是硬錯誤。
