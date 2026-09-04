package backend

import (
	"context"
	"errors"
	"fmt"
	"io"
	"net/url"
	"os"
	"strconv"
	"strings"
	"time"

	"github.com/aws/aws-sdk-go-v2/aws"
	awsconfig "github.com/aws/aws-sdk-go-v2/config"
	"github.com/aws/aws-sdk-go-v2/credentials"
	"github.com/aws/aws-sdk-go-v2/service/s3"
	"github.com/aws/aws-sdk-go-v2/service/s3/types"
	"github.com/aws/smithy-go"
	smithyhttp "github.com/aws/smithy-go/transport/http"
)

// Environment variables the S3 backend reads beyond the standard AWS_*
// set. They exist for S3-compatible services -- MinIO, Ceph, R2 -- whose
// endpoint is not derivable from a region.
const (
	// S3EndpointEnv overrides the service endpoint, e.g.
	// http://localhost:9000 for a local MinIO.
	S3EndpointEnv = "KIST_S3_ENDPOINT"

	// S3PathStyleEnv, when set to 1, addresses the bucket as a path
	// component rather than a subdomain, which is what most
	// S3-compatible services need.
	S3PathStyleEnv = "KIST_S3_PATH_STYLE"
)

// S3Config configures an S3 backend. The zero value reads everything
// from the environment.
type S3Config struct {
	Bucket string
	Prefix string

	// Endpoint is an alternative service URL. Empty means AWS.
	Endpoint string
	// Region defaults to $AWS_REGION, then us-east-1.
	Region string
	// PathStyle addresses the bucket as a path rather than a subdomain.
	PathStyle bool

	// AccessKey and SecretKey override the default credential chain.
	// Empty means the SDK's usual lookup: env, shared config, IMDS.
	AccessKey string
	SecretKey string
}

// S3 stores objects in an S3-compatible bucket, optionally under a
// prefix.
//
// Immutability is enforced by the conditional write: PutIfAbsent is a
// PutObject with If-None-Match: *, which the service refuses with 412
// when the key is taken. That single header is what turns "backup
// credentials cannot delete data" from a policy into a property of the
// storage.
type S3 struct {
	client   *s3.Client
	bucket   string
	prefix   string
	location string
}

// S3 implements Backend.
var _ Backend = (*S3)(nil)

// ParseS3Location parses s3://bucket[/prefix].
func ParseS3Location(location string) (S3Config, error) {
	u, err := url.Parse(location)
	if err != nil {
		return S3Config{}, fmt.Errorf("parse %q: %w", location, err)
	}
	if u.Scheme != "s3" {
		return S3Config{}, fmt.Errorf("parse %q: scheme is %q, want s3", location, u.Scheme)
	}
	if u.Host == "" {
		return S3Config{}, fmt.Errorf("parse %q: no bucket name", location)
	}

	cfg := S3Config{
		Bucket: u.Host,
		Prefix: strings.Trim(u.Path, "/"),
	}
	if cfg.Prefix != "" {
		if err := ValidateKey(cfg.Prefix); err != nil {
			return S3Config{}, fmt.Errorf("parse %q: prefix: %w", location, err)
		}
	}
	return cfg, nil
}

