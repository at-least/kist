//go:build linux || darwin

package mount

import (
	"errors"

	"github.com/at-least/kist/internal/backend"
	"github.com/at-least/kist/internal/index"
)

func errorsIsNotFound(err error) bool {
	return errors.Is(err, backend.ErrNotFound) || errors.Is(err, index.ErrNotFound)
}
