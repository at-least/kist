package repo

import (
	"context"
	"crypto/sha256"
	"encoding/binary"
	"fmt"
	"io"
	"io/fs"
	"os"
	"path/filepath"
	"runtime"
	"strconv"
	"sync"
	"testing"
	"time"

	"github.com/at-least/kist/internal/crypto"
)

// The M1 acceptance criteria, run against a dataset large enough for the
// numbers to mean something:
//
//   - back up and restore, byte for byte
//   - a second backup of unchanged data writes almost nothing
//   - check catches a deliberately damaged pack
//
// It is off by default because it writes tens of gigabytes. Enable it
// with KIST_ACCEPTANCE=1, and scale it with KIST_ACCEPTANCE_FILES and
// KIST_ACCEPTANCE_BYTES when the machine cannot hold the full set.
func TestAcceptance(t *testing.T) {
	if os.Getenv("KIST_ACCEPTANCE") != "1" {
		t.Skip("set KIST_ACCEPTANCE=1 to run the full-scale acceptance test")
	}

	files := envInt(t, "KIST_ACCEPTANCE_FILES", 100_000)
	total := int64(envInt(t, "KIST_ACCEPTANCE_BYTES", 10<<30))
	workDir := os.Getenv("KIST_ACCEPTANCE_DIR")
	if workDir == "" {
		workDir = t.TempDir()
	} else {
		var err error
		if workDir, err = os.MkdirTemp(workDir, "kist-acceptance-*"); err != nil {
			t.Fatalf("create work directory: %v", err)
		}
		t.Cleanup(func() {
			if err := os.RemoveAll(workDir); err != nil {
				t.Errorf("clean up %s: %v", workDir, err)
			}
		})
	}

	ctx := context.Background()
	source := filepath.Join(workDir, "source")
	repoDir := filepath.Join(workDir, "repo")
	target := filepath.Join(workDir, "restored")

	t.Logf("generating %d files totalling %s in %s", files, human(total), source)
	start := time.Now()
	written := generateDataset(t, source, files, total)
	t.Logf("generated %s across %d files in %v", human(written), files, time.Since(start).Round(time.Second))

	r, _ := initRepoAt(t, repoDir, "acceptance")

	// --- first backup -------------------------------------------------
	// PLAN's M5 bar is peak memory under 1 GiB for a million files. The
	// sampler reads MemStats every 20 ms while the backup runs and keeps
	// the highest HeapAlloc (live objects) and Sys (what the process holds
	// from the OS, which with the default GOGC is up to twice the live
	// heap). The bar is judged on Sys: that is what the machine sees.
	stopSampling, peak := samplePeakHeap()
	start = time.Now()
	summary, err := r.Backup(ctx, []string{source}, BackupOptions{
		SpoolDir: workDir,
		Warnf:    func(format string, args ...any) { t.Logf("warning: "+format, args...) },
	})
	if err != nil {
		t.Fatalf("first backup: %v", err)
	}
	first := summary
	handle := summary.Handle
	elapsed := time.Since(start)
	t.Logf("first backup: %d files, %s read, %s stored in %d packs, %d new chunks, in %v (%s/s)",
		first.Snapshot.Stats.Files, human(int64(first.Snapshot.Stats.Bytes)), human(int64(first.Report.BytesStored)),
		first.Report.PacksNew, first.Report.ChunksNew, elapsed.Round(time.Second),
		human(int64(float64(first.Snapshot.Stats.Bytes)/elapsed.Seconds())))
	stopSampling()
	heapPeak, sysPeak := peak()
	t.Logf("peak during backup (20 ms samples): heap in use %s, process Sys %s; heap in use after: %s",
		human(int64(heapPeak)), human(int64(sysPeak)), human(int64(heapInUse())))
	if limit := int64(envInt(t, "KIST_ACCEPTANCE_HEAP_LIMIT", 0)); limit > 0 && int64(sysPeak) > limit {
		t.Errorf("peak Sys %s exceeds the limit %s", human(int64(sysPeak)), human(limit))
	}

	if first.Snapshot.Stats.Files != uint64(files) {
		t.Errorf("backed up %d files, want %d", first.Snapshot.Stats.Files, files)
	}

	// --- second backup, nothing changed -------------------------------
	second := reopenAt(t, repoDir, "acceptance-2")
	start = time.Now()
	again, err := second.Backup(ctx, []string{source}, BackupOptions{SpoolDir: workDir})
	if err != nil {
		t.Fatalf("second backup: %v", err)
	}
	t.Logf("second backup: %d new chunks, %d new packs, in %v", again.Report.ChunksNew, again.Report.PacksNew, time.Since(start).Round(time.Second))

	if again.Report.ChunksNew != 0 {
		t.Errorf("second backup of unchanged data stored %d new chunks, want 0", again.Report.ChunksNew)
	}
	if again.Report.PacksNew != 0 {
		t.Errorf("second backup of unchanged data wrote %d packs, want 0", again.Report.PacksNew)
	}

	// --- restore ------------------------------------------------------
	start = time.Now()
	stats, err := second.Restore(ctx, handle.Key, target, RestoreOptions{})
	if err != nil {
		t.Fatalf("restore: %v", err)
	}
	t.Logf("restore: %d files, %s, in %v", stats.Files, human(int64(stats.Bytes)), time.Since(start).Round(time.Second))

	start = time.Now()
	compareTreesByHash(t, source, filepath.Join(target, source))
	t.Logf("byte-for-byte comparison in %v", time.Since(start).Round(time.Second))

	// --- check --------------------------------------------------------
	start = time.Now()
	report, err := second.Check(ctx, CheckOptions{ReadData: true})
	if err != nil {
		t.Fatalf("check: %v", err)
	}
	if !report.OK() {
		t.Fatalf("check --read-data on a healthy repository found problems: %v", report.Problems)
	}
	t.Logf("check --read-data: %d snapshots, %d trees, %d chunks, %d packs, in %v",
		report.Snapshots, report.Trees, report.Chunks, report.Packs, time.Since(start).Round(time.Second))

	// --- deliberate damage --------------------------------------------
	damagePack(t, second, repoDir)
	damaged := reopenAt(t, repoDir, "acceptance-3")

	deep, err := damaged.Check(ctx, CheckOptions{ReadData: true})
	if err != nil {
		t.Fatalf("check after damage: %v", err)
	}
	if deep.OK() {
		t.Fatal("check --read-data did not notice a deliberately damaged pack")
	}
	t.Logf("check --read-data found the damage: %s", deep.Problems[0])
}

