//! v4 的金鑰階層與密封規則（`docs/format-v4.md` §3、§5）。與 v3 的函式並存，直到使用者遷移完。
//!
//! ```text
//! KEK      = Argon2id(password, config.kdf)
//! 槽的名稱  = DeriveKey("kist/v4/slot-name", KEK)[..16]        → keys/<32 位 hex>
//! 槽的金鑰  = DeriveKey("kist/v4/slot-key", KEK)
//! 槽        = { v: 4, wrapped: nonce(24) ‖ AEAD(槽的金鑰, master(32), AAD) ‖ tag(16) }
//!             AAD = "kist/v4/slot" ‖ BLAKE3(config 的 bytes)
//! master ─DeriveKey─▶ chunk-id、meta-id、cut、data、meta、snapshot、index、cache
//! ```
//!
//! 開啟 repo 只 GET 一個槽：槽的名稱由密碼推導，不掃描。config 的任何一個 byte 被換掉，槽的 AAD 就對不上
//! （錯誤是 [`CryptoError::WrongPassword`]：密碼錯誤或 config 被竄改）。

use kist_format::v4::config::{Kdf, Slot};
use kist_format::v4::ids::{ChunkId, ClientId, MetaId};
use kist_format::v4::keys::{Gen, Key, SnapTs};
use kist_format::v4::{AAD_DELTA, AAD_PACK_TRAILER, AAD_SHARD, AAD_SLOT};
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

use crate::{open, seal, CryptoError, MasterKey, Result};

pub const CTX_SLOT_NAME: &str = "kist/v4/slot-name";
pub const CTX_SLOT_KEY: &str = "kist/v4/slot-key";
pub const CTX_CHUNK_ID: &str = "kist/v4/chunk-id";
pub const CTX_META_ID: &str = "kist/v4/meta-id";
pub const CTX_CUT: &str = "kist/v4/cut";
pub const CTX_DATA: &str = "kist/v4/data";
pub const CTX_META: &str = "kist/v4/meta";
pub const CTX_SNAPSHOT: &str = "kist/v4/snapshot";
pub const CTX_INDEX: &str = "kist/v4/index";
pub const CTX_CACHE: &str = "kist/v4/cache";

/// 由密碼與 config 的 KDF 參數推導出的 KEK。離開作用域時清零。
pub struct Kek(Zeroizing<[u8; 32]>);

/// KEK ＝ Argon2id(password, config.kdf)。參數先照格式的上限驗證，荒謬的值不真的去跑。
pub fn derive_kek(password: &[u8], kdf: &Kdf) -> Result<Kek> {
    kdf.validate()
        .map_err(|e| CryptoError::BadKdfParams(e.to_string()))?;
    let params = argon2::Params::new(kdf.m, kdf.t, kdf.p, Some(32))
        .map_err(|e| CryptoError::BadKdfParams(e.to_string()))?;
    let argon = argon2::Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params);
    let mut kek = Zeroizing::new([0u8; 32]);
    argon
        .hash_password_into(password, kdf.salt.as_bytes(), kek.as_mut())
        .map_err(|e| CryptoError::BadKdfParams(e.to_string()))?;
    Ok(Kek(kek))
}

impl Kek {
    /// 槽的名稱（`keys/<hex>` 的 16 bytes）。
    pub fn slot_name(&self) -> [u8; 16] {
        let full = blake3::derive_key(CTX_SLOT_NAME, self.0.as_ref());
        let mut name = [0u8; 16];
        name.copy_from_slice(&full[..16]);
        name
    }

    fn slot_key(&self) -> Zeroizing<[u8; 32]> {
        Zeroizing::new(blake3::derive_key(CTX_SLOT_KEY, self.0.as_ref()))
    }
}

