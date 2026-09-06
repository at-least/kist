//! kist 的金鑰階層（password → KEK → master key → 派生子金鑰）與 AEAD 封裝（v2）。
//!
//! 所有密碼學原語都來自 RustCrypto（`argon2`、`chacha20poly1305`、`blake3`），
//! 這裡只做組合，不自己實作任何原語。金鑰型別離開作用域時會被清零（`zeroize`）。
//!
//! v2 重點（`docs/format.md` §3、§5）：
//! - 子金鑰 = BLAKE3 DeriveKey，context 為 `kist/v2/{hash,chunk,meta,index}`；
//!   **沒有 nonce key**——v2 沒有任何決定性 nonce，全部用 OS 亂數。
//! - sealed 物件沒有 header：`nonce(24) ‖ 密文 ‖ tag(16)`，AAD 依角色
//!   （tree = 自己的 ID、snapshot = 完整 key、trailer/index = 角色常數）。
//! - master key 封裝的 AAD 綁 repo_id 與 chunker 參數（[`kist_format::master_aad`]）。
//!
//! 金鑰推導的跨語言測試向量見 `tests/poc_keys.rs`（與 Go 實作逐 byte 相同）。

#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used)]

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use kist_format::config::{ChunkerParams, KdfParams, KeySlot, KDF_ARGON2ID};
use kist_format::pack::{CHUNK_NONCE_LEN, TAG_LEN};
use kist_format::{ChunkId, TreeId};
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

/// XChaCha20-Poly1305 的 nonce 長度（sealed 物件用）。
pub const NONCE_LEN: usize = 24;

/// KDF 參數上限：config 是明文，超過這些值的參數視為竄改，不真的去跑。
pub const MAX_KDF_M_COST_KIB: u32 = 1024 * 1024; // 1 GiB
pub const MAX_KDF_T_COST: u32 = 64;
pub const MAX_KDF_P_COST: u32 = 64;

const CTX_HASH_KEY: &str = "kist/v2/hash";
const CTX_CHUNK_KEY: &str = "kist/v2/chunk";
const CTX_META_KEY: &str = "kist/v2/meta";
const CTX_INDEX_KEY: &str = "kist/v2/index";
const CTX_CACHE_ID: &str = "kist/v2/cache";

#[derive(Debug, thiserror::Error)]
pub enum CryptoError {
    #[error("wrong password (or the repository config was tampered with)")]
    WrongPassword,
    #[error("the operating system's random number generator failed: {0}")]
    Rng(String),
    #[error("authentication failed: data is corrupt or was encrypted with a different key")]
    AuthFailed,
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
}

pub type Result<T> = std::result::Result<T, CryptoError>;

/// Argon2id 的成本參數。`Default` = 64 MiB / t=3 / p=4（RFC 9106 第二組建議），
/// 與 Go 實作一致（跨語言向量見 tests/poc_keys.rs）。
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

/// 綁進 master key AAD 的 repo 參數（見 [`kist_format::master_aad`]）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyBinding {
    pub repo_id: Vec<u8>,
    pub chunker: ChunkerParams,
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
    created_ns: i64,
    cost: KdfCost,
    binding: &KeyBinding,
) -> Result<(KeySlot, MasterKey)> {
    let master = MasterKey::generate()?;
    let slot = wrap_master_key(&master, password, name, created_ns, cost, binding)?;
    Ok((slot, master))
}

/// 用一組（新的）密碼把既有的 master key 包成 key slot（加密碼 / 換密碼用）。
pub fn wrap_master_key(
    master: &MasterKey,
    password: &[u8],
    name: &str,
    created_ns: i64,
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
    let sealed = seal_meta(&kek, &binding.aad(), master.as_bytes())?;
    Ok(KeySlot {
        version: kist_format::FORMAT_VERSION,
        name: name.to_owned(),
        created_ns,
        kdf: params,
        wrapped: sealed,
    })
}

impl KeyBinding {
    fn aad(&self) -> Vec<u8> {
        kist_format::master_aad(&self.repo_id, &self.chunker)
    }
}

