//! kist 的儲存後端抽象（object_store：local / S3 / GCS / Azure）與本地 index 快取。
//!
//! 這個 crate 目前只是 M0 的骨架，實作在 M1 之後陸續補上。

#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used)]
