//! §1 的命名空間：每種物件的 key。
//!
//! ```text
//! config                       明文 CBOR，不可變
//! keys/<槽的名稱>               金鑰槽；名稱是 DeriveKey("kist/v4/slot-name", KEK) 的前 16 bytes
//! packs/<c>/<hex>              資料 pack（不寫副本）
//! mpacks/<c>/<hex>[.r1]        metadata pack
//! parity/<c>/<hex>             同名 pack 或 mpack 的 Reed-Solomon 同位
//! deltas/<c>/<hex>             delta
//! snapshots/<c>/<ts>[.r1]      snapshot
//! maint/gen/<g>[.r1]           世代 head（一律有 `.r1`）
//! maint/map/<hex>[.r1]         map 分片
//! maint/probe-<hex>            P0 的探針，用完即刪
//! ```
//!
//! `<c>` 與槽的名稱、探針是 16 bytes 的 32 位小寫 hex；`<hex>` 是物件 bytes 的 BLAKE3（64 位小寫 hex）；
//! `<g>` 是世代號碼的 16 位小寫 hex（字典序＝數值序）；`<ts>` 是 `YYYYMMDDTHHMMSSnnnnnnnnnZ`（UTC）。
//! 解析只接受這一種寫法：大寫 hex、多或少一段、不允許副本的種類帶 `.r1`，都拒絕。

use time::format_description::FormatItem;
use time::macros::format_description;
use time::OffsetDateTime;

use super::ids::{parse_lower_hex, ClientId, ObjectId};
use super::{corrupt, Result};

pub const CONFIG: &str = "config";
pub const KEYS: &str = "keys/";
pub const PACKS: &str = "packs/";
pub const MPACKS: &str = "mpacks/";
pub const PARITY: &str = "parity/";
pub const DELTAS: &str = "deltas/";
pub const SNAPSHOTS: &str = "snapshots/";
pub const GEN: &str = "maint/gen/";
pub const MAP: &str = "maint/map/";
pub const PROBE: &str = "maint/probe-";
/// 副本的後綴：bytes 與主體相同（§13）。
pub const REPLICA: &str = ".r1";

/// 世代號碼。
pub type Gen = u64;

const TS_FORMAT: &[FormatItem<'static>] =
    format_description!("[year][month][day]T[hour][minute][second][subsecond digits:9]Z");

/// snapshot key 裡的時間戳：建立時就驗證過，之後產生路徑不會失敗。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SnapTs {
    nanos: i64,
    text: String,
}

impl SnapTs {
    /// 從 Unix 奈秒（UTC）建立。
    pub fn from_nanos(nanos: i64) -> Result<Self> {
        let t = OffsetDateTime::from_unix_timestamp_nanos(i128::from(nanos))
            .map_err(|e| corrupt("snapshot time", e.to_string()))?;
        let text = t
            .format(TS_FORMAT)
            .map_err(|e| corrupt("snapshot time", e.to_string()))?;
        Ok(Self { nanos, text })
    }

    /// 解析 key 裡的時間戳；必須是規範寫法（重新格式化後完全相同）。
    pub fn parse(s: &str) -> Result<Self> {
        let t = time::PrimitiveDateTime::parse(s, TS_FORMAT)
            .map_err(|e| corrupt("snapshot time", format!("{s:?}: {e}")))?
            .assume_utc();
        let nanos = i64::try_from(t.unix_timestamp_nanos())
            .map_err(|_| corrupt("snapshot time", format!("{s:?} is out of range")))?;
        let ts = Self::from_nanos(nanos)?;
        if ts.text != s {
            return Err(corrupt("snapshot time", format!("{s:?} is not canonical")));
        }
        Ok(ts)
    }

    pub fn nanos(&self) -> i64 {
        self.nanos
    }

    pub fn as_str(&self) -> &str {
        &self.text
    }
}

/// repo 裡的一個 key。`replica` 只出現在可以有副本的種類。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Key {
    Config,
    Slot([u8; 16]),
    Pack {
        client: ClientId,
        id: ObjectId,
    },
    Mpack {
        client: ClientId,
        id: ObjectId,
        replica: bool,
    },
    Parity {
        client: ClientId,
        id: ObjectId,
    },
    Delta {
        client: ClientId,
        id: ObjectId,
    },
    Snapshot {
        client: ClientId,
        ts: SnapTs,
        replica: bool,
    },
    Head {
        g: Gen,
        replica: bool,
    },
    Shard {
        id: ObjectId,
        replica: bool,
    },
    Probe([u8; 16]),
}

