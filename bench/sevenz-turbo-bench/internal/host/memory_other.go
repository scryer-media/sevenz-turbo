//go:build !windows

package host

// platformMemoryBytes is only needed on Windows; memoryBytes reads the other
// systems' memory from sysctl and /proc.
func platformMemoryBytes() uint64 { return 0 }
