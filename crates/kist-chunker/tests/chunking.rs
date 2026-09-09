//! FastCDC 封裝的行為測試。

use std::io::Cursor;

use kist_chunker::Chunker;
use kist_format::config::ChunkerParams;
use proptest::prelude::*;
use rand::{Rng, SeedableRng};

fn random_bytes(seed: u64, len: usize) -> Vec<u8> {
    let mut rng = rand::rngs::StdRng::seed_from_u64(seed);
    let mut v = vec![0u8; len];
    rng.fill_bytes(&mut v);
    v
}

/// 測試用小參數：4 KiB / 16 KiB / 64 KiB。
fn small() -> Chunker {
    Chunker::new(ChunkerParams {
        min: 4 * 1024,
        avg: 16 * 1024,
        max: 64 * 1024,
    })
}

#[test]
fn chunks_concatenate_back_to_input_and_respect_bounds() {
    let data = random_bytes(1, 1_000_000);
    let chunks: Vec<Vec<u8>> = small()
        .chunks(Cursor::new(&data))
        .collect::<Result<_, _>>()
        .unwrap();
    assert!(
        chunks.len() > 10,
        "1 MiB 的隨機資料應該切成很多塊，實際 {}",
        chunks.len()
    );
    let joined: Vec<u8> = chunks.concat();
    assert_eq!(joined, data);
    for (i, c) in chunks.iter().enumerate() {
        assert!(c.len() <= 64 * 1024, "chunk {i} 超過 max");
        if i + 1 < chunks.len() {
            assert!(
                c.len() >= 4 * 1024,
                "chunk {i} 小於 min（只有最後一塊可以）"
            );
        }
    }
}

#[test]
fn empty_input_yields_no_chunks() {
    let chunks: Vec<_> = small().chunks(Cursor::new(Vec::<u8>::new())).collect();
    assert!(chunks.is_empty());
}

#[test]
fn small_input_is_one_chunk() {
    let data = random_bytes(2, 100);
    let chunks: Vec<Vec<u8>> = small()
        .chunks(Cursor::new(&data))
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(chunks, vec![data]);
}

#[test]
fn inserting_bytes_only_changes_nearby_chunks() {
    // 內容定義切塊的重點：在中間插入幾個 byte，後面的 chunk 邊界要能重新對齊。
    let data = random_bytes(3, 2_000_000);
    let mut modified = data.clone();
    modified.splice(1_000_000..1_000_000, [1u8, 2, 3, 4, 5]);

    let a: Vec<Vec<u8>> = small()
        .chunks(Cursor::new(&data))
        .collect::<Result<_, _>>()
        .unwrap();
    let b: Vec<Vec<u8>> = small()
        .chunks(Cursor::new(&modified))
        .collect::<Result<_, _>>()
        .unwrap();
    let set_a: std::collections::HashSet<&[u8]> = a.iter().map(Vec::as_slice).collect();
    let unchanged = b.iter().filter(|c| set_a.contains(c.as_slice())).count();
    assert!(
        unchanged * 10 >= b.len() * 9,
        "插入 5 bytes 後至少 90% 的 chunk 應該不變：{unchanged}/{}",
        b.len()
    );
}

#[test]
fn default_params_match_plan() {
    let p = ChunkerParams::default();
    assert_eq!(
        (p.min, p.avg, p.max),
        (512 * 1024, 2 * 1024 * 1024, 8 * 1024 * 1024)
    );
}

#[test]
fn read_error_is_propagated() {
    struct Broken;
    impl std::io::Read for Broken {
        fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("disk on fire"))
        }
    }
    let mut it = small().chunks(Broken);
    let err = it.next().unwrap().unwrap_err();
    assert!(err.to_string().contains("disk on fire"), "{err}");
}

/// 重用緩衝的結果必須與每次全新配置完全相同，且緩衝可以跨檔案反覆重用
///（backup 每個檔案建一次 iterator；1 MiB 內容 × 數輪）。
#[test]
fn reused_buffer_matches_fresh() {
    let data = random_bytes(4, 1_000_000);
    let fresh: Vec<Vec<u8>> = small()
        .chunks(Cursor::new(&data))
        .collect::<Result<_, _>>()
        .unwrap();
    let max_cap = 2 * 64 * 1024;
    let mut buf = Vec::new(); // 故意從空 buf 開始：API 要自己調整大小
    for round in 0..3 {
        let mut it = small().chunks_with_buf(Cursor::new(&data), std::mem::take(&mut buf));
        let got: Vec<Vec<u8>> = it
            .by_ref()
            .collect::<Result<_, _>>()
            .unwrap_or_else(|e| panic!("round {round}: {e}"));
        assert_eq!(got, fresh, "round {round}: 重用緩衝改變了切塊結果");
        buf = it.take_buf();
        assert_eq!(buf.len(), max_cap, "take_buf 應交回完整緩衝");
    }
}

/// 讀取錯誤之後緩衝一樣要拿得回來（backup 的錯誤路徑要回收 16 MiB）。
#[test]
fn buffer_survives_read_error() {
    struct Broken;
    impl std::io::Read for Broken {
        fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("disk on fire"))
        }
    }
    let mut it = small().chunks_with_buf(Broken, Vec::new());
    assert!(it.next().unwrap().is_err());
    let buf = it.take_buf();
    assert_eq!(buf.len(), 2 * 64 * 1024);
}

proptest! {
    #[test]
    fn concat_is_identity(data in proptest::collection::vec(any::<u8>(), 0..200_000)) {
        let chunks: Vec<Vec<u8>> = small().chunks(Cursor::new(&data)).collect::<Result<_, _>>().unwrap();
        prop_assert_eq!(chunks.concat(), data);
    }
}
