//! kist 的 AEAD 封裝與 master key；金鑰階層（password → KEK → master → 子金鑰）在 [`v4`]。
//!
//! 所有密碼學原語都來自 RustCrypto（`argon2`、`chacha20poly1305`、`blake3`），
//! 這裡只做組合，不自己實作任何原語。金鑰型別離開作用域時會被清零（`zeroize`）。
//!
//! sealed 物件沒有 header：`nonce(24) ‖ 密文 ‖ tag(16)`（`docs/format-v4.md` §5），全部用 OS 亂數
//! nonce——沒有任何決定性 nonce。AAD 依角色由呼叫端給。
//!
//! v3 的金鑰階層（wrapped master 帶不變式、`RepoKeys`）已隨 v3 移除（ADR 026 實作第七段）。

#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used)]

pub mod v4;

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use zeroize::{Zeroize, ZeroizeOnDrop};

/// XChaCha20-Poly1305 的 nonce 長度（sealed 物件用）。
pub const NONCE_LEN: usize = 24;
/// Poly1305 tag 的長度。
pub const TAG_LEN: usize = 16;

#[derive(Debug, thiserror::Error)]
pub enum CryptoError {
    #[error("wrong password (or the repository config was tampered with)")]
    WrongPassword,
    #[error("the operating system's random number generator failed: {0}")]
    Rng(String),
    #[error("authentication failed: data is corrupt or was encrypted with a different key")]
    AuthFailed,
    #[error("invalid KDF parameters: {0}")]
    BadKdfParams(String),
    #[error("{what} has wrong length: {actual} bytes, expected {expected}")]
    BadLength {
        what: &'static str,
        expected: usize,
        actual: usize,
    },
}

pub type Result<T> = std::result::Result<T, CryptoError>;

/// Argon2id 的成本參數。`Default` = 64 MiB / t=3 / p=4（RFC 9106 第二組建議），
/// 即 `docs/format-v4.md` §3 的預設值（向量見 `tests/golden-v4/`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KdfCost {
    pub m_cost_kib: u32,
    pub t_cost: u32,
    pub p_cost: u32,
}

impl Default for KdfCost {
    fn default() -> Self {
        Self {
            m_cost_kib: 64 * 1024,
            t_cost: 3,
            p_cost: 4,
        }
    }
}

/// 32-byte master key。離開作用域時清零。
#[derive(Clone, Zeroize, ZeroizeOnDrop)]
pub struct MasterKey([u8; 32]);

impl MasterKey {
    pub fn generate() -> Result<Self> {
        Ok(Self(random_bytes()?))
    }

    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl std::fmt::Debug for MasterKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MasterKey(..)")
    }
}

/// 直接向作業系統要亂數（getrandom），不經過任何使用者空間的 PRNG。
pub fn random_bytes<const N: usize>() -> Result<[u8; N]> {
    let mut b = [0u8; N];
    getrandom::fill(&mut b).map_err(|e| CryptoError::Rng(e.to_string()))?;
    Ok(b)
}

fn cipher(key: &[u8; 32]) -> XChaCha20Poly1305 {
    XChaCha20Poly1305::new(key.into())
}

fn nonce_from_slice(bytes: &[u8]) -> Result<[u8; NONCE_LEN]> {
    bytes.try_into().map_err(|_| CryptoError::BadLength {
        what: "nonce",
        expected: NONCE_LEN,
        actual: bytes.len(),
    })
}

/// 用指定金鑰密封：`nonce(24) ‖ 密文 ‖ tag(16)`。
fn seal(key: &[u8; 32], aad: &[u8], plaintext: &[u8]) -> Result<Vec<u8>> {
    let nonce = random_bytes::<NONCE_LEN>()?;
    let ct = cipher(key)
        .encrypt(
            &XNonce::from(nonce),
            Payload {
                msg: plaintext,
                aad,
            },
        )
        .map_err(|_| CryptoError::AuthFailed)?;
    let mut out = Vec::with_capacity(NONCE_LEN + ct.len());
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ct);
    Ok(out)
}

/// 用指定金鑰解開 `seal` 的輸出。
fn open(key: &[u8; 32], aad: &[u8], bytes: &[u8]) -> Result<Vec<u8>> {
    if bytes.len() < NONCE_LEN + TAG_LEN {
        return Err(CryptoError::BadLength {
            what: "sealed object",
            expected: NONCE_LEN + TAG_LEN,
            actual: bytes.len(),
        });
    }
    let nonce = nonce_from_slice(&bytes[..NONCE_LEN])?;
    cipher(key)
        .decrypt(
            &XNonce::from(nonce),
            Payload {
                msg: &bytes[NONCE_LEN..],
                aad,
            },
        )
        .map_err(|_| CryptoError::AuthFailed)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn seal_and_open_round_trip_and_reject_tampering() {
        let key = [7u8; 32];
        let sealed = seal(&key, b"aad", b"hello").unwrap();
        assert_eq!(sealed.len(), NONCE_LEN + 5 + TAG_LEN);
        assert_eq!(open(&key, b"aad", &sealed).unwrap(), b"hello");
        assert!(matches!(
            open(&key, b"other", &sealed),
            Err(CryptoError::AuthFailed)
        ));
        let mut bad = sealed.clone();
        bad[NONCE_LEN + 1] ^= 1;
        assert!(matches!(
            open(&key, b"aad", &bad),
            Err(CryptoError::AuthFailed)
        ));
        assert!(matches!(
            open(&key, b"aad", &sealed[..NONCE_LEN + TAG_LEN - 1]),
            Err(CryptoError::BadLength { .. })
        ));
    }
}
