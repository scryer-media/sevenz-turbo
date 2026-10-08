//go:build windows

package host

import "testing"

func TestWindowsReportsPhysicalMemory(t *testing.T) {
	if got := platformMemoryBytes(); got == 0 {
		t.Fatal("GlobalMemoryStatusEx reported no physical memory")
	}
}
