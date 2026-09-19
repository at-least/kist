package pack

import (
	"fmt"
	"sync"

	"github.com/klauspost/compress/zstd"
)

// Compression is decided per chunk by compressing it and comparing, not
// by sampling or by guessing from a file extension. Sampling gets
// structured-but-noisy data wrong in both directions, and the cost of
// being wrong is paid on every future restore.
//
// A chunk keeps its compressed form only if compression saved more than
// 1/16 of it. Below that the decompression cost on every read is not
// worth the space, and a chunk that "compresses" by 1% is usually one
// that will grow under a future encoder.
const compressionThresholdDivisor = 16

var (
	encoderOnce sync.Once
	encoder     *zstd.Encoder
	encoderErr  error

	// One decoder per decode cap: the cap is the repository's chunker.max,
	// a per-repo invariant, so a process touching repositories with
	// different maxima holds one decoder each. zstd's stateless
	// EncodeAll/DecodeAll are safe for concurrent use, so they serve the
	// whole process without further locking.
	decodersMu sync.Mutex
	decoders   = map[uint64]*zstd.Decoder{}
)

// zstd's stateless EncodeAll/DecodeAll are safe for concurrent use, so
// one encoder serves the whole process.
func getEncoder() (*zstd.Encoder, error) {
	encoderOnce.Do(func() {
		encoder, encoderErr = zstd.NewWriter(nil,
			// Level 3 equivalent: the point where zstd still costs almost
			// nothing per megabyte. A backup is bounded by disk and
			// network, not by the compressor.
			zstd.WithEncoderLevel(zstd.SpeedDefault),
			zstd.WithEncoderConcurrency(1),
		)
		if encoderErr != nil {
			encoderErr = fmt.Errorf("create zstd encoder: %w", encoderErr)
		}
	})
	return encoder, encoderErr
}

func getDecoder(limit uint64) (*zstd.Decoder, error) {
	decodersMu.Lock()
	defer decodersMu.Unlock()
	if dec, ok := decoders[limit]; ok {
		return dec, nil
	}
	// A chunk is never larger than the repository's chunker maximum, so a
	// frame claiming more is a decompression bomb, not a chunk.
	dec, err := zstd.NewReader(nil,
		zstd.WithDecoderMaxMemory(limit),
		zstd.WithDecoderConcurrency(1),
	)
	if err != nil {
		return nil, fmt.Errorf("create zstd decoder: %w", err)
	}
	decoders[limit] = dec
	return dec, nil
}

// compress returns the payload to store and the algorithm byte that
// describes it.
func compress(plaintext []byte) (byte, []byte, error) {
	enc, err := getEncoder()
	if err != nil {
		return 0, nil, err
	}

	candidate := enc.EncodeAll(plaintext, nil)
	if len(candidate) < len(plaintext)-len(plaintext)/compressionThresholdDivisor {
		return algorithmZstd, candidate, nil
	}
	return algorithmRaw, plaintext, nil
}

// decompress reverses compress. limit is the repository's chunker maximum:
// the decoder refuses any frame that would decode to more, so a corrupted
// length field cannot ask for unbounded memory.
func decompress(algorithm byte, payload []byte, limit uint64) ([]byte, error) {
	switch algorithm {
	case algorithmRaw:
		return payload, nil
	case algorithmZstd:
		dec, err := getDecoder(limit)
		if err != nil {
			return nil, err
		}
		plaintext, err := dec.DecodeAll(payload, nil)
		if err != nil {
			return nil, fmt.Errorf("decompress chunk: %w", err)
		}
		return plaintext, nil
	default:
		return nil, fmt.Errorf("%w: unknown chunk encoding %d", ErrCorrupt, algorithm)
	}
}