// OpenS3 connects to a bucket. It performs no request: the first
// operation is what discovers a wrong endpoint or a missing bucket.
func OpenS3(ctx context.Context, cfg S3Config) (*S3, error) {
	if cfg.Bucket == "" {
		return nil, errors.New("open s3 backend: no bucket")
	}
	if cfg.Endpoint == "" {
		cfg.Endpoint = os.Getenv(S3EndpointEnv)
	}
	if !cfg.PathStyle {
		cfg.PathStyle = s3Bool(S3PathStyleEnv)
	}
	if cfg.Region == "" {
		cfg.Region = os.Getenv("AWS_REGION")
	}
	if cfg.Region == "" {
		cfg.Region = "us-east-1"
	}

	var loadOpts []func(*awsconfig.LoadOptions) error
	loadOpts = append(loadOpts, awsconfig.WithRegion(cfg.Region))
	if cfg.AccessKey != "" || cfg.SecretKey != "" {
		loadOpts = append(loadOpts, awsconfig.WithCredentialsProvider(
			credentials.NewStaticCredentialsProvider(cfg.AccessKey, cfg.SecretKey, "")))
	}

	awsCfg, err := awsconfig.LoadDefaultConfig(ctx, loadOpts...)
	if err != nil {
		return nil, fmt.Errorf("open s3 backend: load aws config: %w", err)
	}

	client := s3.NewFromConfig(awsCfg, func(o *s3.Options) {
		if cfg.Endpoint != "" {
			o.BaseEndpoint = aws.String(cfg.Endpoint)
		}
		o.UsePathStyle = cfg.PathStyle
		// The SDK's default adds a CRC checksum trailer to every upload,
		// which turns a plain PUT into an aws-chunked body that not
		// every S3-compatible service accepts alongside a conditional
		// header. Every object kist stores is already named by its own
		// hash, so a transport checksum adds nothing.
		o.RequestChecksumCalculation = aws.RequestChecksumCalculationWhenRequired
		o.ResponseChecksumValidation = aws.ResponseChecksumValidationWhenRequired
	})

	location := "s3://" + cfg.Bucket
	if cfg.Prefix != "" {
		location += "/" + cfg.Prefix
	}
	if cfg.Endpoint != "" {
		location += " (" + cfg.Endpoint + ")"
	}

	return &S3{client: client, bucket: cfg.Bucket, prefix: cfg.Prefix, location: location}, nil
}

// Location reports the bucket, prefix and endpoint.
func (s *S3) Location() string { return s.location }

// Close releases nothing: the HTTP client is pooled and process-wide.
func (s *S3) Close() error { return nil }

func (s *S3) objectKey(key string) (string, error) {
	if err := ValidateKey(key); err != nil {
		return "", err
	}
	if s.prefix == "" {
		return key, nil
	}
	return s.prefix + "/" + key, nil
}

func (s *S3) stripPrefix(objectKey string) (string, bool) {
	if s.prefix == "" {
		return objectKey, true
	}
	return strings.CutPrefix(objectKey, s.prefix+"/")
}

// Get fetches a byte range with an HTTP Range header, so that reaching a
// pack's trailer or a single chunk never transfers the whole object.
func (s *S3) Get(ctx context.Context, key string, off, length int64) (io.ReadCloser, error) {
	objectKey, err := s.objectKey(key)
	if err != nil {
		return nil, err
	}
	if off < 0 {
		return nil, fmt.Errorf("get %s: offset %d is negative", key, off)
	}
	if length < 0 && length != ReadToEnd {
		return nil, fmt.Errorf("get %s: length %d is negative", key, length)
	}

	// S3 rejects an empty range outright, and a Range past the end of an
	// object is 416, not an empty body. Both are cases the caller means
	// as "nothing", so they are answered here after confirming the
	// object exists.
	if length == 0 {
		if _, err := s.Stat(ctx, key); err != nil {
			return nil, err
		}
		return io.NopCloser(strings.NewReader("")), nil
	}

	in := &s3.GetObjectInput{Bucket: aws.String(s.bucket), Key: aws.String(objectKey)}
	switch {
	case length == ReadToEnd && off == 0:
		// Whole object: no Range header at all.
	case length == ReadToEnd:
		in.Range = aws.String(fmt.Sprintf("bytes=%d-", off))
	default:
		in.Range = aws.String(fmt.Sprintf("bytes=%d-%d", off, off+length-1))
	}

	out, err := s.client.GetObject(ctx, in)
	if err != nil {
		if isRangeNotSatisfiable(err) {
			// Reading from exactly the end of an object is legal for the
			// local backend and means "nothing left"; S3 says 416.
			if _, serr := s.Stat(ctx, key); serr != nil {
				return nil, serr
			}
			return io.NopCloser(strings.NewReader("")), nil
		}
		return nil, s.wrap("get", key, err)
	}
	return out.Body, nil
}

