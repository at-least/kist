//! 獨立物件的外層封裝（envelope）。
//!
//! tree / index / snapshot / key slot / pack trailer 都是「一段 CBOR 明文，可選 zstd 壓縮，
//! 再整段 AEAD 加密」。這個模組只負責排版，不碰金鑰：
//!
//! ```text
//! offset  size  欄位
//! 0       4     magic "KIST"
//! 4       1     envelope 版本（目前 1）
//! 5       1     物件種類（ObjectKind）
//! 6       1     壓縮方式（Compression）
//! 7       1     保留，必須是 0
//! 8       24    XChaCha20-Poly1305 nonce
//! 32      ...   密文（含 16-byte tag）
//! ```
//!
//! 前 32 bytes 就是 AEAD 的 AAD：物件種類綁進驗證，伺服器端把 index 換成 tree 會被抓到。

use crate::{FormatError, Result};

pub const MAGIC: &[u8; 4] = b"KIST";
pub const VERSION: u8 = 1;
pub const NONCE_LEN: usize = 24;
pub const HEADER_LEN: usize = 32;

/// 物件種類。數值寫進 header 與 AAD，不可重新編號。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ObjectKind {
    Tree = 1,
    Index = 2,
    Snapshot = 3,
    KeySlot = 4,
    PackTrailer = 5,
}

impl ObjectKind {
    pub fn from_u8(v: u8) -> Result<Self> {
        Ok(match v {
            1 => Self::Tree,
            2 => Self::Index,
            3 => Self::Snapshot,
            4 => Self::KeySlot,
            5 => Self::PackTrailer,
            other => return Err(FormatError::UnknownKind(other)),
        })
    }
}

/// 明文在加密前的壓縮方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Compression {
    None = 0,
    Zstd = 1,
}

impl Compression {
    pub fn from_u8(v: u8) -> Result<Self> {
        Ok(match v {
            0 => Self::None,
            1 => Self::Zstd,
            other => return Err(FormatError::UnknownCompression(other)),
        })
    }
}

/// 一個已封裝（已加密）的物件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Envelope {
    pub kind: ObjectKind,
    pub compression: Compression,
    pub nonce: [u8; NONCE_LEN],
    pub ciphertext: Vec<u8>,
}

impl Envelope {
    /// 32-byte header，同時就是 AEAD 的 AAD。
    pub fn header(
        kind: ObjectKind,
        compression: Compression,
        nonce: &[u8; NONCE_LEN],
    ) -> [u8; HEADER_LEN] {
        let mut h = [0u8; HEADER_LEN];
        h[0..4].copy_from_slice(MAGIC);
        h[4] = VERSION;
        h[5] = kind as u8;
        h[6] = compression as u8;
        h[7] = 0;
        h[8..].copy_from_slice(nonce);
        h
    }

    pub fn header_bytes(&self) -> [u8; HEADER_LEN] {
        Self::header(self.kind, self.compression, &self.nonce)
    }

    /// 排成要寫進 repo 的完整 bytes。
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(HEADER_LEN + self.ciphertext.len());
        out.extend_from_slice(&self.header_bytes());
        out.extend_from_slice(&self.ciphertext);
        out
    }

    /// 從 repo 讀到的 bytes 拆回 header 與密文。不驗證密文（那是解密的事）。
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < HEADER_LEN {
            return Err(FormatError::Truncated {
                what: "object envelope",
                needed: HEADER_LEN,
                actual: bytes.len(),
            });
        }
        if &bytes[0..4] != MAGIC {
            return Err(FormatError::BadMagic {
                what: "object envelope",
            });
        }
        if bytes[4] != VERSION {
            return Err(FormatError::UnsupportedVersion {
                what: "object envelope",
                version: u32::from(bytes[4]),
            });
        }
        let kind = ObjectKind::from_u8(bytes[5])?;
        let compression = Compression::from_u8(bytes[6])?;
        if bytes[7] != 0 {
            return Err(FormatError::UnsupportedVersion {
                what: "object envelope (reserved byte)",
                version: u32::from(bytes[7]),
            });
        }
        let mut nonce = [0u8; NONCE_LEN];
        nonce.copy_from_slice(&bytes[8..HEADER_LEN]);
        Ok(Self {
            kind,
            compression,
            nonce,
            ciphertext: bytes[HEADER_LEN..].to_vec(),
        })
    }
}