/// 用密碼解開 key slot 裡的 master key。
pub fn unlock_key_slot(password: &[u8], slot: &KeySlot, binding: &KeyBinding) -> Result<MasterKey> {
    let kek = kdf(password, &slot.kdf)?;
    let plain = open_meta(&kek, &binding.aad(), &slot.wrapped).map_err(|_| CryptoError::WrongPassword)?;
    let key: [u8; 32] = plain
        .as_slice()
        .try_into()
        .map_err(|_| CryptoError::BadLength {
            what: "master key",
            expected: 32,
            actual: plain.len(),
        })?;
    Ok(MasterKey(key))
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

/// KEK 層的密封（master key 封裝專用；不需要 RepoKeys）。
fn seal_meta(kek: &[u8; 32], aad: &[u8], plaintext: &[u8]) -> Result<Vec<u8>> {
    seal(kek, aad, plaintext)
}

fn open_meta(kek: &[u8; 32], aad: &[u8], bytes: &[u8]) -> Result<Vec<u8>> {
    open(kek, aad, bytes)
}

/// 由 master key 派生的四把子金鑰（hash / chunk / meta / index）。
#[derive(Clone, Zeroize, ZeroizeOnDrop)]
pub struct RepoKeys {
    hash_key: [u8; 32],
    chunk_key: [u8; 32],
    meta_key: [u8; 32],
    index_key: [u8; 32],
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
            meta_key: blake3::derive_key(CTX_META_KEY, m),
            index_key: blake3::derive_key(CTX_INDEX_KEY, m),
        }
    }

    /// 本機快取用來識別「這是哪個 repo」的 ID。從 master key 派生而不是用
    /// 明文 config 裡的 `repo_id`：明文可以被換掉，換掉後 client 會信任錯的
    /// 快取、以為 chunk 已存在而不上傳。
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

    /// tree ID = keyed BLAKE3(hash key, tree 明文 CBOR)——與 chunk ID 同函式。
    pub fn tree_id(&self, tree_plaintext: &[u8]) -> TreeId {
        TreeId::from_bytes(*blake3::keyed_hash(&self.hash_key, tree_plaintext).as_bytes())
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

    /// 解開 pack entry，回傳 payload（algorithm byte 在裡面，呼叫端處理）。
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

    /// 密封 tree：meta key，AAD = tree 自己的 ID。
    pub fn seal_tree(&self, id: &TreeId, plaintext: &[u8]) -> Result<Vec<u8>> {
        seal(&self.meta_key, id.as_bytes(), plaintext)
    }

    /// 解開 tree（AAD = ID；呼叫端解開後重算 hash 對名稱）。
    pub fn open_tree(&self, id: &TreeId, bytes: &[u8]) -> Result<Vec<u8>> {
        open(&self.meta_key, id.as_bytes(), bytes)
    }

    /// 密封 snapshot：meta key，AAD = 完整 key 路徑。
    pub fn seal_snapshot(&self, key_path: &str, plaintext: &[u8]) -> Result<Vec<u8>> {
        seal(&self.meta_key, key_path.as_bytes(), plaintext)
    }

    /// 解開 snapshot（AAD = key 路徑；路徑不對就解不開）。
    pub fn open_snapshot(&self, key_path: &str, bytes: &[u8]) -> Result<Vec<u8>> {
        open(&self.meta_key, key_path.as_bytes(), bytes)
    }

    /// 密封 pack trailer：index key，AAD = 角色常數。
    pub fn seal_pack_trailer(&self, plaintext: &[u8]) -> Result<Vec<u8>> {
        seal(&self.index_key, kist_format::AAD_PACK_TRAILER, plaintext)
    }

    /// 解開 pack trailer。
    pub fn open_pack_trailer(&self, bytes: &[u8]) -> Result<Vec<u8>> {
        open(&self.index_key, kist_format::AAD_PACK_TRAILER, bytes)
    }

    /// 密封 index blob：index key，AAD = 角色常數。
    /// 密封 index blob：**吃掉**明文 buffer、就地加密（tag 附加在後），
    /// 回傳 `nonce ‖ ct ‖ tag`（與 [`Self::open_index_blob`] 對應）。
    /// index blob 是 repo 裡最大的 meta（100 萬 chunk 的明文 ≈ 56 MiB），
    /// 就地加密省掉 `encrypt` 的整份密文拷貝。呼叫端先 `reserve(TAG_LEN)`
    /// 就不會在附加 tag 時觸發重配。
    pub fn seal_index_blob_in_place(&self, mut buf: Vec<u8>) -> Result<Vec<u8>> {
        use chacha20poly1305::aead::AeadInPlace;
        let nonce = random_bytes::<NONCE_LEN>()?;
        cipher(&self.index_key)
            .encrypt_in_place(&XNonce::from(nonce), kist_format::AAD_INDEX, &mut buf)
            .map_err(|_| CryptoError::AuthFailed)?;
        let mut out = Vec::with_capacity(NONCE_LEN + buf.len());
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&buf);
        Ok(out)
    }

    /// 解開 index blob。
    pub fn open_index_blob(&self, bytes: &[u8]) -> Result<Vec<u8>> {
        open(&self.index_key, kist_format::AAD_INDEX, bytes)
    }
}
