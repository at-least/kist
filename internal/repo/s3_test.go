package repo

import (
	"context"
	"errors"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/at-least/kist/internal/backend"
	"github.com/at-least/kist/internal/pack"
)

// The S3 tests in this package share one MinIO with the backend package's
// harness in spirit but not in process: each test binary starts its own
// container. Gated the same way.
const s3TestEnv = "KIST_S3_TEST"

const minioImage = "minio/minio:RELEASE.2025-09-07T16-13-09Z"

type minioServer struct {
	endpoint, accessKey, secretKey, bucket, container string
}

var (
	minioOnce sync.Once
	minioEnv  *minioServer
	minioErr  error
)

func startMinio(t *testing.T) *minioServer {
	t.Helper()
	if os.Getenv(s3TestEnv) != "1" {
		t.Skipf("set %s=1 to run the S3 tests against MinIO in Docker", s3TestEnv)
	}
	minioOnce.Do(func() { minioEnv, minioErr = launchMinio() })
	if minioErr != nil {
		t.Fatalf("start minio: %v", minioErr)
	}
	return minioEnv
}

func launchMinio() (*minioServer, error) {
	port, err := freePort()
	if err != nil {
		return nil, err
	}
	srv := &minioServer{
		endpoint:  fmt.Sprintf("http://127.0.0.1:%d", port),
		accessKey: "kisttest",
		secretKey: "kisttest-secret",
		bucket:    "kist-test",
	}
	out, err := exec.Command("docker", "run", "-d", "--rm",
		"-p", fmt.Sprintf("127.0.0.1:%d:9000", port),
		"-e", "MINIO_ROOT_USER="+srv.accessKey,
		"-e", "MINIO_ROOT_PASSWORD="+srv.secretKey,
		minioImage, "server", "/data").CombinedOutput()
	if err != nil {
		return nil, fmt.Errorf("docker run: %w: %s", err, out)
	}
	srv.container = strings.TrimSpace(string(out))

	// mc inside the container, aliased to itself: the admin API is what
	// creates the bucket and, later, the scoped users.
	deadline := time.Now().Add(60 * time.Second)
	for {
		err := srv.mc("alias", "set", "local", "http://127.0.0.1:9000", srv.accessKey, srv.secretKey)
		if err == nil {
			break
		}
		if time.Now().After(deadline) {
			srv.stop()
			return nil, fmt.Errorf("minio did not become ready: %w", err)
		}
		time.Sleep(250 * time.Millisecond)
	}
	if err := srv.mc("mb", "local/"+srv.bucket); err != nil {
		srv.stop()
		return nil, err
	}
	return srv, nil
}

// mc runs the MinIO client inside the container.
func (m *minioServer) mc(args ...string) error {
	cmd := exec.Command("docker", append([]string{"exec", m.container, "mc"}, args...)...)
	if out, err := cmd.CombinedOutput(); err != nil {
		return fmt.Errorf("mc %s: %w: %s", strings.Join(args, " "), err, out)
	}
	return nil
}

func (m *minioServer) stop() {
	if m.container == "" {
		return
	}
	if out, err := exec.Command("docker", "stop", "-t", "1", m.container).CombinedOutput(); err != nil {
		fmt.Fprintf(os.Stderr, "stop minio container %s: %v: %s\n", m.container, err, out)
	}
}

func TestMain(m *testing.M) {
	code := m.Run()
	if minioEnv != nil {
		minioEnv.stop()
	}
	os.Exit(code)
}

var s3PrefixCounter int

// s3Backend opens the shared bucket under a fresh prefix with the given
// credentials.
func s3Backend(t *testing.T, srv *minioServer, prefix, accessKey, secretKey string) backend.Backend {
	t.Helper()
	b, err := backend.OpenS3(context.Background(), backend.S3Config{
		Bucket: srv.bucket, Prefix: prefix, Endpoint: srv.endpoint, PathStyle: true,
		AccessKey: accessKey, SecretKey: secretKey, Region: "us-east-1",
	})
	if err != nil {
		t.Fatalf("open s3: %v", err)
	}
	return b
}

func freshPrefix() string {
	s3PrefixCounter++
	return fmt.Sprintf("r%d-%d", time.Now().UnixNano()%1_000_000, s3PrefixCounter)
}