/// 槽的 AAD：`"kist/v4/slot" ‖ BLAKE3(config 的 bytes)`。`config_bytes` 是從 repo 讀到的原始 bytes。
pub fn slot_aad(config_bytes: &[u8]) -> Vec<u8> {
    let mut aad = AAD_SLOT.to_vec();
    aad.extend_from_slice(blake3::hash(config_bytes).as_bytes());
    aad
}

/// 以 KEK 包住 master，nonce 用 OS 亂數。
pub fn wrap_master(kek: &Kek, master: &MasterKey, config_bytes: &[u8]) -> Result<Slot> {
    let wrapped = seal(&kek.slot_key(), &slot_aad(config_bytes), master.as_bytes())?;
    Ok(Slot::new(wrapped))
}

/// 解開槽。密碼錯誤或 config 被換掉都是 [`CryptoError::WrongPassword`]。
pub fn unwrap_master(kek: &Kek, slot: &Slot, config_bytes: &[u8]) -> Result<MasterKey> {
    let plain = Zeroizing::new(
        open(&kek.slot_key(), &slot_aad(config_bytes), &slot.wrapped).map_err(|e| match e {
            CryptoError::AuthFailed => CryptoError::WrongPassword,
            other => other,
        })?,
    );
    let bytes: [u8; 32] = plain
        .as_slice()
        .try_into()
        .map_err(|_| CryptoError::BadLength {
            what: "master key",
            expected: 32,
            actual: plain.len(),
        })?;
    Ok(MasterKey::from_bytes(bytes))
}

/// master 推導出的八把子金鑰。離開作用域時清零。
#[derive(Clone, Zeroize, ZeroizeOnDrop)]
pub struct Keys {
    chunk_id: [u8; 32],
    meta_id: [u8; 32],
    cut: [u8; 32],
    data: [u8; 32],
    meta: [u8; 32],
    snapshot: [u8; 32],
    index: [u8; 32],
    cache: [u8; 32],
}

impl Keys {
    pub fn derive(master: &MasterKey) -> Self {
        let m = master.as_bytes();
        Self {
            chunk_id: blake3::derive_key(CTX_CHUNK_ID, m),
            meta_id: blake3::derive_key(CTX_META_ID, m),
            cut: blake3::derive_key(CTX_CUT, m),
            data: blake3::derive_key(CTX_DATA, m),
            meta: blake3::derive_key(CTX_META, m),
            snapshot: blake3::derive_key(CTX_SNAPSHOT, m),
            index: blake3::derive_key(CTX_INDEX, m),
            cache: blake3::derive_key(CTX_CACHE, m),
        }
    }

    /// chunk ID ＝ keyed BLAKE3(`K_chunk_id`, chunk 明文)。
    pub fn chunk_id(&self, plaintext: &[u8]) -> ChunkId {
        ChunkId::from_bytes(*blake3::keyed_hash(&self.chunk_id, plaintext).as_bytes())
    }

    /// meta ID ＝ keyed BLAKE3(`K_meta_id`, item 的規範 CBOR，壓縮之前)。
    pub fn meta_id(&self, canonical_cbor: &[u8]) -> MetaId {
        MetaId::from_bytes(*blake3::keyed_hash(&self.meta_id, canonical_cbor).as_bytes())
    }

    /// §7 的切段條件：keyed BLAKE3(`K_cut`, 名稱) 的前兩個 byte 以 little-endian 讀成整數，低 12 位元
    /// 全為 0。
    pub fn is_cut(&self, name: &[u8]) -> bool {
        let h = blake3::keyed_hash(&self.cut, name);
        let b = h.as_bytes();
        u16::from_le_bytes([b[0], b[1]]) & 0x0fff == 0
    }

