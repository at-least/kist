//! v4 的 init 與 open（§3、§10）。
//!
//! init 依序寫 `config`（明文、之後不變）、金鑰槽 `keys/<名稱>`（名稱由密碼推導）、head 0 的主體與 `.r1`
//! （空的 map、預設 settings）。open 讀 `config`（`v` 不是 4 就拒絕）、以密碼推導出槽的名稱、只 GET 那一個
//! 槽、解開 master、推導八把子金鑰。

use std::path::Path;
use std::sync::Arc;

use kist_backend::{Backend, BackendError, RepoLocation};
use kist_crypto::v4::{derive_kek, unwrap_master, wrap_master, Keys};
use kist_crypto::{CryptoError, KdfCost, MasterKey};
use kist_format::v4::config::{Chunker, Config, Kdf, KdfSalt, Slot, KDF_ARGON2ID};
use kist_format::v4::ids::parse_lower_hex;
use kist_format::v4::ids::RepoId;
use kist_format::v4::index::{Head, Settings};
use kist_format::v4::keys::{Key, KEYS};
use zeroize::Zeroizing;

use super::cache::Cache;
use crate::{blocking, CoreError, Result};

#[derive(Debug, Clone, Default)]
pub struct InitOptions {
    pub chunker: Chunker,
    pub kdf: KdfCost,
    /// head 0 的 settings；`None` 是預設（grace 72 小時、pack 64 MiB、mpack 8 MiB，replicas 本機 1、其他 0）。
    pub settings: Option<Settings>,
}

/// 一個開著的 v4 repo。
#[derive(Clone)]
pub struct Repo {
    backend: Backend,
    config: Config,
    keys: Arc<Keys>,
    cache: Option<Cache>,
    /// `key add` 用：master 本身、config 原樣的 bytes（槽的 AAD 綁它）、這個程序開啟時用的槽（`key remove` 不刪它）。
    master: Arc<MasterKey>,
    config_bytes: Arc<Vec<u8>>,
    slot: [u8; 16],
}

impl Repo {
    /// 同一個 repo、不用本地快取：check 要驗證的是後端的物件，不是快取裡的那一份。
    pub fn uncached(&self) -> Self {
        Self {
            cache: None,
            ..self.clone()
        }
    }
}

impl std::fmt::Debug for Repo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Repo({})", self.backend.location())
    }
}

/// init 的 replicas 預設（§10，同 v3）：本機 1，S3、SFTP 與 rclone 0。
pub fn default_replicas(location: &RepoLocation) -> u8 {
    match location {
        RepoLocation::Local(_) => 1,
        _ => 0,
    }
}

fn now_ns() -> Result<i64> {
    i64::try_from(time::OffsetDateTime::now_utc().unix_timestamp_nanos())
        .map_err(|_| CoreError::InvalidConfig("the clock is out of range".to_owned()))
}

/// 只讀 `v`：v3 的 config 欄位不同，先看版本才給得出看得懂的錯誤。
#[derive(serde::Deserialize)]
struct VersionOnly {
    v: u32,
}

impl Repo {
    /// 建立新 repo。`config` 已存在就拒絕（[`CoreError::RepoExists`]）。
    pub async fn init(backend: Backend, password: &[u8], opts: InitOptions) -> Result<Self> {
        let created = now_ns()?;
        let kdf = Kdf {
            alg: KDF_ARGON2ID.to_owned(),
            t: opts.kdf.t_cost,
            m: opts.kdf.m_cost_kib,
            p: opts.kdf.p_cost,
            salt: KdfSalt::from_bytes(kist_crypto::random_bytes()?),
        };
        let config = Config::new(
            RepoId::from_bytes(kist_crypto::random_bytes()?),
            created,
            opts.chunker,
            kdf,
        );
        let config_bytes = config.encode()?;
        let master = MasterKey::generate()?;
        let password = Zeroizing::new(password.to_vec());
        let (slot_name, slot) = {
            let config_bytes = config_bytes.clone();
            let kdf = config.kdf.clone();
            let master = master.clone();
            blocking(move || {
                let kek = derive_kek(&password, &kdf)?;
                let slot = wrap_master(&kek, &master, &config_bytes)?;
                Ok((kek.slot_name(), slot))
            })
            .await?
        };
        let keys = Arc::new(Keys::derive(&master));
        let settings = opts
            .settings
            .unwrap_or_else(|| Settings::defaults(default_replicas(backend.location())));
        let head = Head::initial(created, settings).encode()?;
        let head = keys.seal_head(0, &head)?;

        // 寬鬆後端（rclone://）上兩個 init 可能互相覆蓋 config：讀回比對攔下大多數交錯（同 v3）。
        match backend
            .put_if_absent_verified(&Key::Config.path(), config_bytes.clone())
            .await
        {
            Ok(()) => {}
            Err(BackendError::AlreadyExists(_)) => return Err(CoreError::RepoExists),
            Err(e) => return Err(e.into()),
        }
        backend
            .put_if_absent(&Key::Slot(slot_name).path(), slot.encode()?)
            .await?;
        let config_bytes = Arc::new(config_bytes);
        for replica in [false, true] {
            backend
                .put_if_absent(&Key::Head { g: 0, replica }.path(), head.clone())
                .await?;
        }
        Ok(Self {
            backend,
            config,
            keys,
            cache: None,
            master: Arc::new(master),
            config_bytes,
            slot: slot_name,
        })
    }

