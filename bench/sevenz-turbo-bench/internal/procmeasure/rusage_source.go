//go:build !linux && !windows

package procmeasure

import (
	"runtime"
	"syscall"
)

// NativeRSSSource is where Run and Track read the peak resident set here:
// the child's own rusage. On macOS a process's ru_maxrss is the high-water
// mark of its own task, and exec does not carry the parent's into it, so the
// harness's footprint is no floor under it (the start-up self-check holds
// this on every run).
const NativeRSSSource = RSSSourceRusage

const nativeRSSSource = NativeRSSSource

// selfPeak is the harness's own peak resident set, from the same rusage.
func selfPeak() (int64, error) {
	var usage syscall.Rusage
	if err := syscall.Getrusage(syscall.RUSAGE_SELF, &usage); err != nil {
		return 0, err
	}
	peak := int64(usage.Maxrss)
	if runtime.GOOS != "darwin" {
		peak *= 1024
	}
	return peak, nil
}