// Put stores an object unconditionally.
func (s *S3) Put(ctx context.Context, key string, r io.Reader, size int64) error {
	objectKey, err := s.objectKey(key)
	if err != nil {
		return err
	}
	if size < 0 {
		return fmt.Errorf("put %s: size must be known for an upload", key)
	}

	_, err = s.client.PutObject(ctx, &s3.PutObjectInput{
		Bucket:        aws.String(s.bucket),
		Key:           aws.String(objectKey),
		Body:          r,
		ContentLength: aws.Int64(size),
	})
	if err != nil {
		return s.wrap("put", key, err)
	}
	return nil
}

// PutIfAbsent uploads with If-None-Match: *. The service refuses the
// write with 412 Precondition Failed when the key exists, which becomes
// ErrExists. Two conditional writes that race can also produce 409
// Conditional Request Conflict, which is retried: one of the two will
// then see either success or 412.
func (s *S3) PutIfAbsent(ctx context.Context, key string, r io.Reader, size int64) error {
	objectKey, err := s.objectKey(key)
	if err != nil {
		return err
	}
	if size < 0 {
		return fmt.Errorf("put %s: size must be known for an upload", key)
	}
	seeker, ok := r.(io.ReadSeeker)
	if !ok {
		// A retry has to replay the body, and the SDK itself rewinds a
		// seekable body between attempts. Everything kist uploads is a
		// *os.File or a bytes.Reader, so this is a programming error,
		// not a runtime condition.
		return fmt.Errorf("put %s: body must be seekable for a conditional upload", key)
	}

	const attempts = 5
	backoff := 100 * time.Millisecond
	for attempt := 1; ; attempt++ {
		if _, err := seeker.Seek(0, io.SeekStart); err != nil {
			return fmt.Errorf("put %s: rewind body: %w", key, err)
		}

		_, err := s.client.PutObject(ctx, &s3.PutObjectInput{
			Bucket:        aws.String(s.bucket),
			Key:           aws.String(objectKey),
			Body:          seeker,
			ContentLength: aws.Int64(size),
			IfNoneMatch:   aws.String("*"),
		})
		switch {
		case err == nil:
			return nil
		case isPreconditionFailed(err):
			return fmt.Errorf("put %s: %w", key, ErrExists)
		case isConditionalConflict(err) && attempt < attempts:
			select {
			case <-time.After(backoff):
			case <-ctx.Done():
				return fmt.Errorf("put %s: %w", key, ctx.Err())
			}
			backoff *= 2
		default:
			return s.wrap("put", key, err)
		}
	}
}

// List pages through ListObjectsV2 under the prefix.
func (s *S3) List(ctx context.Context, prefix string, fn func(FileInfo) error) error {
	if err := ValidatePrefix(prefix); err != nil {
		return err
	}
	full := prefix
	if s.prefix != "" {
		full = s.prefix + "/" + prefix
	}

	paginator := s3.NewListObjectsV2Paginator(s.client, &s3.ListObjectsV2Input{
		Bucket: aws.String(s.bucket),
		Prefix: aws.String(full),
	})
	for paginator.HasMorePages() {
		page, err := paginator.NextPage(ctx)
		if err != nil {
			// Classified like every other call: a backup client whose
			// policy does not cover a prefix must see ErrDenied here, not
			// an opaque SDK error.
			return s.wrap("list "+strconv.Quote(prefix), "", err)
		}
		for _, obj := range page.Contents {
			key, ok := s.stripPrefix(aws.ToString(obj.Key))
			if !ok {
				continue
			}
			// Passed through even if it is not a valid kist key, exactly
			// as the local backend does: check must be able to report
			// junk under a repository prefix, not have it hidden.
			if err := fn(FileInfo{Key: key, Size: aws.ToInt64(obj.Size)}); err != nil {
				return fmt.Errorf("list %q in %s: %w", prefix, s.location, err)
			}
		}
	}
	return nil
}