    /// 本地 index 快取的目錄名稱：`K_cache` 前 16 bytes 的 hex。換 repo 位置就不共用（同 v3）。
    pub fn cache_dir_name(&self) -> String {
        self.cache[..16]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    /// chunk（`packs/` 內的一項）：`K_data`，AAD 是 chunk ID。`plain` 是 `alg byte ‖ data`。
    pub fn seal_chunk(&self, id: &ChunkId, plain: &[u8]) -> Result<Vec<u8>> {
        seal(&self.data, id.as_bytes(), plain)
    }

    pub fn open_chunk(&self, id: &ChunkId, sealed: &[u8]) -> Result<Vec<u8>> {
        open(&self.data, id.as_bytes(), sealed)
    }

    /// metadata item（`mpacks/` 內的一項）：`K_meta`，AAD 是 meta ID。`plain` 是 `alg byte ‖ CBOR`。
    pub fn seal_meta(&self, id: &MetaId, plain: &[u8]) -> Result<Vec<u8>> {
        seal(&self.meta, id.as_bytes(), plain)
    }

    pub fn open_meta(&self, id: &MetaId, sealed: &[u8]) -> Result<Vec<u8>> {
        open(&self.meta, id.as_bytes(), sealed)
    }

    /// pack 與 mpack 的 trailer：`K_index`，AAD 是 `"kist/v4/pack-trailer"`。
    pub fn seal_trailer(&self, plain: &[u8]) -> Result<Vec<u8>> {
        seal(&self.index, AAD_PACK_TRAILER, plain)
    }

    pub fn open_trailer(&self, sealed: &[u8]) -> Result<Vec<u8>> {
        open(&self.index, AAD_PACK_TRAILER, sealed)
    }

    /// snapshot：`K_snap`，AAD 是 key 路徑 `snapshots/<c>/<ts>`（不含 `.r1`）。
    pub fn seal_snapshot(&self, client: &ClientId, ts: &SnapTs, plain: &[u8]) -> Result<Vec<u8>> {
        seal(&self.snapshot, snapshot_aad(client, ts).as_bytes(), plain)
    }

    pub fn open_snapshot(&self, client: &ClientId, ts: &SnapTs, sealed: &[u8]) -> Result<Vec<u8>> {
        open(&self.snapshot, snapshot_aad(client, ts).as_bytes(), sealed)
    }

    /// delta：`K_index`，AAD 是 `"kist/v4/delta"` ‖ 所在前綴的 client id。
    pub fn seal_delta(&self, client: &ClientId, plain: &[u8]) -> Result<Vec<u8>> {
        seal(&self.index, &delta_aad(client), plain)
    }

    pub fn open_delta(&self, client: &ClientId, sealed: &[u8]) -> Result<Vec<u8>> {
        open(&self.index, &delta_aad(client), sealed)
    }

    /// head：`K_index`，AAD 是 key 路徑 `maint/gen/<g>`（不含 `.r1`）。
    pub fn seal_head(&self, g: Gen, plain: &[u8]) -> Result<Vec<u8>> {
        seal(&self.index, head_aad(g).as_bytes(), plain)
    }

    pub fn open_head(&self, g: Gen, sealed: &[u8]) -> Result<Vec<u8>> {
        open(&self.index, head_aad(g).as_bytes(), sealed)
    }

    /// map 分片：`K_index`，AAD 是 `"kist/v4/shard"`（分片號碼在明文裡）。
    pub fn seal_shard(&self, plain: &[u8]) -> Result<Vec<u8>> {
        seal(&self.index, AAD_SHARD, plain)
    }

    pub fn open_shard(&self, sealed: &[u8]) -> Result<Vec<u8>> {
        open(&self.index, AAD_SHARD, sealed)
    }
}

fn snapshot_aad(client: &ClientId, ts: &SnapTs) -> String {
    Key::Snapshot {
        client: *client,
        ts: ts.clone(),
        replica: false,
    }
    .path()
}

fn head_aad(g: Gen) -> String {
    Key::Head { g, replica: false }.path()
}

fn delta_aad(client: &ClientId) -> Vec<u8> {
    let mut aad = AAD_DELTA.to_vec();
    aad.extend_from_slice(client.as_bytes());
    aad
}
