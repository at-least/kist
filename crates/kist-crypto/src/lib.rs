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
use kist_format::config::{ChunkerParams, KdfParams, KeySlot, WrappedKey, KDF_ARGON2ID};
use kist_format::envelope::{Compression, Envelope, ObjectKind, NONCE_LEN};
use kist_format::pack::{CHUNK_NONCE_LEN, TAG_LEN};
use kist_format::{ChunkId, FormatError, FORMAT_VERSION};
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

/// 包住 master key 時 AAD 的前綴；後面接 [`KeyBinding`] 的 bytes。
const MASTER_KEY_AAD_PREFIX: &[u8] = b"kist v1 master key\0";

/// KDF 參數上限：config 是明文，超過這些值的參數視為竄改，不真的去跑。
pub const MAX_KDF_M_COST_KIB: u32 = 1024 * 1024; // 1 GiB
pub const MAX_KDF_T_COST: u32 = 64;
pub const MAX_KDF_P_COST: u32 = 64;

const CTX_HASH_KEY: &str = "kist v1 hash key";
const CTX_CHUNK_KEY: &str = "kist v1 chunk key";
const CTX_OBJECT_KEY: &str = "kist v1 object key";
const CTX_NONCE_KEY: &str = "kist v1 nonce key";
const CTX_CACHE_ID: &str = "kist v1 cache id";

/// zstd 壓縮等級（物件用）。
const ZSTD_LEVEL: i32 = 3;

#[derive(Debug, thiserror::Error)]
pub enum CryptoError {
    #[error("wrong password (or the repository config was tampered with)")]
    WrongPassword,
    #[error("the operating system's random number generator failed: {0}")]
    Rng(String),
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

/// Argon2id 的成本參數。`Default` 是 64 MiB / 3 次 / 1 執行緒：高於 OWASP 的下限
/// （19 MiB / 2 次），每次開 repo 只算一次，多花零點幾秒換更貴的暴力破解成本。
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
            p_cost: 1,
        }
    }
}

/// 綁進 master key AAD 的 repo 參數：`config` 是明文，把這些綁進來之後，
/// 有人改了它們就會解不開 master key，而不是悄悄讓去重失效或讓 client 信任錯的快取。
/// 只綁「本來就不能改」的東西（改 chunker 參數等於放棄既有的去重），
/// `pack_target_size` 這種可調的效能參數不綁：綁了每次調整都得用所有 key slot 的密碼重包。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyBinding {
    pub repo_id: Vec<u8>,
    pub chunker: ChunkerParams,
}

impl KeyBinding {
    fn aad(&self) -> Vec<u8> {
        let mut aad = MASTER_KEY_AAD_PREFIX.to_vec();
        aad.extend_from_slice(&self.repo_id);
        aad.extend_from_slice(&self.chunker.min.to_le_bytes());
        aad.extend_from_slice(&self.chunker.avg.to_le_bytes());
        aad.extend_from_slice(&self.chunker.max.to_le_bytes());
        aad
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

fn kdf(password: &[u8], params: &KdfParams) -> Result<Zeroizing<[u8; 32]>> {
    if params.algorithm != KDF_ARGON2ID {
        return Err(CryptoError::UnsupportedKdf(params.algorithm.clone()));
    }
    if params.m_cost_kib > MAX_KDF_M_COST_KIB
        || params.t_cost > MAX_KDF_T_COST
        || params.p_cost > MAX_KDF_P_COST
        || params.salt.len() != 16
    {
        return Err(CryptoError::BadKdfParams(format!(
            "m={} KiB t={} p={} salt={} bytes exceeds the allowed limits (config tampered?)",
            params.m_cost_kib,
            params.t_cost,
            params.p_cost,
            params.salt.len()
        )));
    }
    let argon_params =
        argon2::Params::new(params.m_cost_kib, params.t_cost, params.p_cost, Some(32))
            .map_err(|e| CryptoError::BadKdfParams(e.to_string()))?;
    let argon = argon2::Argon2::new(
        argon2::Algorithm::Argon2id,
        argon2::Version::V0x13,
        argon_params,
    );
    let mut kek = Zeroizing::new([0u8; 32]);
    argon
        .hash_password_into(password, &params.salt, kek.as_mut())
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
    binding: &KeyBinding,
) -> Result<(KeySlot, MasterKey)> {
    let master = MasterKey::generate()?;
    let slot = wrap_master_key(&master, password, name, created, cost, binding)?;
    Ok((slot, master))
}

/// 用一組（新的）密碼把既有的 master key 包成 key slot（加密碼 / 換密碼用）。
pub fn wrap_master_key(
    master: &MasterKey,
    password: &[u8],
    name: &str,
    created: &str,
    cost: KdfCost,
    binding: &KeyBinding,
) -> Result<KeySlot> {
    let params = KdfParams {
        algorithm: KDF_ARGON2ID.to_owned(),
        m_cost_kib: cost.m_cost_kib,
        t_cost: cost.t_cost,
        p_cost: cost.p_cost,
        salt: random_bytes::<16>()?.to_vec(),
    };
    let kek = kdf(password, &params)?;
    let nonce = random_bytes::<NONCE_LEN>()?;
    let ciphertext = cipher(&kek)
        .encrypt(
            &XNonce::from(nonce),
            Payload {
                msg: master.as_bytes(),
                aad: &binding.aad(),
            },
        )
        .map_err(|_| CryptoError::AuthFailed)?;
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
pub fn unlock_key_slot(password: &[u8], slot: &KeySlot, binding: &KeyBinding) -> Result<MasterKey> {
    let kek = kdf(password, &slot.kdf)?;
    let nonce = nonce_from_slice(&slot.wrapped_master_key.nonce)?;
    let result = cipher(&kek).decrypt(
        &XNonce::from(nonce),
        Payload {
            msg: &slot.wrapped_master_key.ciphertext,
            aad: &binding.aad(),
        },
    );
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

    /// 本機快取（M2 的 index 快取等）用來識別「這是哪個 repo」的 ID。
    /// 從 master key 派生而不是用明文 config 裡的 `repo_id`：明文可以被換掉，
    /// 換掉後 client 會信任錯的快取、以為 chunk 已存在而不上傳。
    pub fn cache_id(&self) -> [u8; 16] {
        let full = blake3::derive_key(CTX_CACHE_ID, &self.hash_key);
        let mut id = [0u8; 16];
        id.copy_from_slice(&full[..16]);
        id
    }

    /// chunk ID = keyed BLAKE3(hash key, 明文)。
    pub fn chunk_id(&self, plaintext: &[u8]) -> ChunkId {
        ChunkId::from_bytes(*blake3::keyed_hash(&self.hash_key, plaintext).as_bytes())
    }

    /// 加密一個 chunk 的 payload（明文或已壓縮），回傳 pack entry bytes：nonce ‖ 密文 ‖ tag。
    pub fn seal_chunk(&self, id: &ChunkId, payload: &[u8]) -> Result<Vec<u8>> {
        let nonce = random_bytes::<CHUNK_NONCE_LEN>()?;
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
        // 決定性 nonce 必須從「真正被加密的 bytes」（壓縮後的 body）推導，而不是壓縮前的明文：
        // 否則 zstd 換版本時，同一個 nonce 會拿去加密不同的 body，等於 nonce 重用。
        let nonce: [u8; NONCE_LEN] = if kind == ObjectKind::Tree {
            let mut n = [0u8; NONCE_LEN];
            n.copy_from_slice(&blake3::keyed_hash(&self.nonce_key, &body).as_bytes()[..NONCE_LEN]);
            n
        } else {
            random_bytes()?
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
