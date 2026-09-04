package backend

import (
	"bytes"
	"context"
	"errors"
	"fmt"
	"net"
	"os"
	"os/exec"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/aws/aws-sdk-go-v2/aws"
	"github.com/aws/aws-sdk-go-v2/service/s3"
)

// S3TestEnv enables the S3 tests. They need a running MinIO, which the
// suite starts in Docker; `make test` stays offline unless asked.
const S3TestEnv = "KIST_S3_TEST"

// minioImage is pinned so that a behaviour change upstream shows up as a
// deliberate bump, not as a mysteriously red CI.
const minioImage = "minio/minio:RELEASE.2025-09-07T16-13-09Z"

// minio is the one MinIO container shared by every S3 test in the
// package; each test gets its own prefix under one bucket.
var (
	minioOnce sync.Once
	minioEnv  *minioServer
	minioErr  error
)

type minioServer struct {
	endpoint  string
	accessKey string
	secretKey string
	bucket    string
	container string
}

// startMinio launches MinIO in Docker and waits for it to answer.
func startMinio(t *testing.T) *minioServer {
	t.Helper()

	if os.Getenv(S3TestEnv) != "1" {
		t.Skipf("set %s=1 to run the S3 tests against MinIO in Docker", S3TestEnv)
	}

	minioOnce.Do(func() {
		minioEnv, minioErr = launchMinio()
	})
	if minioErr != nil {
		t.Fatalf("start minio: %v", minioErr)
	}
	return minioEnv
}

func launchMinio() (*minioServer, error) {
	if _, err := exec.LookPath("docker"); err != nil {
		return nil, fmt.Errorf("docker is not on PATH: %w", err)
	}

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

	run := exec.Command("docker", "run", "-d", "--rm",
		"-p", fmt.Sprintf("127.0.0.1:%d:9000", port),
		"-e", "MINIO_ROOT_USER="+srv.accessKey,
		"-e", "MINIO_ROOT_PASSWORD="+srv.secretKey,
		minioImage, "server", "/data")
	out, err := run.CombinedOutput()
	if err != nil {
		return nil, fmt.Errorf("docker run: %w: %s", err, out)
	}
	srv.container = strings.TrimSpace(string(out))

	ctx, cancel := context.WithTimeout(context.Background(), 60*time.Second)
	defer cancel()

	b, err := OpenS3(ctx, S3Config{
		Bucket: srv.bucket, Endpoint: srv.endpoint, PathStyle: true,
		AccessKey: srv.accessKey, SecretKey: srv.secretKey, Region: "us-east-1",
	})
	if err != nil {
		srv.stop()
		return nil, err
	}

	// Poll until the bucket can be created: the container answers on its
	// port a moment before it is ready to serve.
	for {
		_, err = b.client.CreateBucket(ctx, &s3.CreateBucketInput{Bucket: aws.String(srv.bucket)})
		if err == nil {
			break
		}
		if ctx.Err() != nil {
			srv.stop()
			return nil, fmt.Errorf("minio did not become ready: last error: %w", err)
		}
		time.Sleep(250 * time.Millisecond)
	}
	return srv, nil
}

func (m *minioServer) stop() {
	if m.container == "" {
		return
	}
	if out, err := exec.Command("docker", "stop", "-t", "1", m.container).CombinedOutput(); err != nil {
		// Nothing above can act on it; say so where a person will look.
		fmt.Fprintf(os.Stderr, "stop minio container %s: %v: %s\n", m.container, err, out)
	}
}

func freePort() (int, error) {
	l, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		return 0, fmt.Errorf("find a free port: %w", err)
	}
	addr, ok := l.Addr().(*net.TCPAddr)
	if err := l.Close(); err != nil {
		return 0, fmt.Errorf("release the probe port: %w", err)
	}
	if !ok {
		return 0, fmt.Errorf("listener address is %T, not a TCP address", l.Addr())
	}
	return addr.Port, nil
}

func TestMain(m *testing.M) {
	code := m.Run()
	if minioEnv != nil {
		minioEnv.stop()
	}
	os.Exit(code)
}

var prefixCounter int

// newTestS3 returns a backend over a fresh prefix in the shared bucket.
func newTestS3(t *testing.T) Backend {
	t.Helper()
	srv := startMinio(t)

	prefixCounter++
	prefix := fmt.Sprintf("t%d-%d", time.Now().UnixNano()%1_000_000, prefixCounter)

	b, err := OpenS3(context.Background(), S3Config{
		Bucket: srv.bucket, Prefix: prefix, Endpoint: srv.endpoint, PathStyle: true,
		AccessKey: srv.accessKey, SecretKey: srv.secretKey, Region: "us-east-1",
	})
	if err != nil {
		t.Fatalf("open s3: %v", err)
	}
	t.Cleanup(func() {
		if err := b.Close(); err != nil {
			t.Errorf("close: %v", err)
		}
	})
	return b
}

