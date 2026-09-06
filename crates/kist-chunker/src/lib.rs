//! kist 的內容定義切塊：FastCDC（Xia et al., USENIX ATC '16，normalized
//! level 2），**與 Go 實作逐 byte 相同的移植**（`docs/format.md` §12）。
//!
//! 邊界函式是凍結的儲存格式：gear 表 = fastcdc-go v0.2.0 的表（兩邊以
//! digest 測試釘死），mask 由 `avg` 以整數運算推導（避免浮點誤差讓兩個
//! 實作分岔）。參數來自 repo 的 `config`，不是寫死的：同一個 repo 的所有
//! client 必須用同樣的參數，去重才會一致。
//!
//! 與 Go 版相同的緩衝不變量：決定邊界前，掃描位置之後保證 ≥ `max`
//! bytes（除非輸入已耗盡）——這讓邊界與 reader 的分塊方式無關。

#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used)]

use std::io::Read;

use kist_format::config::ChunkerParams;

/// gear 表：每個可能的輸入 byte 一個 64-bit 值。逐 byte 取自
/// fastcdc-go v0.2.0，是凍結的格式常數（見模組說明）。
/// LE 串接的 SHA-256 =
/// a98fa4184eb747cd769328307242285f9d4b25afeb28f71ef3fe437a4c278e28
pub const GEAR_TABLE: [u64; 256] = [
    0xe80e8d55032474b3, 0x11b25b61f5924e15, 0x03aa5bd82a9eb669, 0xc45a153ef107a38c,
    0xeac874b86f0f57b9, 0xa5ccedec95ec79c7, 0xe15a3320ad42ac0a, 0x5ed3583fa63cec15,
    0xcd497bf624a4451d, 0xf9ade5b059683605, 0x773940c03fb11ca1, 0xa36b16e4a6ae15b2,
    0x67afd1adb5a89eac, 0xc44c75ee32f0038e, 0x2101790f365c0967, 0x76415c64a222fc4a,
    0x579929249a1e577a, 0xe4762fc41fdbf750, 0xea52198e57dfcdcc, 0xe2535aafe30b4281,
    0xcb1a1bd6c77c9056, 0x5a1aa9bfc4612a62, 0x15a728aef8943eb5, 0x2f8f09738a8ec8d9,
    0x200f3dec9fac8074, 0x0fa9a7b1e0d318df, 0x06c0804ffd0d8e3a, 0x630cbc412669dd25,
    0x10e34f85f4b10285, 0x2a6fe8164b9b6410, 0xcacb57d857d55810, 0x77f8a3a36ff11b46,
    0x66af517e0dc3003e, 0x76c073c789b4009a, 0x853230dbb529f22a, 0x1e9e9c09a1f77e56,
    0x1e871223802ee65d, 0x37fe4588718ff813, 0x10088539f30db464, 0x366f7470b80b72d1,
    0x33f2634d9a6b31db, 0xd43917751d69ea18, 0xa0f492bc1aa7b8de, 0x3f94e5a8054edd20,
    0xedfd6e25eb8b1dbf, 0x759517a54f196a56, 0xe81d5006ec7b6b17, 0x8dd8385fa894a6b7,
    0x45f4d5467b0d6f91, 0xa1f894699de22bc8, 0x33829d09ef93e0fe, 0x3e29e250caed603c,
    0xf7382cba7f63a45e, 0x970f95412bb569d1, 0xc7fcea456d356b4b, 0x723042513f3e7a57,
    0x17ae7688de3596f1, 0x27ac1fcd7cd23c1a, 0xf429beeb78b3f71f, 0xd0780692fb93a3f9,
    0x9f507e28a7c9842f, 0x56001ad536e433ae, 0x7e1dd1ecf58be306, 0x15fee353aa233fc6,
    0xb033a0730b7638e8, 0xeb593ad6bd2406d1, 0x7c86502574d0f133, 0xce3b008d4ccb4be7,
    0xf8566e3d383594c8, 0xb2c261e9b7af4429, 0xf685e7e253799dbb, 0x05d33ed60a494cbc,
    0xeaf88d55a4cb0d1a, 0x3ee9368a902415a1, 0x8980fe6a8493a9a4, 0x358ed008cb448631,
    0xd0cb7e37b46824b8, 0xe9bc375c0bc94f84, 0xea0bf1d8e6b55bb3, 0xb66a60d0f9f6f297,
    0x66db2cc4807b3758, 0x7e4e014afbca8b4d, 0xa5686a4938b0c730, 0xa5f0d7353d623316,
    0x26e38c349242d5e8, 0xeeefa80a29858e30, 0x8915cb912aa67386, 0x4b957a47bfc420d4,
    0xbb53d051a895f7e1, 0x09f5e3235f6911ce, 0x416b98e695cfb7ce, 0x97a08183344c5c86,
    0xbf68e0791839a861, 0xea05dde59ed3ed56, 0x0ca732280beda160, 0xac748ed62fe7f4e2,
    0xc686da075cf6e151, 0xe1ba5658f4af05c8, 0xe9ff09fbeb67cc35, 0xafaea9470323b28d,
    0x0291e8db5bb0ac2a, 0x342072a9bbee77ae, 0x03147eed6b3d0a9c, 0x21379d4de31dbadb,
    0x2388d965226fb986, 0x52c96988bfebabfa, 0xa6fc29896595bc2d, 0x38fa4af70aa46b8b,
    0xa688dd13939421ee, 0x99d5275d9b1415da, 0x453d31bb4fe73631, 0xde51debc1fbe3356,
    0x75a3c847a06c622f, 0xe80e32755d272579, 0x5444052250d8ec0d, 0x8f17dfda19580a3b,
    0xf6b3e9363a185e42, 0x7a42adec6868732f, 0x32cb6a07629203a2, 0x1eca8957defe56d9,
    0x9fa85e4bc78ff9ed, 0x20ff07224a499ca7, 0x3fa6295ff9682c70, 0xe3d5b1e3ce993eff,
    0xa341209362e0b79a, 0x64bd9eae5712ffe8, 0xceebb537babbd12a, 0x5586ef404315954f,
    0x46c3085c938ab51a, 0xa82ccb9199907cee, 0x8c51b6690a3523c8, 0xc4dbd4c9ae518332,
    0x979898dbb23db7b2, 0x1b5b585e6f672a9d, 0xce284da7c4903810, 0x841166e8bb5f1c4f,
    0xb7d884a3fceca7d0, 0xa76468f5a4572374, 0xc10c45f49ee9513d, 0x68f9a5663c1908c9,
    0x0095a13476a6339d, 0xd1d7516ffbe9c679, 0xfd94ab0c9726f938, 0x627468bbdb27c959,
    0xedc3f8988e4a8c9a, 0x58efd33f0dfaa499, 0x21e37d7e2ef4ac8b, 0x297f9ab5586259c6,
    0xda3ba4dc6cb9617d, 0xae11d8d9de2284d2, 0xcfeed88cb3729865, 0xefc2f9e4f03e2633,
    0x8226393e8f0855a4, 0xd6e25fd7acf3a767, 0x435784c3bfd6d14a, 0xf97142e6343fe757,
    0xd73b9fe826352f85, 0x6c3ac444b5b2bd76, 0xd8e88f3e9fd4a3fd, 0x31e50875c36f3460,
    0xa824f1bf88cf4d44, 0x54a4d2c8f5f25899, 0xbff254637ce3b1e6, 0xa02cfe92561b3caa,
    0x7bedb4edee9f0af7, 0x879c0620ac49a102, 0xa12c4ccd23b332e7, 0x09a5ff47bf94ed1e,
    0x7b62f43cd3046fa0, 0xaa3af0476b9c2fb9, 0x22e55301abebba8e, 0x3a6035c42747bd58,
    0x1705373106c8ec07, 0xb1f660de828d0628, 0x065fe82d89ca563d, 0xf555c2d8074d516d,
    0x6bb6c186b423ee99, 0x54a807be6f3120a8, 0x8a3c7fe2f88860b8, 0xbeffc344f5118e81,
    0xd686e80b7d1bd268, 0x661aef4ef5e5e88b, 0x5bf256c654cd1dda, 0x9adb1ab85d7640f4,
    0x68449238920833a2, 0x843279f4cebcb044, 0xc8710cdefa93f7bb, 0x236943294538f3e6,
    0x80d7d136c486d0b4, 0x61653956b28851d3, 0x3f843be9a9a956b5, 0xf73cfbbf137987e5,
    0xcf0cb6dee8ceac2c, 0x50c401f52f185cae, 0xbdbe89ce735c4c1c, 0xeef3ade9c0570bc7,
    0xbe8b066f8f64cbf6, 0x5238d6131705dcb9, 0x20219086c950e9f6, 0x634468d9ed74de02,
    0x0aba4b3d705c7fa5, 0x3374416f725a6672, 0xe7378bdf7beb3bc6, 0x0f7b6a1b1cee565b,
    0x234e4c41b0c33e64, 0x4efa9a0c3f21fe28, 0x1167fc551643e514, 0x9f81a69d3eb01fa4,
    0xdb75c22b12306ed0, 0xe25055d738fc9686, 0x9f9f167a3f8507bb, 0x195f8336d3fbe4d3,
    0x8442b6feffdcb6f6, 0x1e07ed24746ffde9, 0x140e31462d555266, 0x8bd0ce515ae1406e,
    0x2c0be0042b5584b3, 0x35a23d0e15d45a60, 0xc14f1ba147d9bc83, 0xbbf168691264b23f,
    0xad2cc7b57e589ade, 0x9501963154c7815c, 0x9664afa6b8d67d47, 0x7f9e5101fea0a81c,
    0x45ecffb610d25bfd, 0x3157f7aecf9b6ab3, 0xc43ca6f88d87501d, 0x9576ff838dee38dc,
    0x93f21afe0ce1c7d7, 0xceac699df343d8f9, 0x2fec49e29f03398d, 0x8805ccd5730281ed,
    0xf9fc16fc750a8e59, 0x35308cc771adf736, 0x4a57b7c9ee2b7def, 0x03a4c6cdc937a02a,
    0x6c9a8a269fc8c4fc, 0x4681decec7a03f43, 0x342eecded1353ef9, 0x8be0552d8413a867,
    0xc7b4ac51beda8be8, 0xebcc64fb719842c0, 0xde8e4c7fb6d40c1c, 0xcc8263b62f9738b1,
    0xd3cfc0f86511929a, 0x466024ce8bb226ea, 0x459ff690253a3c18, 0x98b27e9d91284c9c,
    0x75c3ae8aa3af373d, 0xfbf8f8e79a866ffc, 0x32327f59d0662799, 0x8228b57e729e9830,
    0x065ceb7a18381b58, 0xd2177671a31dc5ff, 0x90cd801f2f8701f9, 0x9d714428471c65fe,
];

