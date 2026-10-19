//! 測試用的 SFTP v3 伺服器，只有讀檔用到的請求（INIT、OPEN、READ、CLOSE）：經 `tokio::io::duplex` 接上
//! `Sftp::new`，不用網路與容器。用來造出測試容器（OpenSSH）不會給的回應，例如 READ 少送；也記下同時在等的
//! READ 最多幾個（管線化）。

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt, WriteHalf};

const INIT: u8 = 1;
const OPEN: u8 = 3;
const CLOSE: u8 = 4;
const READ: u8 = 5;
const VERSION: u8 = 2;
const STATUS: u8 = 101;
const HANDLE: u8 = 102;
const DATA: u8 = 103;
const FX_OK: u32 = 0;
const FX_EOF: u32 = 1;

/// 假伺服器怎麼回 READ。
pub(super) struct Replies {
    /// 每次最多送這麼多 bytes（不管要了多少）：伺服器自己的上限。
    pub cap: usize,
    /// 每第幾個 READ 只送要的（截過 `cap` 之後）的一半；0 是不會。
    pub short_every: usize,
    /// 收到這麼多個 READ 才照順序一起回；或是 100 ms 沒有新的請求。1 是收到就回。
    pub hold: usize,
}

/// 起一個只有一個檔案的假伺服器（任何 OPEN 都開它），回連到它的 `Sftp` 與「同時在等的 READ 最多幾個」。伺服器
/// 不宣告任何擴充，用戶端因此一次要 20 480 bytes（OpenSSH 的預設）。
pub(super) async fn connect(
    content: Vec<u8>,
    replies: Replies,
) -> (openssh_sftp_client::Sftp, Arc<AtomicUsize>) {
    let (client, server) = tokio::io::duplex(1 << 20);
    let most_waiting = Arc::new(AtomicUsize::new(0));
    tokio::spawn(serve(server, content, replies, Arc::clone(&most_waiting)));
    let (read, write) = tokio::io::split(client);
    let sftp = openssh_sftp_client::Sftp::new(write, read, Default::default())
        .await
        .unwrap();
    (sftp, most_waiting)
}

/// 一個 READ：(id, offset, 要的長度)。
type Waiting = Vec<(u32, u64, usize)>;

async fn serve(
    io: tokio::io::DuplexStream,
    content: Vec<u8>,
    replies: Replies,
    most_waiting: Arc<AtomicUsize>,
) {
    let (mut rd, mut wr) = tokio::io::split(io);
    // 讀封包放在自己的 task、經 channel 交過來：等新請求的逾時才不會切掉讀到一半的封包。
    let (packets, mut incoming) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    tokio::spawn(async move {
        while let Ok(len) = rd.read_u32().await {
            let mut body = vec![0u8; len as usize];
            if rd.read_exact(&mut body).await.is_err() || packets.send(body).is_err() {
                return;
            }
        }
    });
    let mut waiting = Waiting::new();
    let mut replied = 0;
    loop {
        let next = if waiting.is_empty() {
            incoming.recv().await
        } else {
            match tokio::time::timeout(Duration::from_millis(100), incoming.recv()).await {
                Ok(next) => next,
                Err(_) => {
                    answer(&mut wr, &mut waiting, &content, &replies, &mut replied).await;
                    continue;
                }
            }
        };
        // 用戶端關了（測試結束）就停。沒想到的請求就 panic：連線跟著斷，用戶端回錯，測試不會卡住。
        let Some(body) = next else {
            return;
        };
        let mut req = Fields(&body);
        let reply = match req.u8() {
            READ => {
                let id = req.u32();
                req.string();
                let offset = req.u64();
                waiting.push((id, offset, req.u32() as usize));
                most_waiting.fetch_max(waiting.len(), Ordering::Relaxed);
                if waiting.len() >= replies.hold {
                    answer(&mut wr, &mut waiting, &content, &replies, &mut replied).await;
                }
                continue;
            }
            INIT => packet(VERSION, &[&3u32.to_be_bytes()]),
            OPEN => packet(HANDLE, &[&req.u32().to_be_bytes(), &string(b"h")]),
            CLOSE => status(req.u32(), FX_OK),
            other => panic!("fake SFTP server: unexpected request type {other}"),
        };
        answer(&mut wr, &mut waiting, &content, &replies, &mut replied).await;
        if wr.write_all(&reply).await.is_err() {
            return;
        }
    }
}

/// 照收到的順序回在等的 READ。`replied` 是到目前為止回了幾個（`short_every` 用）。
async fn answer(
    wr: &mut WriteHalf<tokio::io::DuplexStream>,
    waiting: &mut Waiting,
    content: &[u8],
    replies: &Replies,
    replied: &mut usize,
) {
    for (id, offset, want) in waiting.drain(..) {
        *replied += 1;
        let reply = match content.get(offset as usize..) {
            Some(rest) if !rest.is_empty() => {
                let mut n = want.min(replies.cap);
                if replies.short_every > 0 && replied.is_multiple_of(replies.short_every) {
                    n = n.div_ceil(2);
                }
                let n = n.min(rest.len());
                packet(DATA, &[&id.to_be_bytes(), &string(&rest[..n])])
            }
            _ => status(id, FX_EOF),
        };
        if wr.write_all(&reply).await.is_err() {
            return;
        }
    }
}

struct Fields<'a>(&'a [u8]);

impl<'a> Fields<'a> {
    fn take(&mut self, n: usize) -> &'a [u8] {
        let (head, rest) = self.0.split_at(n);
        self.0 = rest;
        head
    }

    fn u8(&mut self) -> u8 {
        self.take(1)[0]
    }

    fn u32(&mut self) -> u32 {
        u32::from_be_bytes(self.take(4).try_into().unwrap())
    }

    fn u64(&mut self) -> u64 {
        u64::from_be_bytes(self.take(8).try_into().unwrap())
    }

    fn string(&mut self) -> &'a [u8] {
        let n = self.u32() as usize;
        self.take(n)
    }
}

fn string(bytes: &[u8]) -> Vec<u8> {
    let mut out = (bytes.len() as u32).to_be_bytes().to_vec();
    out.extend_from_slice(bytes);
    out
}

fn packet(kind: u8, parts: &[&[u8]]) -> Vec<u8> {
    let len = 1 + parts.iter().map(|p| p.len()).sum::<usize>();
    let mut out = (len as u32).to_be_bytes().to_vec();
    out.push(kind);
    for part in parts {
        out.extend_from_slice(part);
    }
    out
}

fn status(id: u32, code: u32) -> Vec<u8> {
    packet(
        STATUS,
        &[
            &id.to_be_bytes(),
            &code.to_be_bytes(),
            &string(b""),
            &string(b""),
        ],
    )
}