// damagePack flips one byte inside the chunk data of one pack.
func damagePack(t *testing.T, r *Repository, repoDir string) {
	t.Helper()

	key := anyKey(t, r, "packs/")
	path := filepath.Join(repoDir, filepath.FromSlash(key))

	f, err := os.OpenFile(path, os.O_RDWR, 0)
	if err != nil {
		t.Fatalf("open pack: %v", err)
	}
	defer func() { _ = f.Close() }()

	// Past the first chunk's nonce, well away from the trailer.
	var b [1]byte
	if _, err := f.ReadAt(b[:], crypto.NonceSize+7); err != nil {
		t.Fatalf("read pack: %v", err)
	}
	b[0] ^= 0x01
	if _, err := f.WriteAt(b[:], crypto.NonceSize+7); err != nil {
		t.Fatalf("write pack: %v", err)
	}
	if err := f.Close(); err != nil {
		t.Fatalf("close pack: %v", err)
	}
	t.Logf("flipped one byte inside %s", key)
}

// generateDataset writes a nested tree of files whose contents are a mix
// of incompressible and repetitive data, so that both compression paths
// and the deduplicator are exercised. It returns the bytes written.
func generateDataset(t *testing.T, root string, files int, total int64) int64 {
	t.Helper()

	const perDir = 100
	average := total / int64(files)

	var written int64
	buf := make([]byte, 1<<20)

	for i := range files {
		dir := filepath.Join(root, fmt.Sprintf("d%03d", i/(perDir*perDir)), fmt.Sprintf("s%03d", (i/perDir)%perDir))
		if i%perDir == 0 {
			if err := os.MkdirAll(dir, 0o755); err != nil {
				t.Fatalf("mkdir: %v", err)
			}
		}

		// A long tail: most files are small, a few are large, which is
		// what a real filesystem looks like and what stresses both the
		// tree writer and the chunker.
		size := average / 2
		if i%1000 == 0 {
			size = average * 200
		} else if i%10 == 0 {
			size = average * 3
		}

		path := filepath.Join(dir, fmt.Sprintf("f%06d.bin", i))
		n, err := writeFile(path, size, i, buf)
		if err != nil {
			t.Fatalf("write %s: %v", path, err)
		}
		written += n
	}
	return written
}

func writeFile(path string, size int64, seed int, buf []byte) (int64, error) {
	f, err := os.Create(path) //nolint:gosec // path is inside the test's own directory
	if err != nil {
		return 0, err
	}
	defer func() { _ = f.Close() }()

	var src io.Reader
	if seed%3 == 0 {
		// Repetitive: compresses, and deduplicates against its siblings.
		src = &repeatReader{pattern: []byte(fmt.Sprintf("kist acceptance line %d\n", seed%7))}
	} else {
		src = crypto.DeterministicReader(strconv.Itoa(seed))
	}

	n, err := io.CopyBuffer(f, io.LimitReader(src, size), buf)
	if err != nil {
		return n, err
	}
	return n, f.Close()
}

type repeatReader struct {
	pattern []byte
	at      int
}

func (r *repeatReader) Read(p []byte) (int, error) {
	for i := range p {
		p[i] = r.pattern[r.at]
		r.at = (r.at + 1) % len(r.pattern)
	}
	return len(p), nil
}