func TestS3Conformance(t *testing.T) {
	runConformance(t, newTestS3)
}

// The conditional write is the whole reason S3 was chosen: two clients
// racing on one key must produce exactly one winner, with the loser told
// the bytes are already there.
func TestS3PutIfAbsentHasOneWinner(t *testing.T) {
	ctx := context.Background()
	b := newTestS3(t)

	const writers = 8
	var (
		wg      sync.WaitGroup
		mu      sync.Mutex
		wins    int
		existed int
	)
	start := make(chan struct{})
	for i := range writers {
		wg.Add(1)
		go func() {
			defer wg.Done()
			<-start
			payload := []byte("shared content")
			err := b.PutIfAbsent(ctx, "packs/contended", bytes.NewReader(payload), int64(len(payload)))
			mu.Lock()
			defer mu.Unlock()
			switch {
			case err == nil:
				wins++
			case errors.Is(err, ErrExists):
				existed++
			default:
				t.Errorf("writer %d: %v", i, err)
			}
		}()
	}
	close(start)
	wg.Wait()

	if wins != 1 {
		t.Errorf("%d writers succeeded, want exactly 1", wins)
	}
	if existed != writers-1 {
		t.Errorf("%d writers saw ErrExists, want %d", existed, writers-1)
	}
}

func TestS3PutIfAbsentNeedsASeekableBody(t *testing.T) {
	b := newTestS3(t)

	err := b.PutIfAbsent(context.Background(), "packs/x", strings.NewReader("x"), 1)
	if err != nil {
		// strings.Reader is seekable; this must succeed.
		t.Fatalf("seekable body: %v", err)
	}
	err = b.PutIfAbsent(context.Background(), "packs/y", nonSeeking{strings.NewReader("y")}, 1)
	if err == nil || !strings.Contains(err.Error(), "seekable") {
		t.Fatalf("non-seekable body: err = %v, want a seekable-body error", err)
	}
}

type nonSeeking struct{ r *strings.Reader }

func (n nonSeeking) Read(p []byte) (int, error) { return n.r.Read(p) }

func TestParseS3Location(t *testing.T) {
	for in, want := range map[string]S3Config{
		"s3://bucket":            {Bucket: "bucket"},
		"s3://bucket/":           {Bucket: "bucket"},
		"s3://bucket/some/path":  {Bucket: "bucket", Prefix: "some/path"},
		"s3://bucket/some/path/": {Bucket: "bucket", Prefix: "some/path"},
	} {
		got, err := ParseS3Location(in)
		if err != nil {
			t.Errorf("%s: %v", in, err)
			continue
		}
		if got.Bucket != want.Bucket || got.Prefix != want.Prefix {
			t.Errorf("%s = %+v, want %+v", in, got, want)
		}
	}

	for _, in := range []string{"", "http://bucket", "s3://", "s3://bucket/Bad Prefix", "s3://bucket/../x"} {
		if _, err := ParseS3Location(in); err == nil {
			t.Errorf("%q: got nil error", in)
		}
	}
}

// The listing must not report objects under the prefix that kist did not
// write, and must not confuse one repository's prefix with another's.
func TestS3PrefixesAreIsolated(t *testing.T) {
	ctx := context.Background()
	srv := startMinio(t)

	open := func(prefix string) Backend {
		b, err := OpenS3(ctx, S3Config{
			Bucket: srv.bucket, Prefix: prefix, Endpoint: srv.endpoint, PathStyle: true,
			AccessKey: srv.accessKey, SecretKey: srv.secretKey,
		})
		if err != nil {
			t.Fatalf("open: %v", err)
		}
		return b
	}
	a, ab := open("iso/a"), open("iso/ab")

	if err := PutBytesIfAbsent(ctx, a, "packs/one", []byte("a")); err != nil {
		t.Fatalf("put: %v", err)
	}
	if err := PutBytesIfAbsent(ctx, ab, "packs/two", []byte("ab")); err != nil {
		t.Fatalf("put: %v", err)
	}

	var seen []string
	if err := a.List(ctx, "", func(fi FileInfo) error { seen = append(seen, fi.Key); return nil }); err != nil {
		t.Fatalf("list: %v", err)
	}
	if len(seen) != 1 || seen[0] != "packs/one" {
		t.Errorf("prefix iso/a lists %v, want [packs/one]; prefix iso/ab leaked in", seen)
	}
}
