//! kist 的金鑰階層（password → KEK → master key → 派生子金鑰）與 AEAD 封裝。
//!
//! 所有密碼學原語都來自 RustCrypto（`argon2`、`chacha20poly1305`、`blake3`），
//! 這裡只做組合，不自己實作任何原語。金鑰型別離開作用域時會被清零（`zeroize`）。
//!
//! 主要 API：
//! - [`create_key_slot`] / [`wrap_master_key`] / [`unlock_key_slot`]：密碼 ↔ master key。
//! - [`RepoKeys`]：由 master key 派生出的四把子金鑰，提供 chunk / 物件的 seal 與 open。
//!
//! 格式細節（AAD、nonce 規則）見 `docs/format.md` §3、§5、§6。

#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used)]

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use kist_format::config::{KdfParams, KeySlot, WrappedKey, KDF_ARGON2ID};
use kist_format::envelope::{Compression, Envelope, ObjectKind, NONCE_LEN};
use kist_format::pack::{CHUNK_NONCE_LEN, TAG_LEN};
use kist_format::{ChunkId, FormatError, FORMAT_VERSION};
use zeroize::{Zeroize, ZeroizeOnDrop};

/// 包住 master key 時的 AAD。
const MASTER_KEY_AAD: &[u8] = b"kist v1 master key";

const CTX_HASH_KEY: &str = "kist v1 hash key";
const CTX_CHUNK_KEY: &str = "kist v1 chunk key";
const CTX_OBJECT_KEY: &str = "kist v1 object key";
const CTX_NONCE_KEY: &str = "kist v1 nonce key";

/// zstd 壓縮等級（物件用）。
const ZSTD_LEVEL: i32 = 3;

#[derive(Debug, thiserror::Error)]
pub enum CryptoError {
    #[error("wrong password")]
    WrongPassword,
    #[error("authentication failed: data is corrupt or was encrypted with a different key")]
    AuthFailed,
    #[error("expected a {expected:?} object but found {actual:?}")]
    KindMismatch {
        expected: ObjectKind,
        actual: ObjectKind,
    },
    #[error("unsupported KDF {0:?}")]
    UnsupportedKdf(String),
    #[error("invalid KDF parameters: {0}")]
    BadKdfParams(String),
    #[error("{what} has wrong length: {actual} bytes, expected {expected}")]
    BadLength {
        what: &'static str,
        expected: usize,
        actual: usize,
    },
    #[error("compression failed: {0}")]
    Compression(String),
    #[error(transparent)]
    Format(#[from] FormatError),
}

pub type Result<T> = std::result::Result<T, CryptoError>;

/// Argon2id 的成本參數。`Default` 是 RustCrypto 的建議值（19 MiB / 2 次 / 1 執行緒）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KdfCost {
    pub m_cost_kib: u32,
    pub t_cost: u32,
    pub p_cost: u32,
}

impl Default for KdfCost {
    fn default() -> Self {
        Self {
            m_cost_kib: argon2::Params::DEFAULT_M_COST,
            t_cost: argon2::Params::DEFAULT_T_COST,
            p_cost: argon2::Params::DEFAULT_P_COST,
        }
    }
}

/// 32-byte master key。離開作用域時清零。
#[derive(Clone, Zeroize, ZeroizeOnDrop)]
pub struct MasterKey([u8; 32]);