    /// 打開既有 repo。`cache_root` 底下放這個 repo（這個位置）的本地快取；`None` 不用快取。
    pub async fn open(
        backend: Backend,
        password: &[u8],
        cache_root: Option<&Path>,
    ) -> Result<Self> {
        let config_bytes = match backend.get(&Key::Config.path()).await {
            Ok(b) => b,
            Err(BackendError::NotFound(_)) => return Err(CoreError::NotARepository),
            Err(e) => return Err(e.into()),
        };
        let VersionOnly { v } = kist_format::cbor::decode(&config_bytes)
            .map_err(|e| CoreError::InvalidConfig(format!("config: {e}")))?;
        if v != kist_format::v4::VERSION {
            return Err(CoreError::InvalidConfig(format!(
                "the repository is format v{v}; this build reads v{} only (no migration)",
                kist_format::v4::VERSION
            )));
        }
        let config = Config::decode(&config_bytes)?;
        let password = Zeroizing::new(password.to_vec());
        let kdf = config.kdf.clone();
        let kek = blocking(move || Ok(derive_kek(&password, &kdf)?)).await?;
        // 槽的名稱由密碼推導：密碼錯了就是找不到這個槽（§3：只 GET 一個槽，不掃描）。
        let slot = match backend.get(&Key::Slot(kek.slot_name()).path()).await {
            Ok(b) => Slot::decode(&b)?,
            Err(BackendError::NotFound(_)) => {
                return Err(CoreError::Crypto(CryptoError::WrongPassword))
            }
            Err(e) => return Err(e.into()),
        };
        let slot_name = kek.slot_name();
        let master = unwrap_master(&kek, &slot, &config_bytes)?;
        let keys = Arc::new(Keys::derive(&master));
        let cache = cache_root.map(|root| {
            Cache::new(root.join(keys.cache_dir_name(&backend.location().to_string())))
        });
        Ok(Self {
            backend,
            config,
            keys,
            cache,
            master: Arc::new(master),
            config_bytes: Arc::new(config_bytes),
            slot: slot_name,
        })
    }

    pub fn backend(&self) -> &Backend {
        &self.backend
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn keys(&self) -> &Arc<Keys> {
        &self.keys
    }

    pub fn cache(&self) -> Option<&Cache> {
        self.cache.as_ref()
    }
}

/// 一個金鑰槽：名稱（32 位小寫 hex）與它是不是這個程序開啟時用的那一個。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct SlotInfo {
    pub name: String,
    pub current: bool,
}

impl Repo {
    /// `key add`（§3）：以新密碼推導 KEK 與槽的名稱，把 master 包進新的槽 `keys/<名稱>`（只在不存在時建立）。
    /// 這個密碼已經有槽就拒絕。回傳新槽的名稱。
    pub async fn key_add(&self, password: &[u8]) -> Result<String> {
        let kdf = self.config.kdf.clone();
        let master = Arc::clone(&self.master);
        let config_bytes = Arc::clone(&self.config_bytes);
        let password = Zeroizing::new(password.to_vec());
        let (name, slot) = blocking(move || {
            let kek = derive_kek(&password, &kdf)?;
            Ok((kek.slot_name(), wrap_master(&kek, &master, &config_bytes)?))
        })
        .await?;
        let key = Key::Slot(name).path();
        match self.backend.put_if_absent(&key, slot.encode()?).await {
            Ok(()) => Ok(hex::encode(name)),
            Err(BackendError::AlreadyExists(_)) => Err(CoreError::Usage(format!(
                "this password already has a key slot ({key})"
            ))),
            Err(e) => Err(e.into()),
        }
    }

    /// 列表裡的金鑰槽。
    pub async fn key_list(&self) -> Result<Vec<SlotInfo>> {
        let mut out = Vec::new();
        for o in self.backend.list(KEYS.trim_end_matches('/')).await? {
            match Key::parse(&o.key) {
                Ok(Key::Slot(name)) => out.push(SlotInfo {
                    name: hex::encode(name),
                    current: name == self.slot,
                }),
                _ => tracing::warn!("{}: not a v4 key slot; ignored", o.key),
            }
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    /// `key remove`（§3，維護角色）：刪掉名稱是 `name` 的槽。只在這個程序開啟時用的槽仍在列表裡、而且不是
    /// 要刪的那一個時才刪，所以最後一個槽刪不掉。同一台主機一次一個維護命令（檔案鎖）；不同主機同時互刪對方
    /// 的槽仍可能兩個都刪掉——這個命令很少用，接受這個風險。
    pub async fn key_remove(&self, name: &str) -> Result<()> {
        let target = parse_lower_hex::<16>("key slot name", name)
            .map_err(|e| CoreError::Usage(e.to_string()))?;
        if target == self.slot {
            return Err(CoreError::Usage(format!(
                "key slot {name} is the one this command opened the repository with; open it with \
                 another password to remove this one"
            )));
        }
        let _lock = super::maint::lock(self)?;
        let listed: Vec<[u8; 16]> = self
            .backend
            .list(KEYS.trim_end_matches('/'))
            .await?
            .into_iter()
            .filter_map(|o| match Key::parse(&o.key) {
                Ok(Key::Slot(n)) => Some(n),
                _ => None,
            })
            .collect();
        if !listed.contains(&self.slot) {
            return Err(CoreError::Usage(format!(
                "the key slot this command opened the repository with ({}) is no longer listed; \
                 not removing another one",
                hex::encode(self.slot)
            )));
        }
        if !listed.contains(&target) {
            return Err(CoreError::Usage(format!("no key slot {name}")));
        }
        self.backend.delete(&Key::Slot(target).path()).await?;
        Ok(())
    }
}
