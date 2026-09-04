//go:build !unix

package repo

import "errors"

// makeFIFO exists so the "unsupported file type" test compiles
// everywhere; the test itself skips on platforms without FIFOs.
func makeFIFO(string) error { return errors.New("FIFOs are not supported on this platform") }
