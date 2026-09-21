package source

import (
	"context"
	"errors"
	"fmt"
	"io"
	"slices"
	"strings"

	"github.com/aws/aws-sdk-go-v2/aws"
	"github.com/aws/aws-sdk-go-v2/service/s3"
	"github.com/aws/aws-sdk-go-v2/service/s3/types"
	"github.com/aws/smithy-go"
	smithyhttp "github.com/aws/smithy-go/transport/http"

	"github.com/at-least/kist/internal/backend"
	"github.com/at-least/kist/internal/tree"
)

// An S3Source is an object-storage prefix, listed and read the way a
// backup source needs: one level at a time, through the delimiter.
//
// S3 has no directories; a "directory" is a common prefix the listing
// reports. A prefix that is itself an object (s3://bucket/data naming
// the object data) is discovered with a HeadObject and reported as the
// single file it is, which is what the walker's file-root rule needs:
// object stores' plain delimiter listings hide the prefix object, and a
// file root that silently listed empty would quietly back up nothing.
type S3Source struct {
	locator []byte
	spec    string
	cfg     backend.S3Config

	client *s3.Client
}

// OpenS3Source builds a source over s3://bucket[/prefix]. Credentials,
// endpoint and path-style come from the environment, the same way the
// s3 backend resolves them.
func OpenS3Source(ctx context.Context, spec string) (*S3Source, error) {
	cfg, err := backend.ParseS3Location(spec)
	if err != nil {
		return nil, fmt.Errorf("open s3 source %s: %w", spec, err)
	}
	client, err := backend.S3ClientFor(ctx, cfg)
	if err != nil {
		return nil, fmt.Errorf("open s3 source %s: %w", spec, err)
	}
	return &S3Source{locator: []byte(spec), spec: spec, cfg: cfg, client: client}, nil
}

// Locator is the URL the source was opened with, as given.
func (s *S3Source) Locator() []byte { return s.locator }

// MetaKind is s3: the storage computes the etag, and the fast path
// trusts it above any timestamp.
func (s *S3Source) MetaKind() uint8 { return uint8(tree.MetaS3) }

// Close releases nothing: the HTTP client is pooled and process-wide.
func (s *S3Source) Close() error { return nil }

// join builds the object key for a relative source path.
func (s *S3Source) join(rel []byte) string {
	relStr := string(rel)
	if s.cfg.Prefix == "" {
		return relStr
	}
	if relStr == "" {
		return s.cfg.Prefix
	}
	return s.cfg.Prefix + "/" + relStr
}

// List returns one directory level: the objects directly under the
// prefix (no further slash in the remainder), the common prefixes as
// bare directory names, and -- for the root listing only -- the object
// the root prefix itself names, if there is one.
func (s *S3Source) List(ctx context.Context, dir []byte) ([]SourceItem, error) {
	// The root object: a file source's own name. Only the root probes
	// for it, because only the root is ever listed without the walker
	// already knowing whether it names a file or a directory.
	out := make([]SourceItem, 0, 16)
	if len(dir) == 0 && s.cfg.Prefix != "" && !strings.HasSuffix(s.spec, "/") {
		head, err := s.client.HeadObject(ctx, &s3.HeadObjectInput{
			Bucket: aws.String(s.cfg.Bucket),
			Key:    aws.String(s.cfg.Prefix),
		})
		switch {
		case err == nil:
			// The version id is the vern slot: the Rust peer records it
			// from the same HEAD (object_store reads x-amz-version-id),
			// and §8.1's fast path pins a versioned object to the exact
			// version it saw.
			out = append(out, makeFileItem(lastComponent(s.cfg.Prefix),
				uint64(aws.ToInt64(head.ContentLength)), //nolint:gosec // a length is never negative
				aws.ToTime(head.LastModified).UnixNano(),
				[]byte(aws.ToString(head.ETag)), []byte(aws.ToString(head.VersionId))))
		case isS3NotFound(err) || isS3AccessDenied(err):
			// Not a file, or unknowable: S3 answers 403 rather than 404
			// for a missing key when the caller lacks s3:ListBucket, the
			// same permission a plain listing needs. Either way the root
			// is not a proven single file; the directory listing below
			// says what this source actually holds.
		default:
			return nil, fmt.Errorf("head %s in s3 source %s: %w", s.cfg.Prefix, s.spec, err)
		}
	}

	prefix := s.join(dir)
	if prefix != "" {
		prefix += "/"
	}
	paginator := s3.NewListObjectsV2Paginator(s.client, &s3.ListObjectsV2Input{
		Bucket:    aws.String(s.cfg.Bucket),
		Prefix:    aws.String(prefix),
		Delimiter: aws.String("/"),
		MaxKeys:   aws.Int32(1000),
	})
	for paginator.HasMorePages() {
		page, err := paginator.NextPage(ctx)
		if err != nil {
			return nil, fmt.Errorf("list %q in s3 source %s: %w", prefix, s.spec, err)
		}
		for _, cp := range page.CommonPrefixes {
			name := lastComponent(strings.TrimSuffix(aws.ToString(cp.Prefix), "/"))
			if name == "" {
				continue
			}
			out = append(out, SourceItem{Name: []byte(name), Kind: SourceItemKind{Kind: KindDir}})
		}
		for _, obj := range page.Contents {
			key := aws.ToString(obj.Key)
			rel := strings.TrimPrefix(key, prefix)
			if rel == "" || strings.Contains(rel, "/") {
				continue // the prefix's own marker object, or deeper levels
			}
			out = append(out, makeFileItem(rel,
				uint64(aws.ToInt64(obj.Size)), //nolint:gosec // a length is never negative
				aws.ToTime(obj.LastModified).UnixNano(),
				[]byte(aws.ToString(obj.ETag)), nil))
		}
	}

	slices.SortFunc(out, func(a, b SourceItem) int { return slices.Compare(a.Name, b.Name) })
	return out, nil
}

// Read streams one object.
func (s *S3Source) Read(ctx context.Context, file []byte) (io.ReadCloser, error) {
	key := s.join(file)
	out, err := s.client.GetObject(ctx, &s3.GetObjectInput{
		Bucket: aws.String(s.cfg.Bucket),
		Key:    aws.String(key),
	})
	if err != nil {
		return nil, fmt.Errorf("read %s in s3 source %s: %w", key, s.spec, err)
	}
	return out.Body, nil
}

// lastComponent returns the bytes after the final slash.
func lastComponent(p string) string {
	if i := strings.LastIndexByte(p, '/'); i >= 0 {
		return p[i+1:]
	}
	return p
}

// isS3NotFound reports whether the service answered that the key holds
// no object. Both the typed error and the status code are checked,
// because S3-compatible services do not agree on which they send.
func isS3NotFound(err error) bool {
	var nsk *types.NoSuchKey
	var nf *types.NotFound
	if errors.As(err, &nsk) || errors.As(err, &nf) {
		return true
	}
	return hasStatus(err, 404)
}

// isS3AccessDenied reports whether the service refused the request for
// lack of permission.
func isS3AccessDenied(err error) bool {
	var apiErr smithy.APIError
	if errors.As(err, &apiErr) && apiErr.ErrorCode() == "AccessDenied" {
		return true
	}
	return hasStatus(err, 403)
}

func hasStatus(err error, status int) bool {
	var respErr *smithyhttp.ResponseError
	return errors.As(err, &respErr) && respErr.HTTPStatusCode() == status
}
