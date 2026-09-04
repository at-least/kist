//! kist 的金鑰階層（password → KEK → master key → 派生子金鑰）與 AEAD 封裝。
//!
//! 這個 crate 目前只是 M0 的骨架，實作在 M1 之後陸續補上。

#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used)]
