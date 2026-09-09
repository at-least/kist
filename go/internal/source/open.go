package source

import (
	"context"
	"fmt"
	"strings"
)

// OpenSource builds the Source a backup spec names: a local path, or an
// sftp://host/path or s3://bucket/prefix URL. Anything that spells a URL
// with a scheme this build does not source from is refused, rather than
// quietly becoming a local directory named "gopher://x".
func OpenSource(ctx context.Context, spec string) (Source, error) {
	switch {
	case strings.HasPrefix(spec, "sftp://"):
		return OpenSFTPSource(ctx, spec)
	case strings.HasPrefix(spec, "s3://"):
		return OpenS3Source(ctx, spec)
	case hasURLScheme(spec):
		return nil, fmt.Errorf("open source %s: unsupported source scheme (want sftp:// or s3://)", spec)
	default:
		return NewLocalSource(spec)
	}
}

// hasURLScheme reports whether spec looks like "scheme://..." with any
// scheme.
func hasURLScheme(spec string) bool {
	i := strings.Index(spec, "://")
	return i > 0 && isSchemeChar(spec[:i])
}

func isSchemeChar(s string) bool {
	if s == "" {
		return false
	}
	for i := 0; i < len(s); i++ {
		c := s[i]
		switch {
		case c >= 'a' && c <= 'z', c >= '0' && c <= '9', c == '+', c == '-', c == '.':
		default:
			return false
		}
	}
	return true
}