impl Key {
    pub fn path(&self) -> String {
        let r1 = |replica: bool| if replica { REPLICA } else { "" };
        match self {
            Key::Config => CONFIG.to_owned(),
            Key::Slot(name) => format!("{KEYS}{}", hex::encode(name)),
            Key::Pack { client, id } => format!("{PACKS}{client}/{id}"),
            Key::Mpack {
                client,
                id,
                replica,
            } => format!("{MPACKS}{client}/{id}{}", r1(*replica)),
            Key::Parity { client, id } => format!("{PARITY}{client}/{id}"),
            Key::Delta { client, id } => format!("{DELTAS}{client}/{id}"),
            Key::Snapshot {
                client,
                ts,
                replica,
            } => format!("{SNAPSHOTS}{client}/{}{}", ts.as_str(), r1(*replica)),
            Key::Head { g, replica } => format!("{GEN}{g:016x}{}", r1(*replica)),
            Key::Shard { id, replica } => format!("{MAP}{id}{}", r1(*replica)),
            Key::Probe(nonce) => format!("{PROBE}{}", hex::encode(nonce)),
        }
    }

    /// 主體的 key（副本去掉 `.r1`）。
    pub fn primary(&self) -> Key {
        let mut k = self.clone();
        match &mut k {
            Key::Mpack { replica, .. }
            | Key::Snapshot { replica, .. }
            | Key::Head { replica, .. }
            | Key::Shard { replica, .. } => *replica = false,
            _ => {}
        }
        k
    }

    pub fn parse(s: &str) -> Result<Key> {
        let bad = || corrupt("key", format!("{s:?} is not a v4 key"));
        if s == CONFIG {
            return Ok(Key::Config);
        }
        let (body, replica) = match s.strip_suffix(REPLICA) {
            Some(b) => (b, true),
            None => (s, false),
        };
        let no_replica = |k: Key| if replica { Err(bad()) } else { Ok(k) };
        let parts: Vec<&str> = body.split('/').collect();
        match parts.as_slice() {
            ["keys", name] => no_replica(Key::Slot(parse_lower_hex::<16>("slot name", name)?)),
            ["packs", c, id] => no_replica(Key::Pack {
                client: ClientId::from_hex(c)?,
                id: object_id(id)?,
            }),
            ["mpacks", c, id] => Ok(Key::Mpack {
                client: ClientId::from_hex(c)?,
                id: object_id(id)?,
                replica,
            }),
            ["parity", c, id] => no_replica(Key::Parity {
                client: ClientId::from_hex(c)?,
                id: object_id(id)?,
            }),
            ["deltas", c, id] => no_replica(Key::Delta {
                client: ClientId::from_hex(c)?,
                id: object_id(id)?,
            }),
            ["snapshots", c, ts] => Ok(Key::Snapshot {
                client: ClientId::from_hex(c)?,
                ts: SnapTs::parse(ts)?,
                replica,
            }),
            ["maint", "gen", g] => Ok(Key::Head {
                g: Gen::from_be_bytes(parse_lower_hex::<8>("generation", g)?),
                replica,
            }),
            ["maint", "map", id] => Ok(Key::Shard {
                id: object_id(id)?,
                replica,
            }),
            ["maint", probe] => match probe.strip_prefix("probe-") {
                Some(nonce) => no_replica(Key::Probe(parse_lower_hex::<16>("probe", nonce)?)),
                None => Err(bad()),
            },
            _ => Err(bad()),
        }
    }
}

