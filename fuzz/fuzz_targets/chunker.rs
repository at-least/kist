//! chunker 的 fuzz target。
//!
//! 斷言（對照 boundary() 的語意，min 64 / avg 256 / max 1024 讓邊界在
//! 小輸入裡實際出現）：
//! - 同輸入切兩次，邊界相同（決定論）。
//! - 各塊長度總和 == 輸入長度（切塊是分割，不是過濾）。
//! - 每塊 <= max（硬邊界）。
//! - 非最後一塊 >= min（fill() 保證非 EOF 時剩餘 >= max，邊界函式
//!   從 min 起掃；只有輸入耗盡的尾巴可以不足 min）。
#![no_main]

use kist_chunker::Chunker;
use kist_format::config::ChunkerParams;
use libfuzzer_sys::fuzz_target;
use std::io::Cursor;

const SMALL: ChunkerParams = ChunkerParams {
    min: 64,
    avg: 256,
    max: 1024,
};

fn chunk_lengths(params: ChunkerParams, data: &[u8]) -> Vec<usize> {
    Chunker::new(params)
        .chunks(Cursor::new(data.to_vec()))
        .map(|r| {
            r.unwrap_or_else(|e| panic!("chunker errored on in-memory input: {e}"))
                .len()
        })
        .collect()
}

fuzz_target!(|data: &[u8]| {
    let first = chunk_lengths(SMALL, data);
    let second = chunk_lengths(SMALL, data);
    assert_eq!(first, second, "chunking is not deterministic");

    let total: usize = first.iter().sum();
    assert_eq!(total, data.len(), "chunks must reassemble the input");

    for (i, len) in first.iter().enumerate() {
        assert!(*len <= SMALL.max as usize, "chunk {i} is {len}, over max");
        if i + 1 < first.len() {
            assert!(
                *len >= SMALL.min as usize,
                "non-final chunk {i} is {len}, under min"
            );
        }
    }

    // 預設參數（大 min/avg/max）走過不 panic：fuzz 輸入對它永遠是「尾巴」。
    let _ = chunk_lengths(ChunkerParams::default(), data);
});
