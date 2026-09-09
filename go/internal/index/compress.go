package index

import (
	"sync"

	"github.com/klauspost/compress/zstd"
)

// One process-wide encoder/decoder pair; index blobs are written rarely
// and read once per repository open.
var (
	encOnce sync.Once
	enc     *zstd.Encoder
	decOnce sync.Once
	dec     *zstd.Decoder
)

func compressIndex(plain []byte) []byte {
	encOnce.Do(func() {
		var err error
		enc, err = zstd.NewWriter(nil, zstd.WithEncoderLevel(zstd.SpeedDefault))
		if err != nil {
			panic("index: build zstd encoder: " + err.Error())
		}
	})
	return enc.EncodeAll(plain, nil)
}

func decompressIndex(comp []byte) ([]byte, error) {
	decOnce.Do(func() {
		var err error
		dec, err = zstd.NewReader(nil,
			zstd.WithDecoderConcurrency(1),
			// A blob describes every pack in a repository; cap it at a
			// size no real blob approaches so a corrupted length field
			// cannot ask for gigabytes.
			zstd.WithDecoderMaxMemory(1<<30),
		)
		if err != nil {
			panic("index: build zstd decoder: " + err.Error())
		}
	})
	return dec.DecodeAll(comp, nil)
}