#[derive(Debug, thiserror::Error)]
pub enum ChunkerError {
    #[error("read error while chunking: {0}")]
    Io(std::io::Error),
}

#[derive(Debug, Clone, Copy)]
pub struct Chunker {
    params: ChunkerParams,
    mask_small: u64,
    mask_large: u64,
}

impl Chunker {
    pub fn new(params: ChunkerParams) -> Self {
        let bits = round_log2(params.avg);
        Self {
            params,
            mask_small: (1u64 << (bits + 2)) - 1,
            mask_large: (1u64 << (bits.saturating_sub(2))) - 1,
        }
    }

    pub fn params(&self) -> ChunkerParams {
        self.params
    }

    /// 以串流方式切塊。每個項目是一個 chunk 的明文；讀取錯誤以 `Err`
    /// 交出並終止。空輸入不產生任何 chunk。
    pub fn chunks<R: Read>(&self, reader: R) -> Chunks<R> {
        Chunks {
            state: State {
                params: self.params,
                mask_small: self.mask_small,
                mask_large: self.mask_large,
                reader: Some(reader),
                buf: vec![0u8; 2 * self.params.max as usize],
                len: 0,
                cursor: 0,
                eof: false,
            },
        }
    }

    /// 同 [`chunks`]，但重用呼叫端的緩衝：省掉每個輸入一次 2×max 的
    /// 配置與歸零（backup 逐檔切塊時原本每檔都要配一次）。`buf` 的長度
    /// 會被調整成 2×max，內容不需要預先清掉——`len`/`cursor` 之外的
    /// 位元組不會被讀。iterator 跑完或讀取出錯之後，用
    /// [`Chunks::take_buf`] 把緩衝拿回去給下一個輸入重用；
    /// **`take_buf` 之後不可再迭代**（緩衝已被拿走）。
    pub fn chunks_with_buf<R: Read>(&self, reader: R, mut buf: Vec<u8>) -> Chunks<R> {
        buf.resize(2 * self.params.max as usize, 0);
        Chunks {
            state: State {
                params: self.params,
                mask_small: self.mask_small,
                mask_large: self.mask_large,
                reader: Some(reader),
                buf,
                len: 0,
                cursor: 0,
                eof: false,
            },
        }
    }
}

