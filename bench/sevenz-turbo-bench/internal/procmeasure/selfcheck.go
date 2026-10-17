package procmeasure

import (
	"context"
	"fmt"
	"os/exec"
	"runtime"
	"time"
)

// SelfCheckResult is what the start-up self-check measured.
type SelfCheckResult struct {
	// Tool is the tiny child that was run, "" when the check was skipped.
	Tool string
	// ChildBytes is the peak Run reported for it; HarnessBytes the
	// harness's own peak at the time, read from the same kind of source.
	ChildBytes   int64
	HarnessBytes int64
	Source       string
	// Skipped says why no check ran.
	Skipped string
}

// SelfCheck runs a child known to be tiny, `true`, through Run, and requires
// the peak Run reports for it to be below the harness's own. A floor under
// the children's figures (the harness's own footprint leaking into them, as
// rusage does on Linux) shows as the child reading at or above the harness,
// so the check catches the floor coming back on every run instead of a round
// of reports finding it.
//
// Windows is skipped: its peak is read from the child's own process handle,
// which no exec inherits into.
func SelfCheck(ctx context.Context) (SelfCheckResult, error) {
	if runtime.GOOS == "windows" {
		return SelfCheckResult{Skipped: "the Windows peak is the child's own process handle's"}, nil
	}
	tool, err := exec.LookPath("true")
	if err != nil {
		return SelfCheckResult{}, fmt.Errorf("RSS self-check: no `true` to run as the tiny child: %w", err)
	}
	result := Run(ctx, Command{Path: tool, Timeout: time.Minute})
	check := SelfCheckResult{Tool: tool, ChildBytes: result.MaxRSSBytes, Source: result.RSSSource}
	if result.Failure != "" || result.ExitCode != 0 {
		return check, fmt.Errorf("RSS self-check: %s failed (%s, exit %d): %v", tool, result.Failure, result.ExitCode, result.Err)
	}
	if result.MaxRSSBytes <= 0 || result.RSSSource != NativeRSSSource {
		return check, fmt.Errorf("RSS self-check: %s exited but no peak RSS was read (source %q, want %q): %v", tool, result.RSSSource, NativeRSSSource, result.Err)
	}
	if check.HarnessBytes, err = selfPeak(); err != nil {
		return check, fmt.Errorf("RSS self-check: the harness's own peak: %w", err)
	}
	if check.ChildBytes >= check.HarnessBytes {
		return check, fmt.Errorf("RSS self-check: %s, a process of a few MiB at most, reported a %s MiB peak, not below the harness's own %s MiB: the harness's footprint is flooring the children's figures (source %s)",
			tool, MiB(check.ChildBytes), MiB(check.HarnessBytes), check.Source)
	}
	return check, nil
}