impl MasterKey {
    pub fn generate() -> Self {
        let mut k = [0u8; 32];
        rand::fill(&mut k);
        Self(k)
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

/// 產生 OS 亂數。
pub fn random_bytes<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    rand::fill(&mut b);
    b
}

fn kdf(password: &[u8], params: &KdfParams) -> Result<[u8; 32]> {
    if params.algorithm != KDF_ARGON2ID {
        return Err(CryptoError::UnsupportedKdf(params.algorithm.clone()));
    }
    let argon_params =
        argon2::Params::new(params.m_cost_kib, params.t_cost, params.p_cost, Some(32))
            .map_err(|e| CryptoError::BadKdfParams(e.to_string()))?;
    let argon = argon2::Argon2::new(
        argon2::Algorithm::Argon2id,
        argon2::Version::V0x13,
        argon_params,
    );
    let mut kek = [0u8; 32];
    argon
        .hash_password_into(password, &params.salt, &mut kek)
        .map_err(|e| CryptoError::BadKdfParams(e.to_string()))?;
    Ok(kek)
}

fn cipher(key: &[u8; 32]) -> XChaCha20Poly1305 {
    XChaCha20Poly1305::new(key.into())
}

/// 產生新的 master key，並用密碼包成一個 key slot（`init` 用）。
pub fn create_key_slot(
    password: &[u8],
    name: &str,
    created: &str,
    cost: KdfCost,
) -> Result<(KeySlot, MasterKey)> {
    let master = MasterKey::generate();
    let slot = wrap_master_key(&master, password, name, created, cost)?;
    Ok((slot, master))
}

/// 用一組（新的）密碼把既有的 master key 包成 key slot（加密碼 / 換密碼用）。
pub fn wrap_master_key(
    master: &MasterKey,
    password: &[u8],
    name: &str,
    created: &str,
    cost: KdfCost,
) -> Result<KeySlot> {
    let params = KdfParams {
        algorithm: KDF_ARGON2ID.to_owned(),
        m_cost_kib: cost.m_cost_kib,
        t_cost: cost.t_cost,
        p_cost: cost.p_cost,
        salt: random_bytes::<16>().to_vec(),
    };
    let mut kek = kdf(password, &params)?;
    let nonce = random_bytes::<NONCE_LEN>();
    let ciphertext = cipher(&kek)
        .encrypt(
            &XNonce::from(nonce),
            Payload {
                msg: master.as_bytes(),
                aad: MASTER_KEY_AAD,
            },
        )
        .map_err(|_| CryptoError::AuthFailed)?;
    kek.zeroize();
    Ok(KeySlot {
        version: FORMAT_VERSION,
        name: name.to_owned(),
        created: created.to_owned(),
        kdf: params,
        wrapped_master_key: WrappedKey {
            nonce: nonce.to_vec(),
            ciphertext,
        },
    })
}

/// 用密碼解開 key slot 裡的 master key。
pub fn unlock_key_slot(password: &[u8], slot: &KeySlot) -> Result<MasterKey> {
    let mut kek = kdf(password, &slot.kdf)?;
    let nonce = nonce_from_slice(&slot.wrapped_master_key.nonce)?;
    let result = cipher(&kek).decrypt(
        &XNonce::from(nonce),
        Payload {
            msg: &slot.wrapped_master_key.ciphertext,
            aad: MASTER_KEY_AAD,
        },
    );
    kek.zeroize();
    let mut plain = result.map_err(|_| CryptoError::WrongPassword)?;
    let key: [u8; 32] = plain
        .as_slice()
        .try_into()
        .map_err(|_| CryptoError::BadLength {
            what: "master key",
            expected: 32,
            actual: plain.len(),
        })?;
    plain.zeroize();
    Ok(MasterKey(key))
}

fn nonce_from_slice(bytes: &[u8]) -> Result<[u8; NONCE_LEN]> {
    bytes.try_into().map_err(|_| CryptoError::BadLength {
        what: "nonce",
        expected: NONCE_LEN,
        actual: bytes.len(),
    })
}

/// 由 master key 派生的四把子金鑰。
#[derive(Clone, Zeroize, ZeroizeOnDrop)]
pub struct RepoKeys {
    hash_key: [u8; 32],
    chunk_key: [u8; 32],
    object_key: [u8; 32],
    nonce_key: [u8; 32],
}

impl std::fmt::Debug for RepoKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RepoKeys(..)")
    }
}

impl RepoKeys {
    pub fn from_master(master: &MasterKey) -> Self {
        let m = master.as_bytes();
        Self {
            hash_key: blake3::derive_key(CTX_HASH_KEY, m),
            chunk_key: blake3::derive_key(CTX_CHUNK_KEY, m),
            object_key: blake3::derive_key(CTX_OBJECT_KEY, m),
            nonce_key: blake3::derive_key(CTX_NONCE_KEY, m),
        }
    }

