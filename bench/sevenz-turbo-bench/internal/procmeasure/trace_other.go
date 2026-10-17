//go:build !linux

package procmeasure

import "os/exec"

// exitTrace is Linux's (trace_linux.go): elsewhere the kernel's own figure
// for the exited child is the child's alone, and nothing is traced.
type exitTrace struct{}

func beginExitTrace(*exec.Cmd) *exitTrace { return nil }

func (*exitTrace) abandon() {}

func (*exitTrace) await(int) (int64, error) { return 0, nil }
