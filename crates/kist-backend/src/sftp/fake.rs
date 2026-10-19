//! 測試用的 SFTP v3 伺服器，只有讀檔用到的請求（INIT、OPEN、READ、CLOSE）：經 `tokio::io::duplex` 接上
//! `Sftp::new`，不用網路與容器。用來造出測試容器（OpenSSH）不會給的回應，例如 READ 少送。

use tokio::io::{AsyncReadExt, AsyncWriteExt};

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

/// 起一個只有一個檔案的假伺服器（任何 OPEN 都開它），每次 READ 最多送 `cap` bytes（不管要了多少），回連到它的
/// `Sftp`。伺服器不宣告任何擴充，用戶端因此一次要 20 480 bytes（OpenSSH 的預設）。
pub(super) async fn connect(content: Vec<u8>, cap: usize) -> openssh_sftp_client::Sftp {
    let (client, server) = tokio::io::duplex(1 << 20);
    tokio::spawn(serve(server, content, cap));
    let (read, write) = tokio::io::split(client);
    openssh_sftp_client::Sftp::new(write, read, Default::default())
        .await
        .unwrap()
}

async fn serve(io: tokio::io::DuplexStream, content: Vec<u8>, cap: usize) {
    let (mut rx, mut tx) = tokio::io::split(io);
    // 用戶端關了（測試結束）就停。沒想到的請求就 panic：連線跟著斷，用戶端回錯，測試不會卡住。
    while let Ok(len) = rx.read_u32().await {
        let mut body = vec![0u8; len as usize];
        rx.read_exact(&mut body).await.unwrap();
        let mut req = Fields(&body);
        let reply = match req.u8() {
            INIT => packet(VERSION, &[&3u32.to_be_bytes()]),
            OPEN => packet(HANDLE, &[&req.u32().to_be_bytes(), &string(b"h")]),
            READ => {
                let id = req.u32();
                req.string();
                let offset = req.u64();
                let want = req.u32() as usize;
                match content.get(offset as usize..) {
                    Some(rest) if !rest.is_empty() => {
                        let n = want.min(cap).min(rest.len());
                        packet(DATA, &[&id.to_be_bytes(), &string(&rest[..n])])
                    }
                    _ => status(id, FX_EOF),
                }
            }
            CLOSE => status(req.u32(), FX_OK),
            other => panic!("fake SFTP server: unexpected request type {other}"),
        };
        if tx.write_all(&reply).await.is_err() {
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
