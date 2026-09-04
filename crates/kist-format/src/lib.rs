//! kist 的 on-disk 格式：所有結構定義、CBOR 序列化與版本演進。其他 crate 只能透過這裡讀寫 repo 內容。
//!
//! 這個 crate 目前只是 M0 的骨架，實作在 M1 之後陸續補上。

#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used)]