// compareTreesByHash compares two trees by streaming hash rather than by
// loading files, so that a 2 GiB file does not need 4 GiB of memory to
// compare.
func compareTreesByHash(t *testing.T, want, got string) {
	t.Helper()

	digests := func(root string) map[string]string {
		out := map[string]string{}
		err := filepath.WalkDir(root, func(path string, d fs.DirEntry, err error) error {
			if err != nil {
				return err
			}
			rel, err := filepath.Rel(root, path)
			if err != nil {
				return err
			}
			if rel == "." {
				return nil
			}
			if d.IsDir() {
				out[filepath.ToSlash(rel)] = "dir"
				return nil
			}

			f, err := os.Open(path) //nolint:gosec // path comes from walking the test's own tree
			if err != nil {
				return err
			}
			defer func() { _ = f.Close() }()

			h := sha256.New()
			if _, err := io.Copy(h, f); err != nil {
				return err
			}
			info, err := d.Info()
			if err != nil {
				return err
			}
			var meta [12]byte
			binary.BigEndian.PutUint64(meta[:8], uint64(info.Size()))
			binary.BigEndian.PutUint32(meta[8:], uint32(info.Mode().Perm()))
			out[filepath.ToSlash(rel)] = fmt.Sprintf("%x %x", h.Sum(nil), meta)
			return nil
		})
		if err != nil {
			t.Fatalf("walk %s: %v", root, err)
		}
		return out
	}

	wantDigests, gotDigests := digests(want), digests(got)
	if len(wantDigests) != len(gotDigests) {
		t.Errorf("restored tree has %d entries, source has %d", len(gotDigests), len(wantDigests))
	}

	differing := 0
	for name, wantDigest := range wantDigests {
		gotDigest, ok := gotDigests[name]
		if !ok {
			if differing < 10 {
				t.Errorf("restored tree is missing %s", name)
			}
			differing++
			continue
		}
		if gotDigest != wantDigest {
			if differing < 10 {
				t.Errorf("%s differs: %s vs %s", name, gotDigest, wantDigest)
			}
			differing++
		}
	}
	if differing > 10 {
		t.Errorf("... and %d more differences", differing-10)
	}
}

func initRepoAt(t *testing.T, dir, seed string) (*Repository, string) {
	t.Helper()

	b, err := createLocalAt(dir)
	if err != nil {
		t.Fatalf("create backend: %v", err)
	}
	opts := testOptions(t, seed)
	opts.NonceSource = nil // real randomness at real scale

	r, err := Init(context.Background(), b, opts)
	if err != nil {
		t.Fatalf("init: %v", err)
	}
	t.Cleanup(func() {
		if err := r.Close(); err != nil {
			t.Errorf("close: %v", err)
		}
	})
	return r, dir
}

func reopenAt(t *testing.T, dir, seed string) *Repository {
	t.Helper()

	b, err := openLocalAt(dir)
	if err != nil {
		t.Fatalf("open backend: %v", err)
	}
	opts := testOptions(t, seed)
	opts.NonceSource = nil

	r, err := Open(context.Background(), b, opts)
	if err != nil {
		t.Fatalf("open: %v", err)
	}
	t.Cleanup(func() {
		if err := r.Close(); err != nil {
			t.Errorf("close: %v", err)
		}
	})
	return r
}

func envInt(t *testing.T, name string, fallback int) int {
	t.Helper()

	raw := os.Getenv(name)
	if raw == "" {
		return fallback
	}
	n, err := strconv.Atoi(raw)
	if err != nil {
		t.Fatalf("%s=%q: %v", name, raw, err)
	}
	return n
}

func human(n int64) string {
	const unit = 1024
	if n < unit {
		return fmt.Sprintf("%d B", n)
	}
	value, exp := float64(n), 0
	for value >= unit && exp < 5 {
		value /= unit
		exp++
	}
	return fmt.Sprintf("%.1f %ciB", value, "KMGTP"[exp-1])
}

// heapInUse is a single sample taken after the backup returns, not a
// peak. Measuring the peak needs continuous sampling, which is the memory
// profile M5 calls for; this is here only to catch an order-of-magnitude
// regression.
// samplePeakHeap watches MemStats until stopped and reports the highest
// HeapAlloc and Sys seen.
func samplePeakHeap() (stop func(), peak func() (heap, sys uint64)) {
	var (
		mu       sync.Mutex
		highHeap uint64
		highSys  uint64
		done     = make(chan struct{})
		once     sync.Once
	)
	sample := func() {
		var m runtime.MemStats
		runtime.ReadMemStats(&m)
		mu.Lock()
		highHeap = max(highHeap, m.HeapAlloc)
		highSys = max(highSys, m.Sys)
		mu.Unlock()
	}
	go func() {
		t := time.NewTicker(20 * time.Millisecond)
		defer t.Stop()
		for {
			select {
			case <-done:
				return
			case <-t.C:
				sample()
			}
		}
	}()
	return func() { once.Do(func() { close(done); sample() }) }, func() (uint64, uint64) {
		mu.Lock()
		defer mu.Unlock()
		return highHeap, highSys
	}
}

func heapInUse() uint64 {
	var m runtime.MemStats
	runtime.ReadMemStats(&m)
	return m.HeapAlloc
}
