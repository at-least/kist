package cmd

import "fmt"

// humanBytes renders a size the way a person reads it. Binary units,
// because that is what a filesystem reports and a mismatch between the
// two is a support question nobody enjoys.
func humanBytes(n uint64) string {
	const unit = 1024
	if n < unit {
		return fmt.Sprintf("%d B", n)
	}

	value, exp := float64(n), 0
	for value >= unit && exp < 5 {
		value /= unit
		exp++
	}
	return fmt.Sprintf("%.1f %ciB", value, "KMGTP"[exp-1])
}