    /// chunk ID = keyed BLAKE3(hash key, 明文)。
    pub fn chunk_id(&self, plaintext: &[u8]) -> ChunkId {
        ChunkId::from_bytes(*blake3::keyed_hash(&self.hash_key, plaintext).as_bytes())
    }

    /// 加密一個 chunk 的 payload（明文或已壓縮），回傳 pack entry bytes：nonce ‖ 密文 ‖ tag。
    pub fn seal_chunk(&self, id: &ChunkId, payload: &[u8]) -> Result<Vec<u8>> {
        let nonce = random_bytes::<CHUNK_NONCE_LEN>();
        let ct = cipher(&self.chunk_key)
            .encrypt(
                &XNonce::from(nonce),
                Payload {
                    msg: payload,
                    aad: id.as_bytes(),
                },
            )
            .map_err(|_| CryptoError::AuthFailed)?;
        let mut out = Vec::with_capacity(CHUNK_NONCE_LEN + ct.len());
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&ct);
        Ok(out)
    }

    /// 解開 pack entry，回傳 payload（呼叫端再依 flags 決定是否解壓）。
    pub fn open_chunk(&self, id: &ChunkId, entry: &[u8]) -> Result<Vec<u8>> {
        if entry.len() < CHUNK_NONCE_LEN + TAG_LEN {
            return Err(CryptoError::BadLength {
                what: "chunk entry",
                expected: CHUNK_NONCE_LEN + TAG_LEN,
                actual: entry.len(),
            });
        }
        let nonce = nonce_from_slice(&entry[..CHUNK_NONCE_LEN])?;
        cipher(&self.chunk_key)
            .decrypt(
                &XNonce::from(nonce),
                Payload {
                    msg: &entry[CHUNK_NONCE_LEN..],
                    aad: id.as_bytes(),
                },
            )
            .map_err(|_| CryptoError::AuthFailed)
    }

    /// 把一個物件的明文封裝成要寫進 repo 的完整 bytes（envelope）。
    ///
    /// tree 用決定性 nonce（同明文 → 同密文，子樹才能重用）；其他物件用隨機 nonce。
    pub fn seal_object(
        &self,
        kind: ObjectKind,
        compression: Compression,
        plaintext: &[u8],
    ) -> Result<Vec<u8>> {
        let body = match compression {
            Compression::None => plaintext.to_vec(),
            Compression::Zstd => zstd::encode_all(plaintext, ZSTD_LEVEL)
                .map_err(|e| CryptoError::Compression(e.to_string()))?,
        };
        let nonce: [u8; NONCE_LEN] = if kind == ObjectKind::Tree {
            let mut n = [0u8; NONCE_LEN];
            n.copy_from_slice(
                &blake3::keyed_hash(&self.nonce_key, plaintext).as_bytes()[..NONCE_LEN],
            );
            n
        } else {
            random_bytes()
        };
        let header = Envelope::header(kind, compression, &nonce);
        let ciphertext = cipher(&self.object_key)
            .encrypt(
                &XNonce::from(nonce),
                Payload {
                    msg: &body,
                    aad: &header,
                },
            )
            .map_err(|_| CryptoError::AuthFailed)?;
        Ok(Envelope {
            kind,
            compression,
            nonce,
            ciphertext,
        }
        .encode())
    }

    /// 解開 envelope，驗證種類，回傳（解壓後的）明文。
    pub fn open_object(&self, expected: ObjectKind, bytes: &[u8]) -> Result<Vec<u8>> {
        let env = Envelope::parse(bytes)?;
        if env.kind != expected {
            return Err(CryptoError::KindMismatch {
                expected,
                actual: env.kind,
            });
        }
        let header = env.header_bytes();
        let body = cipher(&self.object_key)
            .decrypt(
                &XNonce::from(env.nonce),
                Payload {
                    msg: &env.ciphertext,
                    aad: &header,
                },
            )
            .map_err(|_| CryptoError::AuthFailed)?;
        match env.compression {
            Compression::None => Ok(body),
            Compression::Zstd => zstd::decode_all(body.as_slice())
                .map_err(|e| CryptoError::Compression(e.to_string())),
        }
    }
}