/// 整數的 round(log2(v))：v >= 2^b·√2 時進位到 b+1。用整數比較
/// （v² vs 2^(2b+1)）避免浮點捨入在兩個實作間分岔。
fn round_log2(v: u32) -> u32 {
    debug_assert!(v >= 2);
    let mut b = 0u32;
    while (1u64 << (b + 1)) <= u64::from(v) {
        b += 1;
    }
    // 進位條件：v >= 2^b * sqrt(2) ⟺ v*v >= 2^(2b+1)。
    let vv = u64::from(v) * u64::from(v);
    if vv >= (1u64 << (2 * b + 1)) {
        b + 1
    } else {
        b
    }
}

/// 邊界函式：回傳從 data[0] 開始的下一個 chunk 長度。與 Go 版完全相同：
/// 前 `min` bytes 不參與 hash；未滿 `avg` 用較嚴的 mask、超過用較鬆的；
/// 硬邊界 `max`；尾巴不足 `min` 就整段。
fn boundary(data: &[u8], min: usize, avg: usize, max: usize, mask_small: u64, mask_large: u64) -> usize {
    if data.len() <= min {
        return data.len();
    }
    let limit = data.len().min(max);
    let normal = limit.min(avg);

    let mut fp: u64 = 0;
    let mut i = min;
    while i < normal {
        fp = (fp << 1).wrapping_add(GEAR_TABLE[data[i] as usize]);
        if fp & mask_small == 0 {
            return i + 1;
        }
        i += 1;
    }
    while i < limit {
        fp = (fp << 1).wrapping_add(GEAR_TABLE[data[i] as usize]);
        if fp & mask_large == 0 {
            return i + 1;
        }
        i += 1;
    }
    i
}

