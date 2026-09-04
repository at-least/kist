package repo

import (
	"context"
	"fmt"
	"os"
	"path/filepath"
	"testing"

	"github.com/at-least/kist/internal/crypto"
)

// BenchmarkBackupSmallFiles is the per-file cost: a thousand 1 KiB files,
// the shape of a home directory or a source tree, where the work is
// almost entirely overhead per file rather than bytes.
func BenchmarkBackupSmallFiles(b *testing.B) {
	source := b.TempDir()
	for i := range 1000 {
		dir := filepath.Join(source, fmt.Sprintf("d%02d", i/100))
		if err := os.MkdirAll(dir, 0o755); err != nil {
			b.Fatal(err)
		}
		if err := os.WriteFile(filepath.Join(dir, fmt.Sprintf("f%04d", i)), []byte(fmt.Sprintf("file %d\n", i)), 0o644); err != nil {
			b.Fatal(err)
		}
	}
	b.ReportAllocs()
	b.ResetTimer()
	for i := range b.N {
		b.StopTimer()
		dir := filepath.Join(b.TempDir(), "repo")
		be, err := createLocalAt(dir)
		if err != nil {
			b.Fatal(err)
		}
		opts := Options{
			Password: []byte(testPassword), ClientID: "00112233445566778899aabbccddeeff",
			StateDir: b.TempDir(), CacheDir: b.TempDir(), KDF: cheapKDF(),
			NonceSource: crypto.DeterministicReader(fmt.Sprint("bench", i)),
		}
		r, err := Init(context.Background(), be, opts)
		if err != nil {
			b.Fatal(err)
		}
		b.StartTimer()
		if _, _, err := r.Backup(context.Background(), []string{source}, BackupOptions{SpoolDir: b.TempDir()}); err != nil {
			b.Fatal(err)
		}
		b.StopTimer()
		if err := r.Close(); err != nil {
			b.Fatal(err)
		}
	}
}
