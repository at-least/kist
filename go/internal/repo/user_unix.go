//go:build unix

package repo

import (
	"os/user"
)

// currentUser is the snapshot's human-label user field.
func currentUser() (string, error) {
	u, err := user.Current()
	if err != nil {
		return "", err
	}
	return u.Username, nil
}
