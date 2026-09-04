//! kist 的核心流程：backup / restore / check / prune。
//!
//! 這個 crate 目前只是 M0 的骨架，實作在 M1 之後陸續補上。

#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used)]
