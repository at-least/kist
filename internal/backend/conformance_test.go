package backend

import (
	"bytes"
	"context"
	"errors"
	"io"
	"sort"
	"testing"
)

// runConformance exercises the contract in the package documentation
// against any implementation. The local backend runs it here; S3 and SFTP
// will run the same suite when they land, so "conforms to Backend" keeps
// meaning one thing.
func runConformance(t *testing.T, newBackend func(t *testing.T) Backend) {
	ctx := context.Background()

	t.Run("put and get", func(t *testing.T) {
		b := newBackend(t)
		payload := []byte("the object body")

		if err := PutBytesIfAbsent(ctx, b, "packs/aa", payload); err != nil {
			t.Fatalf("put: %v", err)
		}
		got, err := GetAll(ctx, b, "packs/aa")
		if err != nil {
			t.Fatalf("get: %v", err)
		}
		if !bytes.Equal(got, payload) {
			t.Errorf("get = %q, want %q", got, payload)
		}
	})

	t.Run("get is ranged", func(t *testing.T) {
		b := newBackend(t)
		payload := []byte("0123456789")
		if err := PutBytesIfAbsent(ctx, b, "packs/bb", payload); err != nil {
			t.Fatalf("put: %v", err)
		}

		cases := []struct {
			off, length int64
			want        string
		}{
			{0, ReadToEnd, "0123456789"},
			{0, 4, "0123"},
			{6, ReadToEnd, "6789"},
			{6, 2, "67"},
			{10, ReadToEnd, ""},
			{3, 0, ""},
		}
		for _, tc := range cases {
			r, err := b.Get(ctx, "packs/bb", tc.off, tc.length)
			if err != nil {
				t.Fatalf("get(%d,%d): %v", tc.off, tc.length, err)
			}
			got, err := io.ReadAll(r)
			_ = r.Close()
			if err != nil {
				t.Fatalf("read(%d,%d): %v", tc.off, tc.length, err)
			}
			if string(got) != tc.want {
				t.Errorf("get(%d,%d) = %q, want %q", tc.off, tc.length, got, tc.want)
			}
		}
	})

	t.Run("missing objects report ErrNotFound", func(t *testing.T) {
		b := newBackend(t)

		if _, err := b.Get(ctx, "packs/absent", 0, ReadToEnd); !errors.Is(err, ErrNotFound) {
			t.Errorf("get: err = %v, want ErrNotFound", err)
		}
		if _, err := b.Stat(ctx, "packs/absent"); !errors.Is(err, ErrNotFound) {
			t.Errorf("stat: err = %v, want ErrNotFound", err)
		}
		if ok, err := Exists(ctx, b, "packs/absent"); err != nil || ok {
			t.Errorf("exists = %v, %v; want false, nil", ok, err)
		}
	})

	t.Run("PutIfAbsent will not overwrite", func(t *testing.T) {
		b := newBackend(t)
		if err := PutBytesIfAbsent(ctx, b, "trees/cc", []byte("first")); err != nil {
			t.Fatalf("first put: %v", err)
		}

		err := PutBytesIfAbsent(ctx, b, "trees/cc", []byte("second"))
		if !errors.Is(err, ErrExists) {
			t.Fatalf("second put: err = %v, want ErrExists", err)
		}
		got, err := GetAll(ctx, b, "trees/cc")
		if err != nil {
			t.Fatalf("get: %v", err)
		}
		if string(got) != "first" {
			t.Errorf("object = %q, want the original %q", got, "first")
		}
	})

	t.Run("Put overwrites", func(t *testing.T) {
		b := newBackend(t)
		if err := b.Put(ctx, "config", bytes.NewReader([]byte("v1")), 2); err != nil {
			t.Fatalf("first put: %v", err)
		}
		if err := b.Put(ctx, "config", bytes.NewReader([]byte("v2-longer")), 9); err != nil {
			t.Fatalf("second put: %v", err)
		}

		got, err := GetAll(ctx, b, "config")
		if err != nil {
			t.Fatalf("get: %v", err)
		}
		if string(got) != "v2-longer" {
			t.Errorf("object = %q, want %q", got, "v2-longer")
		}
	})

	t.Run("PutIfAbsent streams", func(t *testing.T) {
		b := newBackend(t)
		payload := bytes.Repeat([]byte("streamed "), 4096)

		if err := b.PutIfAbsent(ctx, "packs/streamed", bytes.NewReader(payload), int64(len(payload))); err != nil {
			t.Fatalf("put: %v", err)
		}
		got, err := GetAll(ctx, b, "packs/streamed")
		if err != nil {
			t.Fatalf("get: %v", err)
		}
		if !bytes.Equal(got, payload) {
			t.Errorf("object differs from what was streamed in")
		}
	})

	t.Run("PutIfAbsent rejects a short reader", func(t *testing.T) {
		b := newBackend(t)

		err := b.PutIfAbsent(ctx, "packs/short", bytes.NewReader([]byte("ab")), 100)
		if err == nil {
			t.Fatal("put with a wrong size: got nil error")
		}
		if ok, err := Exists(ctx, b, "packs/short"); err != nil || ok {
			t.Errorf("object exists after a failed put: %v, %v", ok, err)
		}
	})

	t.Run("Put rejects a short reader", func(t *testing.T) {
		b := newBackend(t)

		err := b.Put(ctx, "config", bytes.NewReader([]byte("ab")), 100)
		if err == nil {
			t.Fatal("put with a wrong size: got nil error")
		}
		if ok, err := Exists(ctx, b, "config"); err != nil || ok {
			t.Errorf("object exists after a failed put: %v, %v", ok, err)
		}
	})

	t.Run("stat reports size", func(t *testing.T) {
		b := newBackend(t)
		if err := PutBytesIfAbsent(ctx, b, "packs/dd", []byte("12345")); err != nil {
			t.Fatalf("put: %v", err)
		}

		info, err := b.Stat(ctx, "packs/dd")
		if err != nil {
			t.Fatalf("stat: %v", err)
		}
		if info.Key != "packs/dd" || info.Size != 5 {
			t.Errorf("stat = %+v, want {packs/dd 5}", info)
		}
	})

	t.Run("list by prefix", func(t *testing.T) {
		b := newBackend(t)
		for _, key := range []string{"packs/a1", "packs/a2", "indexes/i1", "snapshots/client1/t1", "config"} {
			if err := PutBytesIfAbsent(ctx, b, key, []byte(key)); err != nil {
				t.Fatalf("put %s: %v", key, err)
			}
		}

		cases := map[string][]string{
			"":                  {"config", "indexes/i1", "packs/a1", "packs/a2", "snapshots/client1/t1"},
			"packs/":            {"packs/a1", "packs/a2"},
			"snapshots/":        {"snapshots/client1/t1"},
			"snapshots/client1": {"snapshots/client1/t1"},
			"indexes/":          {"indexes/i1"},
			"absent/":           nil,
			"packs/a1":          {"packs/a1"},
		}
		for prefix, want := range cases {
			var got []string
			err := b.List(ctx, prefix, func(fi FileInfo) error {
				if fi.Size != int64(len(fi.Key)) {
					t.Errorf("list %q: %s has size %d, want %d", prefix, fi.Key, fi.Size, len(fi.Key))
				}
				got = append(got, fi.Key)
				return nil
			})
			if err != nil {
				t.Fatalf("list %q: %v", prefix, err)
			}
			sort.Strings(got)
			if len(got) != len(want) {
				t.Errorf("list %q = %v, want %v", prefix, got, want)
				continue
			}
			for i := range got {
				if got[i] != want[i] {
					t.Errorf("list %q = %v, want %v", prefix, got, want)
					break
				}
			}
		}
	})

	t.Run("list stops on error", func(t *testing.T) {
		b := newBackend(t)
		for _, key := range []string{"packs/a1", "packs/a2", "packs/a3"} {
			if err := PutBytesIfAbsent(ctx, b, key, []byte("x")); err != nil {
				t.Fatalf("put: %v", err)
			}
		}

		sentinel := errors.New("stop here")
		seen := 0
		err := b.List(ctx, "packs/", func(FileInfo) error {
			seen++
			return sentinel
		})
		if !errors.Is(err, sentinel) {
			t.Errorf("list: err = %v, want the callback error", err)
		}
		if seen != 1 {
			t.Errorf("callback ran %d times after returning an error, want 1", seen)
		}
	})

	t.Run("delete", func(t *testing.T) {
		b := newBackend(t)
		if err := PutBytesIfAbsent(ctx, b, "gc/ee", []byte("marker")); err != nil {
			t.Fatalf("put: %v", err)
		}
		if err := b.Delete(ctx, "gc/ee"); err != nil {
			t.Fatalf("delete: %v", err)
		}
		if ok, err := Exists(ctx, b, "gc/ee"); err != nil || ok {
			t.Errorf("exists after delete = %v, %v", ok, err)
		}

		// Deleting what is not there is the outcome the caller asked for.
		if err := b.Delete(ctx, "gc/ee"); err != nil {
			t.Errorf("second delete: %v", err)
		}

		// And the key is free again.
		if err := PutBytesIfAbsent(ctx, b, "gc/ee", []byte("again")); err != nil {
			t.Errorf("put after delete: %v", err)
		}
	})

	t.Run("invalid keys are rejected", func(t *testing.T) {
		b := newBackend(t)
		bad := []string{"", "/leading", "trailing/", "double//slash", "../escape", "packs/../../etc", "UPPER", "has space", "snapshots/a:b", ".hidden", "packs/.tmp-1"}

		for _, key := range bad {
			t.Run(key, func(t *testing.T) {
				if err := PutBytesIfAbsent(ctx, b, key, []byte("x")); !errors.Is(err, ErrInvalidKey) {
					t.Errorf("PutIfAbsent: err = %v, want ErrInvalidKey", err)
				}
				if _, err := b.Get(ctx, key, 0, ReadToEnd); !errors.Is(err, ErrInvalidKey) {
					t.Errorf("Get: err = %v, want ErrInvalidKey", err)
				}
				if _, err := b.Stat(ctx, key); !errors.Is(err, ErrInvalidKey) {
					t.Errorf("Stat: err = %v, want ErrInvalidKey", err)
				}
				if err := b.Delete(ctx, key); !errors.Is(err, ErrInvalidKey) {
					t.Errorf("Delete: err = %v, want ErrInvalidKey", err)
				}
			})
		}
	})

	t.Run("empty object", func(t *testing.T) {
		b := newBackend(t)
		if err := PutBytesIfAbsent(ctx, b, "packs/empty", nil); err != nil {
			t.Fatalf("put: %v", err)
		}
		got, err := GetAll(ctx, b, "packs/empty")
		if err != nil {
			t.Fatalf("get: %v", err)
		}
		if len(got) != 0 {
			t.Errorf("get = %q, want empty", got)
		}
		if ok, err := Exists(ctx, b, "packs/empty"); err != nil || !ok {
			t.Errorf("exists = %v, %v; want true, nil", ok, err)
		}
	})
}
