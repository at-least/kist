//! 記憶體後端：只在不存在時建立的 PUT、GET、LIST、DELETE，修改時間由後端時鐘決定（§12）。

use std::collections::BTreeMap;

use crate::types::{Content, Key, Time};

#[derive(Clone, Debug)]
pub struct Obj {
    pub content: Content,
    pub mtime: Time,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Exists;

#[derive(Clone, Debug, Default)]
pub struct Backend {
    objects: BTreeMap<Key, Obj>,
    /// false ＝ 寬鬆後端（rclone、B2、Wasabi）：第二次建立也成功，蓋掉原本的物件。
    pub create_only: bool,
    /// 後端時鐘 ＝ 真實時間 + skew。
    pub skew: Time,
}

impl Backend {
    pub fn new(create_only: bool) -> Self {
        Backend {
            objects: BTreeMap::new(),
            create_only,
            skew: 0,
        }
    }

    pub fn put(&mut self, key: Key, content: Content, mtime: Time) -> Result<(), Exists> {
        if self.create_only && self.objects.contains_key(&key) {
            return Err(Exists);
        }
        self.objects.insert(key, Obj { content, mtime });
        Ok(())
    }

    pub fn get(&self, key: &Key) -> Option<&Obj> {
        self.objects.get(key)
    }

    pub fn exists(&self, key: &Key) -> bool {
        self.objects.contains_key(key)
    }

    pub fn delete(&mut self, key: &Key) -> bool {
        self.objects.remove(key).is_some()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&Key, &Obj)> {
        self.objects.iter()
    }

    pub fn keys(&self) -> impl Iterator<Item = &Key> {
        self.objects.keys()
    }
}
