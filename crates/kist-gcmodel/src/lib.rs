//! v4 GC 協定的可執行模型（ADR 026 實作第一段）。
//!
//! 這不是正式程式碼。它把 `docs/format-v4.md` §11 的 B1–B5、P0–P9、重建、settings 與 grace 的
//! drain 寫成純邏輯的狀態機，搭配記憶體後端、可控制的時鐘與可注入的故障，讓 proptest 跑各種交錯，
//! 每一步之後檢查不變式（[`check`]）。模型找到的問題先回頭改規格。
//!
//! - 每個 actor（backup、prune、forget、reader、settings、rebuild）是一個顯式的 phase 狀態機，
//!   `step()` 一次只做一個後端操作，或規格上屬於同一步的一小組讀取。
//! - 排程器（[`World::apply`]）依 [`Action`] 清單推進時間、啟動 actor、推進某個 actor 一步、讓它
//!   當掉、注入故障或惡意寫入。proptest 縮小的是這份清單，所以失敗時得到的是最短的交錯。
//! - 真相與協定狀態分開：item 的 DAG、每個 item 的真副本、誠實 client commit 過的 snapshot 都在
//!   [`Core`]，actor 只能經由後端操作看到 repo。
//!
//! 時間單位是秒。物件的修改時間 = 後端時鐘 − ε（ε ∈ [0, `eps_max`]：修改時間是請求開始的時間，
//! 物件在請求結束才看得到）。backup 量到的經過時間 = 真實經過 − u（u ∈ [0, `under_max`]：時鐘的
//! 低估加上 snapshot PUT 的延遲）。規格的假設是 `eps_max + under_max < M`。
//!
//! 模型刻意省略的（不影響 R1 的論證，或屬於別的實作階段）：parity、Object Lock、Dir 與 ChunkIndex
//! 的巢狀（目錄只有一段）、snapshot 時間戳的 +1 ns 重試（時間戳一律唯一）、加密與壓縮（以「真／假
//! 副本」與「讀得了／讀不了」旗標代替）、大小上限、本地 index 快取（只保留沿用前驗證的結果）、map
//! 分片的 `.r1`（分片遺失以故障注入代替）。map 分片是 4 個而不是 256 個，增刪規則相同。

#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used)]

mod backend;
mod backup;
mod check;
mod core;
mod maint;
mod prune;
mod reader;
mod types;
mod universe;
mod world;

pub use crate::backend::{Backend, Obj};
pub use crate::check::check;
pub use crate::core::{Config, Core, Mutations, RepackPolicy, Stats};
pub use crate::types::*;
pub use crate::universe::{ItemDef, Node, Universe};
pub use crate::world::{Action, ActorId, Mal, Outcome, StepResult, World};