struct State<R> {
    params: ChunkerParams,
    mask_small: u64,
    mask_large: u64,
    reader: Option<R>,
    buf: Vec<u8>,
    /// buf 中有效資料的長度。
    len: usize,
    cursor: usize,
    eof: bool,
}

impl<R: Read> State<R> {
    /// 保證 cursor 之後至少有 max bytes，除非輸入已耗盡（與 Go 版的
    /// fill() 相同的不變量）。
    fn fill(&mut self) -> Result<(), ChunkerError> {
        let max = self.params.max as usize;
        let remaining = self.len - self.cursor;
        if remaining >= max {
            return Ok(());
        }
        self.buf.copy_within(self.cursor..self.len, 0);
        self.cursor = 0;
        self.len = remaining;
        if self.eof {
            return Ok(());
        }
        let reader = match self.reader.as_mut() {
            Some(r) => r,
            None => {
                self.eof = true;
                return Ok(());
            }
        };
        // 讀到滿或 EOF（對應 Go 的 io.ReadFull + UnexpectedEOF → eof）。
        while self.len < self.buf.len() {
            match reader.read(&mut self.buf[self.len..]) {
                Ok(0) => {
                    self.eof = true;
                    self.reader = None;
                    break;
                }
                Ok(n) => self.len += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(ChunkerError::Io(e)),
            }
        }
        Ok(())
    }

    fn next_chunk(&mut self) -> Result<Option<Vec<u8>>, ChunkerError> {
        self.fill()?;
        if self.len == 0 {
            return Ok(None);
        }
        let data = &self.buf[self.cursor..self.len];
        let length = boundary(
            data,
            self.params.min as usize,
            self.params.avg as usize,
            self.params.max as usize,
            self.mask_small,
            self.mask_large,
        );
        let chunk = data[..length].to_vec();
        self.cursor += length;
        Ok(Some(chunk))
    }
}

pub struct Chunks<R> {
    state: State<R>,
}

impl<R> Chunks<R> {
    /// 拿回內部緩衝供下一個輸入重用（內容殘留無妨，[`Chunker::chunks_with_buf`]
    /// 會整段重設）。呼叫後這個 iterator 不可再迭代。
    pub fn take_buf(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.state.buf)
    }
}

impl<R: Read> Iterator for Chunks<R> {
    type Item = Result<Vec<u8>, ChunkerError>;

    fn next(&mut self) -> Option<Self::Item> {
        match self.state.next_chunk() {
            Ok(Some(c)) => Some(Ok(c)),
            Ok(None) => None,
            Err(e) => Some(Err(e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gear_table_digest_is_pinned() {
        // 與 Go 端的 digest 測試相同（LE 串接 SHA-256）。改任何一個
        // entry 都會改變所有 chunk 邊界，這裡讓它被看見。
        let mut buf = Vec::with_capacity(256 * 8);
        for v in GEAR_TABLE {
            buf.extend_from_slice(&v.to_le_bytes());
        }
        let digest = {
            use sha2::Digest;
            let mut h = sha2::Sha256::new();
            h.update(&buf);
            h.finalize()
        };
        assert_eq!(
            hex::encode(digest),
            "a98fa4184eb747cd769328307242285f9d4b25afeb28f71ef3fe437a4c278e28"
        );
    }

    #[test]
    fn masks_for_default_params() {
        let c = Chunker::new(ChunkerParams::default());
        assert_eq!(c.mask_small, (1 << 23) - 1);
        assert_eq!(c.mask_large, (1 << 19) - 1);
    }
}