fn object_id(s: &str) -> Result<ObjectId> {
    Ok(ObjectId::from_bytes(parse_lower_hex::<32>(
        "object name",
        s,
    )?))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn samples() -> Vec<Key> {
        let c = ClientId::from_bytes([0x1f; 16]);
        let id = ObjectId::from_bytes([0xa0; 32]);
        let ts = SnapTs::from_nanos(1_788_523_200_123_456_789).unwrap();
        vec![
            Key::Config,
            Key::Slot([0x5a; 16]),
            Key::Pack { client: c, id },
            Key::Mpack {
                client: c,
                id,
                replica: false,
            },
            Key::Mpack {
                client: c,
                id,
                replica: true,
            },
            Key::Parity { client: c, id },
            Key::Delta {
                client: ClientId::MAINT,
                id,
            },
            Key::Snapshot {
                client: c,
                ts: ts.clone(),
                replica: false,
            },
            Key::Snapshot {
                client: c,
                ts,
                replica: true,
            },
            Key::Head {
                g: 0,
                replica: false,
            },
            Key::Head {
                g: 0x0123_4567_89ab_cdef,
                replica: true,
            },
            Key::Shard { id, replica: false },
            Key::Shard { id, replica: true },
            Key::Probe([0x33; 16]),
        ]
    }

    #[test]
    fn every_key_round_trips() {
        for k in samples() {
            let p = k.path();
            assert_eq!(Key::parse(&p).unwrap(), k, "{p}");
        }
    }

    #[test]
    fn paths_match_the_namespace_table() {
        let c = "1f".repeat(16);
        let h = "a0".repeat(32);
        let paths: Vec<String> = samples().iter().map(Key::path).collect();
        assert_eq!(
            paths,
            vec![
                "config".to_owned(),
                format!("keys/{}", "5a".repeat(16)),
                format!("packs/{c}/{h}"),
                format!("mpacks/{c}/{h}"),
                format!("mpacks/{c}/{h}.r1"),
                format!("parity/{c}/{h}"),
                format!("deltas/{}/{h}", "0".repeat(32)),
                format!("snapshots/{c}/20260904T120000123456789Z"),
                format!("snapshots/{c}/20260904T120000123456789Z.r1"),
                "maint/gen/0000000000000000".to_owned(),
                "maint/gen/0123456789abcdef.r1".to_owned(),
                format!("maint/map/{h}"),
                format!("maint/map/{h}.r1"),
                format!("maint/probe-{}", "33".repeat(16)),
            ]
        );
    }

    #[test]
    fn non_canonical_keys_are_rejected() {
        let c = "1f".repeat(16);
        let h = "a0".repeat(32);
        for bad in [
            format!("packs/{c}/{h}.r1"), // 資料 pack 不寫副本
            format!("parity/{c}/{h}.r1"),
            format!("deltas/{c}/{h}.r1"),
            format!("keys/{}.r1", "5a".repeat(16)),
            "config.r1".to_owned(),
            format!("packs/{}/{h}", c.to_uppercase()),
            format!("packs/{c}/{}", h.to_uppercase()),
            format!("packs/{c}/{h}/x"),
            format!("packs/{h}"),
            format!("trees/{h}"), // v3 的命名空間
            format!("indexes/{h}"),
            "maint/gen/123".to_owned(), // 不是 16 位
            "maint/gen/0123456789ABCDEF".to_owned(),
            format!("snapshots/{c}/20260904T120000Z"),
            format!("snapshots/{c}/20260904T120000123456789z"),
            "maint/probe-".to_owned(),
            format!("maint/prob-{}", "33".repeat(16)),
            String::new(),
        ] {
            assert!(Key::parse(&bad).is_err(), "{bad} should be rejected");
        }
    }

    #[test]
    fn generation_order_is_lexicographic_order() {
        let gens = [0u64, 1, 9, 10, 15, 16, 255, 256, 0xffff_ffff, u64::MAX];
        let mut paths: Vec<String> = gens
            .iter()
            .map(|&g| Key::Head { g, replica: false }.path())
            .collect();
        let sorted = paths.clone();
        paths.sort();
        assert_eq!(paths, sorted);
    }

    #[test]
    fn primary_drops_the_replica_suffix() {
        let k = Key::Head {
            g: 3,
            replica: true,
        };
        assert_eq!(
            k.primary(),
            Key::Head {
                g: 3,
                replica: false
            }
        );
        assert_eq!(Key::Config.primary(), Key::Config);
    }

    #[test]
    fn snapshot_time_is_canonical_and_exact() {
        for ns in [
            0i64,
            1,
            999_999_999,
            1_788_523_200_123_456_789,
            -1,
            i64::MIN,
            i64::MAX,
        ] {
            let ts = SnapTs::from_nanos(ns).unwrap();
            assert_eq!(
                SnapTs::parse(ts.as_str()).unwrap().nanos(),
                ns,
                "{}",
                ts.as_str()
            );
        }
    }
}
