//go:build linux || darwin

package mount

import (
	"errors"

	"github.com/at-least/kist/internal/backend"
	"github.com/at-least/kist/internal/index"
)

// errorsIsNotFound also counts an invalid key as not found: a name that
// cannot be a key cannot name anything, and the probes file managers
// make (".Trash-1000", ".hidden") deserve ENOENT, not EIO.
func errorsIsNotFound(err error) bool {
	return errors.Is(err, backend.ErrNotFound) || errors.Is(err, index.ErrNotFound) || errors.Is(err, backend.ErrInvalidKey)
}
