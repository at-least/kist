//! 檔案樹的 chunk 邊界表：樹的 chunk 清單只有 id，但 index 記了每個 chunk 的
//! 明文長度（`PackEntry.raw_len`），所以「第 N byte 在哪個 chunk」不用解密就能算。
//! 這是 mount 隨機讀的根基（Go 參考實作得往前解碼 chunks 0..i 才學到邊界）。

/// `ends[i]` = 前 i+1 個 chunk 的明文長度總和（chunk i 的結尾 offset）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkOffsets {
    ends: Vec<u64>,
}

impl ChunkOffsets {
    /// 從每個 chunk 的明文長度建表（依樹的 chunk 順序）。
    pub fn from_raw_lens<'a>(raw_lens: impl IntoIterator<Item = &'a u64>) -> Self {
        let mut ends = Vec::new();
        let mut total = 0u64;
        for len in raw_lens {
            total += len;
            ends.push(total);
        }
        Self { ends }
    }

    pub fn total(&self) -> u64 {
        self.ends.last().copied().unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.ends.is_empty()
    }

    /// chunk i 的明文起點。
    pub fn chunk_start(&self, i: usize) -> u64 {
        match i {
            0 => 0,
            n => self.ends[n - 1],
        }
    }

    /// chunk i 的明文結尾。
    pub fn chunk_end(&self, i: usize) -> u64 {
        self.ends[i]
    }

    /// `offset` 落在哪個 chunk：回傳 (chunk 序號, chunk 內偏移)。
    /// 越過檔尾（offset ≥ total）回 `None`；長度 0 的 chunk 自動被跳過。
    pub fn locate(&self, offset: u64) -> Option<(usize, u64)> {
        let i = self.ends.partition_point(|&e| e <= offset);
        if i >= self.ends.len() {
            return None;
        }
        Some((i, offset - self.chunk_start(i)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn offsets(lens: &[u64]) -> ChunkOffsets {
        ChunkOffsets::from_raw_lens(lens.iter())
    }

    #[test]
    fn empty_file_has_no_chunks() {
        let o = offsets(&[]);
        assert_eq!(o.total(), 0);
        assert!(o.is_empty());
        assert_eq!(o.locate(0), None);
    }

    #[test]
    fn single_chunk_boundaries() {
        let o = offsets(&[100]);
        assert_eq!(o.total(), 100);
        assert_eq!(o.locate(0), Some((0, 0)));
        assert_eq!(o.locate(99), Some((0, 99)));
        assert_eq!(o.locate(100), None);
    }

    #[test]
    fn locate_across_many_chunks() {
        // chunk 長度 10, 20, 30, 40 → ends = 10, 30, 60, 100
        let o = offsets(&[10, 20, 30, 40]);
        assert_eq!(o.total(), 100);
        assert_eq!(o.locate(0), Some((0, 0)));
        assert_eq!(o.locate(9), Some((0, 9)));
        assert_eq!(o.locate(10), Some((1, 0))); // 正好落在第二顆開頭
        assert_eq!(o.locate(29), Some((1, 19)));
        assert_eq!(o.locate(30), Some((2, 0)));
        assert_eq!(o.locate(99), Some((3, 39)));
        assert_eq!(o.locate(100), None);
        assert_eq!(o.locate(u64::MAX), None);
    }

    #[test]
    fn zero_length_chunk_does_not_confuse_locate() {
        // 長度 0 的 chunk（理論上不會出現，但表不能壞掉）：ends 有重複值，
        // locate 必須跳過空 chunk 落在「下一顆非空 chunk」。
        let o = offsets(&[10, 0, 30]);
        assert_eq!(o.total(), 40);
        assert_eq!(o.locate(10), Some((2, 0)));
        assert_eq!(o.locate(0), Some((0, 0)));
        assert_eq!(o.locate(39), Some((2, 29)));
    }

    #[test]
    fn read_spanning_chunks_math() {
        // 模擬一個跨 chunk 讀：offset 25、長 40 → chunks 1(25..30) 2(30..60) 3(60..65)
        let o = offsets(&[10, 20, 30, 40]);
        let mut pos = 25u64;
        let end = 65u64;
        let mut out = Vec::new();
        while pos < end {
            let (i, in_chunk) = o.locate(pos).expect("pos < end ≤ total");
            let take = std::cmp::min(o.chunk_end(i) - pos, end - pos);
            out.push((i, o.chunk_start(i) + in_chunk, take));
            pos += take;
        }
        assert_eq!(
            out,
            vec![(1, 25, 5), (2, 30, 30), (3, 60, 5)],
            "每段 = (chunk, 全檔 offset 起點, 讀取長度)"
        );
    }
}
