package backend

// syncDir is a no-op on Windows: directories cannot be opened for
// synchronisation there, and NTFS orders metadata updates itself, so
// there is nothing for a caller to flush.
func syncDir(string) error { return nil }
