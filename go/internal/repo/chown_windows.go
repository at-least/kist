package repo

import "errors"

// chown has no meaning on Windows, where ownership is an ACL rather than
// a uid/gid pair. Saying so is more honest than silently succeeding.
func chown(string, uint32, uint32) error {
	return errors.New("ownership is not restorable on Windows")
}