// s3Options are repo options for a client on S3: real randomness, a
// unique client, and the cheap KDF.
func s3Options(t *testing.T, clientID string) Options {
	t.Helper()
	return Options{
		Password: []byte(testPassword),
		ClientID: clientID,
		StateDir: t.TempDir(),
		CacheDir: t.TempDir(),
		KDF:      cheapKDF(),
	}
}

// Two clients backing up the same data to one repository at the same
// moment, with no lock. Nothing may be lost, nothing may be corrupted,
// and a third client must afterwards see everything both of them wrote.
//
// Run several times: the interleaving is not under the test's control.
func TestS3ConcurrentBackups(t *testing.T) {
	srv := startMinio(t)
	ctx := context.Background()

	source := t.TempDir()
	writeTree(t, source, sampleFiles(t))

	for round := range 3 {
		t.Run(fmt.Sprintf("round %d", round), func(t *testing.T) {
			prefix := freshPrefix()
			root := s3Backend(t, srv, prefix, srv.accessKey, srv.secretKey)
			init, err := Init(ctx, root, s3Options(t, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"))
			if err != nil {
				t.Fatalf("init: %v", err)
			}
			if err := init.Close(); err != nil {
				t.Fatalf("close: %v", err)
			}

			clients := []string{"11111111111111111111111111111111", "22222222222222222222222222222222"}
			var wg sync.WaitGroup
			errs := make([]error, len(clients))
			for i, id := range clients {
				wg.Add(1)
				go func() {
					defer wg.Done()
					b := s3Backend(t, srv, prefix, srv.accessKey, srv.secretKey)
					r, err := Open(ctx, b, s3Options(t, id))
					if err != nil {
						errs[i] = fmt.Errorf("client %s open: %w", id, err)
						return
					}
					if _, _, err := r.Backup(ctx, []string{source}, BackupOptions{SpoolDir: t.TempDir()}); err != nil {
						errs[i] = fmt.Errorf("client %s backup: %w", id, err)
					}
					if err := r.Close(); err != nil && errs[i] == nil {
						errs[i] = fmt.Errorf("client %s close: %w", id, err)
					}
				}()
			}
			wg.Wait()
			for _, err := range errs {
				if err != nil {
					t.Fatal(err)
				}
			}

			// A third party, with no memory of either run.
			third, err := Open(ctx, s3Backend(t, srv, prefix, srv.accessKey, srv.secretKey), s3Options(t, "33333333333333333333333333333333"))
			if err != nil {
				t.Fatalf("third open: %v", err)
			}
			defer func() {
				if err := third.Close(); err != nil {
					t.Errorf("close: %v", err)
				}
			}()

			report, err := third.Check(ctx, CheckOptions{ReadData: true})
			if err != nil {
				t.Fatalf("check: %v", err)
			}
			if !report.OK() {
				t.Fatalf("check after concurrent backups found problems: %v", report.Problems)
			}
			if report.Snapshots != 2 {
				t.Errorf("repository holds %d snapshots, want 2", report.Snapshots)
			}

			handles, err := third.Snapshots(ctx, "")
			if err != nil {
				t.Fatalf("snapshots: %v", err)
			}
			for _, h := range handles {
				target := filepath.Join(t.TempDir(), "out")
				if _, err := third.Restore(ctx, h.Key, target, RestoreOptions{}); err != nil {
					t.Fatalf("restore %s: %v", h.Key, err)
				}
				compareTrees(t, source, filepath.Join(target, filepath.Base(source)))
			}

			// The sharp assertion: the third client's index merged both
			// clients' blobs, so it already has every chunk.
			again, _, err := third.Backup(ctx, []string{source}, BackupOptions{SpoolDir: t.TempDir()})
			if err != nil {
				t.Fatalf("third backup: %v", err)
			}
			if again.Stats.ChunksNew != 0 {
				t.Errorf("a third client stored %d new chunks after two identical backups; the index blobs did not merge", again.Stats.ChunksNew)
			}

			// Expected, and worth stating: both clients packed the same
			// plaintext under different nonces, so the packs differ and
			// are both stored. That is the price of no lock plus random
			// nonces; prune reclaims the unreferenced copy.
			packs := countKeys(t, third.Backend(), pack.Prefix)
			t.Logf("round %d: %d packs stored for two concurrent backups of one source", round, packs)
		})
	}
}

// backupPolicy is the least a backup client needs, scoped to one prefix.
// It is what docs/format.md §10 has to match, and the test is what says
// whether the table there is honest.
//
// The PutObject statement carries the s3:if-none-match condition: only a
// PutObject that sends If-None-Match is allowed at all. On a service that
// honours it, "backup credentials cannot overwrite a pack" stops being
// something the client chooses and becomes something the storage
// enforces.
func backupPolicy(bucket, prefix string, enforceConditional bool) string {
	res := func(p string) string { return fmt.Sprintf(`"arn:aws:s3:::%s/%s/%s"`, bucket, prefix, p) }
	condition := ""
	if enforceConditional {
		condition = `,
      "Condition": {"Null": {"s3:if-none-match": "false"}}`
	}
	return fmt.Sprintf(`{
  "Version": "2012-10-17",
  "Statement": [
    {
      "Sid": "ReadWhatBackupNeeds",
      "Effect": "Allow",
      "Action": ["s3:GetObject"],
      "Resource": [%s, %s]
    },
    {
      "Sid": "ListOnlyTheIndex",
      "Effect": "Allow",
      "Action": ["s3:ListBucket"],
      "Resource": ["arn:aws:s3:::%s"],
      "Condition": {"StringLike": {"s3:prefix": ["%s/indexes/*"]}}
    },
    {
      "Sid": "ConditionalWriteOnly",
      "Effect": "Allow",
      "Action": ["s3:PutObject"],
      "Resource": [%s, %s, %s, %s]%s
    }
  ]
}`, res("config"), res("indexes/*"), bucket, prefix, res("packs/*"), res("indexes/*"), res("trees/*"), res("snapshots/*"), condition)
}

// createScopedUser makes a MinIO user holding the backup policy, with
// the conditional-write clause if the service accepts it.
//
// enforced reports whether it did. MinIO RELEASE.2025-09-07 rejects the
// policy outright ("invalid condition key 's3:if-none-match'"), so on
// that service the clause is dropped and the caller is told.
func createScopedUser(t *testing.T, srv *minioServer, name, bucket, prefix string) (accessKey, secretKey string, enforced bool) {
	t.Helper()
	accessKey, secretKey = name, name+"-secret-key"

	if err := srv.mc("admin", "user", "add", "local", accessKey, secretKey); err != nil {
		t.Fatalf("add user: %v", err)
	}

	create := func(policy string) error {
		// The policy file has to exist inside the container for mc to read.
		write := exec.Command("docker", "exec", "-i", srv.container, "sh", "-c", "cat > /tmp/"+name+".json")
		write.Stdin = strings.NewReader(policy)
		if out, err := write.CombinedOutput(); err != nil {
			return fmt.Errorf("write policy: %w: %s", err, out)
		}
		return srv.mc("admin", "policy", "create", "local", name, "/tmp/"+name+".json")
	}

	enforced = true
	if err := create(backupPolicy(bucket, prefix, true)); err != nil {
		if !strings.Contains(err.Error(), "invalid condition key") {
			t.Fatalf("create policy: %v", err)
		}
		t.Logf("service rejects the s3:if-none-match condition key; falling back to a policy without it: %v", err)
		enforced = false
		if err := create(backupPolicy(bucket, prefix, false)); err != nil {
			t.Fatalf("create policy: %v", err)
		}
	}
	if err := srv.mc("admin", "policy", "attach", "local", name, "--user", accessKey); err != nil {
		t.Fatalf("attach policy: %v", err)
	}
	return accessKey, secretKey, enforced
}

// A backup client holding only backupPolicy must be able to back up, and
// must be unable to delete, list data prefixes, or overwrite.
func TestS3BackupPolicy(t *testing.T) {
	srv := startMinio(t)
	ctx := context.Background()
	prefix := freshPrefix()

	// The repository is created by an administrator; the backup client
	// never needs to.
	admin := s3Backend(t, srv, prefix, srv.accessKey, srv.secretKey)
	adminRepo, err := Init(ctx, admin, s3Options(t, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"))
	if err != nil {
		t.Fatalf("init: %v", err)
	}
	if err := adminRepo.Close(); err != nil {
		t.Fatalf("close: %v", err)
	}

	user := strings.ReplaceAll("backup-"+prefix, "_", "-")
	accessKey, secretKey, enforced := createScopedUser(t, srv, user, srv.bucket, prefix)
	limited := s3Backend(t, srv, prefix, accessKey, secretKey)

	source := t.TempDir()
	writeTree(t, source, sampleFiles(t))

	t.Run("backup succeeds", func(t *testing.T) {
		r, err := Open(ctx, limited, s3Options(t, "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"))
		if err != nil {
			t.Fatalf("open with the backup policy: %v", err)
		}
		defer func() {
			if err := r.Close(); err != nil {
				t.Errorf("close: %v", err)
			}
		}()

		snap, _, err := r.Backup(ctx, []string{source}, BackupOptions{SpoolDir: t.TempDir()})
		if err != nil {
			t.Fatalf("backup with the backup policy: %v", err)
		}
		if snap.Stats.PacksAdded == 0 {
			t.Fatal("backup wrote no packs")
		}
	})

	var packKey string
	if err := admin.List(ctx, pack.Prefix, func(fi backend.FileInfo) error { packKey = fi.Key; return nil }); err != nil {
		t.Fatalf("list packs as admin: %v", err)
	}
	if packKey == "" {
		t.Fatal("no pack to test against")
	}

	t.Run("cannot delete data", func(t *testing.T) {
		for _, key := range []string{packKey, "config"} {
			err := limited.Delete(ctx, key)
			if !errors.Is(err, backend.ErrDenied) {
				t.Errorf("delete %s: err = %v, want ErrDenied", key, err)
			}
		}
		if ok, err := backend.Exists(ctx, admin, packKey); err != nil || !ok {
			t.Errorf("pack is gone after a denied delete: %v, %v", ok, err)
		}
	})

	t.Run("cannot list data prefixes", func(t *testing.T) {
		for _, prefix := range []string{pack.Prefix, "trees/", "snapshots/", ""} {
			err := limited.List(ctx, prefix, func(backend.FileInfo) error { return nil })
			if !errors.Is(err, backend.ErrDenied) {
				t.Errorf("list %q: err = %v, want ErrDenied", prefix, err)
			}
		}
	})

	t.Run("cannot read data", func(t *testing.T) {
		if _, err := backend.GetAll(ctx, limited, packKey); !errors.Is(err, backend.ErrDenied) {
			t.Errorf("get %s: err = %v, want ErrDenied", packKey, err)
		}
	})

	original, err := backend.GetAll(ctx, admin, packKey)
	if err != nil {
		t.Fatalf("read pack as admin: %v", err)
	}

	t.Run("conditional put on an existing pack reports ErrExists", func(t *testing.T) {
		err := backend.PutBytesIfAbsent(ctx, limited, packKey, []byte("attacker"))
		if !errors.Is(err, backend.ErrExists) {
			t.Fatalf("PutIfAbsent on an existing pack: err = %v, want ErrExists", err)
		}
	})

	// The blind spot. Everything above is IAM doing what IAM has always
	// done. This is the one that decides whether "backup cannot overwrite
	// a pack" is enforced by the storage or merely observed by an honest
	// client.
	t.Run("unconditional put on an existing pack", func(t *testing.T) {
		err := limited.Put(ctx, packKey, strings.NewReader("attacker"), 8)
		after, readErr := backend.GetAll(ctx, admin, packKey)
		if readErr != nil {
			t.Fatalf("read pack as admin: %v", readErr)
		}
		overwritten := string(after) != string(original)

		if !enforced {
			// Restore the pack so the rest of the suite is not looking
			// at attacker bytes, then say exactly what happened.
			if overwritten {
				if err := admin.Put(ctx, packKey, strings.NewReader(string(original)), int64(len(original))); err != nil {
					t.Fatalf("restore pack: %v", err)
				}
			}
			t.Skipf("UNVERIFIED on this service: it rejects the s3:if-none-match condition key, so the policy cannot require conditional writes. "+
				"Without the clause an unconditional PutObject from backup credentials returned err=%v, overwrote=%v. "+
				"On AWS S3 the clause is accepted (docs: conditional-writes-enforce) and this is where the storage would refuse it.", err, overwritten)
		}

		if !errors.Is(err, backend.ErrDenied) {
			t.Errorf("unconditional put with the conditional-write clause in force: err = %v, want ErrDenied", err)
		}
		if overwritten {
			t.Error("the pack was overwritten despite the policy")
		}
	})
}