// Stat is a HeadObject.
func (s *S3) Stat(ctx context.Context, key string) (FileInfo, error) {
	objectKey, err := s.objectKey(key)
	if err != nil {
		return FileInfo{}, err
	}

	out, err := s.client.HeadObject(ctx, &s3.HeadObjectInput{
		Bucket: aws.String(s.bucket),
		Key:    aws.String(objectKey),
	})
	if err != nil {
		return FileInfo{}, s.wrap("stat", key, err)
	}
	return FileInfo{Key: key, Size: aws.ToInt64(out.ContentLength)}, nil
}

// Delete removes an object. S3 answers 204 for a missing key, so the
// "already gone" case needs no special handling.
//
// A 403 is returned as ErrDenied, kept distinguishable from other
// failures. It is *not* how Object Lock shows up: on a versioned bucket a
// DeleteObject without a version ID succeeds by writing a delete marker,
// even when the version underneath is locked -- the key vanishes from
// List while the bytes stay. Only version-specific deletes are refused.
// M3's prune has to reckon with that, and does not get to learn it from
// this error.
func (s *S3) Delete(ctx context.Context, key string) error {
	objectKey, err := s.objectKey(key)
	if err != nil {
		return err
	}

	_, err = s.client.DeleteObject(ctx, &s3.DeleteObjectInput{
		Bucket: aws.String(s.bucket),
		Key:    aws.String(objectKey),
	})
	if err != nil {
		return s.wrap("delete", key, err)
	}
	return nil
}

// ErrDenied means the service refused the operation for lack of
// permission. A backup client is expected to hit this on anything it
// should not be doing; that is the permission model working.
var ErrDenied = errors.New("access denied")

func (s *S3) wrap(op, key string, err error) error {
	what := op
	if key != "" {
		what += " " + key
	}
	switch {
	case isNotFound(err):
		return fmt.Errorf("%s: %w", what, ErrNotFound)
	case isAccessDenied(err):
		return fmt.Errorf("%s in %s: %w: %w", what, s.location, ErrDenied, err)
	default:
		return fmt.Errorf("%s in %s: %w", what, s.location, err)
	}
}

// Error classification. The SDK surfaces service errors as smithy
// APIErrors carrying the S3 error code, and HTTP-level failures as
// ResponseErrors carrying the status; both are checked, because
// S3-compatible services do not all agree on which one they send.

func isNotFound(err error) bool {
	var nsk *types.NoSuchKey
	var nf *types.NotFound
	if errors.As(err, &nsk) || errors.As(err, &nf) {
		return true
	}
	return hasCode(err, "NoSuchKey", "NotFound") || hasStatus(err, 404)
}

func isPreconditionFailed(err error) bool {
	return hasCode(err, "PreconditionFailed") || hasStatus(err, 412)
}

func isConditionalConflict(err error) bool {
	return hasCode(err, "ConditionalRequestConflict") || hasStatus(err, 409)
}

func isRangeNotSatisfiable(err error) bool {
	return hasCode(err, "InvalidRange") || hasStatus(err, 416)
}

func isAccessDenied(err error) bool {
	return hasCode(err, "AccessDenied") || hasStatus(err, 403)
}

func hasCode(err error, codes ...string) bool {
	var apiErr smithy.APIError
	if !errors.As(err, &apiErr) {
		return false
	}
	for _, c := range codes {
		if apiErr.ErrorCode() == c {
			return true
		}
	}
	return false
}

func hasStatus(err error, status int) bool {
	var respErr *smithyhttp.ResponseError
	return errors.As(err, &respErr) && respErr.HTTPStatusCode() == status
}

// s3Bool reads a boolean environment flag. An unparseable value is
// false: an env var set to "yes" is a configuration mistake, not one the
// backend can act on, and it will be visible when path-style addressing
// fails to work.
func s3Bool(name string) bool {
	v, err := strconv.ParseBool(os.Getenv(name))
	return err == nil && v
}
