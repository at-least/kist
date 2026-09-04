//! kist 的內容定義切塊（FastCDC 2020）封裝。
//!
//! 只做一件事：把一個 `Read` 串流切成 chunk，逐塊交出，不把整個檔案讀進記憶體。
//! 參數來自 repo 的 `config`（預設 512 KiB / 2 MiB / 8 MiB），不是寫死的：
//! 同一個 repo 的所有 client 必須用同樣的參數，去重才會一致。

#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used)]

use std::io::Read;

use fastcdc::v2020::StreamCDC;
use kist_format::config::ChunkerParams;

#[derive(Debug, thiserror::Error)]
pub enum ChunkerError {
    #[error("read error while chunking: {0}")]
    Io(std::io::Error),
    #[error("chunker error: {0}")]
    Other(String),
}

impl From<fastcdc::v2020::Error> for ChunkerError {
    fn from(e: fastcdc::v2020::Error) -> Self {
        match e {
            fastcdc::v2020::Error::IoError(io) => Self::Io(io),
            other => Self::Other(other.to_string()),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Chunker {
    params: ChunkerParams,
}

impl Chunker {
    pub fn new(params: ChunkerParams) -> Self {
        Self { params }
    }

    pub fn params(&self) -> ChunkerParams {
        self.params
    }

    /// 以串流方式切塊。每個項目是一個 chunk 的明文；讀取錯誤會以 `Err` 交出並終止。
    pub fn chunks<R: Read>(&self, reader: R) -> Chunks<R> {
        Chunks {
            inner: StreamCDC::new(
                reader,
                usize_of(self.params.min),
                usize_of(self.params.avg),
                usize_of(self.params.max),
            ),
        }
    }
}

fn usize_of(v: u32) -> usize {
    usize::try_from(v).unwrap_or(usize::MAX)
}

pub struct Chunks<R: Read> {
    inner: StreamCDC<R>,
}

impl<R: Read> Iterator for Chunks<R> {
    type Item = Result<Vec<u8>, ChunkerError>;

    fn next(&mut self) -> Option<Self::Item> {
        match self.inner.next()? {
            Ok(chunk) => Some(Ok(chunk.data)),
            Err(e) => Some(Err(e.into())),
        }
    }
}
