package suite

import (
	"context"
	"encoding/json"
	"fmt"
	"io"
	"os"
	"os/exec"
	"strings"
	"time"

	"github.com/scryer-media/sevenz-turbo/bench/sevenz-turbo-bench/internal/fixtures"
	"github.com/scryer-media/sevenz-turbo/bench/sevenz-turbo-bench/internal/host"
	"github.com/scryer-media/sevenz-turbo/bench/sevenz-turbo-bench/internal/procmeasure"
	"github.com/scryer-media/sevenz-turbo/bench/sevenz-turbo-bench/internal/toolchain"
)

// RawSchema identifies raw.json.
const RawSchema = "sevenz-turbo-bench/raw/1"

// Statuses.
const (
	StatusOK     = "ok"
	StatusFailed = "failed"
	StatusDNF    = "dnf"
)

// RunRecord is one measured process run. Field names follow rarpar-bench.
type RunRecord struct {
	Scenario string `json:"scenario"`
	Group    string `json:"group"`
	Op       string `json:"op"`
	Variant  string `json:"variant"`
	Role     string `json:"role"`
	Tool     string `json:"tool"`
	Warmup   bool   `json:"warmup"`
	Repeat   int    `json:"repeat"`
	Position int    `json:"position"`
	Command  string `json:"command"`
	// LoadAverage is the host's one-minute load just before the run (-1
	// where the OS has none).
	LoadAverage float64 `json:"load_average"`
	procmeasure.Measurement
	// BytesIn / BytesOut are the operation's input and output: archive and
	// unpacked bytes for a decode, source and archive bytes for an encode.
	BytesIn  int64 `json:"bytes_in"`
	BytesOut int64 `json:"bytes_out"`
	// Result is decode-bench's own JSON line (candidate and secondary rows).
	Result map[string]any `json:"result,omitempty"`
	// Verified is the untimed `7zz t` of a candidate's encoded archive
	// (first measured repeat only): "ok" or the failure.
	Verified   string `json:"verified,omitempty"`
	Status     string `json:"status"`
	Failure    string `json:"failure,omitempty"`
	Error      string `json:"error,omitempty"`
	StderrLine string `json:"stderr_line,omitempty"`
}

// Raw is raw.json: everything a run measured.
type Raw struct {
	SchemaVersion  int                 `json:"schema_version"`
	Schema         string              `json:"schema"`
	StartedUTC     string              `json:"started_utc"`
	FinishedUTC    string              `json:"finished_utc"`
	Machine        host.Machine        `json:"machine"`
	Toolchain      toolchain.Toolchain `json:"toolchain"`
	Fixtures       *fixtures.Manifest  `json:"fixtures"`
	Quick          bool                `json:"quick"`
	Warmups        int                 `json:"warmups"`
	Repeats        int                 `json:"repeats"`
	Threads        []string            `json:"threads"`
	PinCPUs        string              `json:"pin_cpus,omitempty"`
	TimeoutSeconds float64             `json:"timeout_seconds,omitempty"`
	Scenarios      []Scenario          `json:"scenarios"`
	Runs           []RunRecord         `json:"runs"`
}

// Options control Execute.
type Options struct {
	Warmups int
	Repeats int
	PinCPUs string
	Timeout time.Duration
	Log     io.Writer
	// Oracle is used for the untimed verification of encoded archives.
	Oracle string
}

// orderFor alternates the variant order every repeat (A B C, C B A, ...), so
// no variant always runs first on a cold or last on a warm cache.
func orderFor(n, repeat int) []int {
	order := make([]int, n)
	for i := range order {
		if repeat%2 == 0 {
			order[i] = i
		} else {
			order[i] = n - 1 - i
		}
	}
	return order
}

// Execute runs every scenario, interleaving its variants, and appends a
// record per run.
func Execute(ctx context.Context, raw *Raw, options Options) {
	logf := func(format string, args ...any) {
		if options.Log != nil {
			fmt.Fprintf(options.Log, format+"\n", args...)
		}
	}
	digests := map[string]string{}
	for index, scenario := range raw.Scenarios {
		logf("[%d/%d] %s", index+1, len(raw.Scenarios), scenario.ID)
		total := options.Warmups + options.Repeats
		for pass := range total {
			warmup := pass < options.Warmups
			repeat := pass - options.Warmups
			if warmup {
				repeat = pass
			}
			for position, variant := range orderFor(len(scenario.Variants), pass) {
				run := scenario.Variants[variant]
				record := measure(ctx, raw, scenario, run, options)
				record.Warmup, record.Repeat, record.Position = warmup, repeat, position
				if !warmup && repeat == 0 && run.Role == RoleCandidate && scenario.Op == OpEncode && record.Status == StatusOK {
					record.Verified = verify(ctx, options.Oracle, run.Output, scenario.Encrypted)
					if record.Verified != "ok" {
						record.Status, record.Failure, record.Error = StatusFailed, "7zz-rejects-output", record.Verified
					}
				}
				if run.Output != "" {
					_ = os.Remove(run.Output)
				}
				checkDigest(digests, scenario, &record)
				line := fmt.Sprintf("  %-28s %-6s wall %.3fs rss %s MiB", run.Variant, record.Status, record.WallSeconds, procmeasure.MiB(record.MaxRSSBytes))
				if warmup {
					line += " (warmup)"
				}
				if record.Error != "" {
					line += " " + record.Error
				}
				logf("%s", line)
				raw.Runs = append(raw.Runs, record)
			}
		}
	}
}

// checkDigest holds every decode of the same archive by any of this crate's
// engines to one output digest (7zz t reports no digest; it checks CRCs).
func checkDigest(digests map[string]string, scenario Scenario, record *RunRecord) {
	if scenario.Op != OpDecode || record.Status != StatusOK || record.Result == nil {
		return
	}
	digest, _ := record.Result["digest"].(string)
	if digest == "" {
		return
	}
	if first, ok := digests[scenario.Fixture]; !ok {
		digests[scenario.Fixture] = digest
	} else if first != digest {
		record.Status, record.Failure = StatusFailed, "digest-mismatch"
		record.Error = fmt.Sprintf("output digest %s, earlier decode of %s gave %s", digest, scenario.Fixture, first)
	}
}

func measure(ctx context.Context, raw *Raw, scenario Scenario, run Run, options Options) RunRecord {
	command := procmeasure.Command{Path: run.Tool, Args: run.Args, Dir: run.Dir, PinCPUs: options.PinCPUs, Timeout: options.Timeout}
	record := RunRecord{
		Scenario: scenario.ID, Group: scenario.Group, Op: scenario.Op, Variant: run.Variant, Role: run.Role,
		Tool: run.Tool, Command: command.Describe(), LoadAverage: host.LoadAverage(),
	}
	if run.Output != "" {
		_ = os.Remove(run.Output)
	}
	result := procmeasure.Run(ctx, command)
	record.Measurement = result.Measurement
	switch {
	case result.Failure == "timeout":
		record.Status, record.Failure = StatusDNF, "timeout"
	case result.Failure == "signal":
		record.Status, record.Failure = StatusDNF, "signal"
	case result.Failure != "":
		record.Status, record.Failure = StatusFailed, result.Failure
	case result.ExitCode != 0:
		record.Status, record.Failure = StatusFailed, fmt.Sprintf("exit-%d", result.ExitCode)
	default:
		record.Status = StatusOK
	}
	if result.Err != nil {
		record.Error = result.Err.Error()
	}
	if record.Status != StatusOK {
		record.StderrLine = lastLine(result.Stderr, result.Stdout)
	}
	if run.JSON && result.Failure == "" {
		object, err := toolchain.LastJSON([]byte(result.Stdout))
		switch {
		case err != nil && record.Status == StatusOK:
			record.Status, record.Failure, record.Error = StatusFailed, "bad-json", err.Error()
		case err == nil:
			record.Result = object
			if ok, _ := object["ok"].(bool); !ok {
				message, _ := object["error"].(string)
				record.Status, record.Failure, record.Error = StatusFailed, "op-error", message
			}
		}
	}
	fillBytes(raw, scenario, run, &record)
	if record.Status == StatusOK && (record.MaxRSSBytes <= 0 || record.RSSSource == "") {
		record.Status, record.Failure = StatusFailed, procmeasure.FailureMissingRSS
		record.Error = "the process exited but its peak RSS was not captured"
	}
	return record
}

func fillBytes(raw *Raw, scenario Scenario, run Run, record *RunRecord) {
	switch scenario.Op {
	case OpList, OpDecode:
		archive, ok := raw.Fixtures.Archive(scenario.Fixture)
		if !ok {
			return
		}
		record.BytesIn = archive.Bytes
		record.BytesOut = archive.UnpackedBytes
		if scenario.Op == OpList {
			record.BytesOut = 0
		}
		if record.Status == StatusOK && record.Result != nil && scenario.Op == OpDecode {
			got := int64(number(record.Result["bytes_out"]))
			if got != archive.UnpackedBytes {
				record.Status, record.Failure = StatusFailed, "short-output"
				record.Error = fmt.Sprintf("decoded %d bytes, the fixture holds %d", got, archive.UnpackedBytes)
			}
		}
	case OpEncode:
		if source, ok := raw.Fixtures.Source(scenario.Fixture); ok {
			record.BytesIn = source.TotalBytes
		}
		if info, err := os.Stat(run.Output); err == nil && record.Status == StatusOK {
			record.BytesOut = info.Size()
		} else if record.Status == StatusOK {
			record.Status, record.Failure, record.Error = StatusFailed, "no-output", fmt.Sprintf("%s was not written", run.Output)
		}
	}
}

func number(value any) float64 {
	if f, ok := value.(float64); ok {
		return f
	}
	return 0
}

// verify runs an untimed `7zz t` over an archive this crate wrote.
func verify(ctx context.Context, oracle, path string, encrypted bool) string {
	args := []string{"t", "-bso0", "-bsp0"}
	if encrypted {
		args = append(args, "-p"+fixtures.Password)
	}
	output, err := exec.CommandContext(ctx, oracle, append(args, path)...).CombinedOutput()
	if err != nil {
		return fmt.Sprintf("7zz t: %v: %s", err, lastLine(string(output), ""))
	}
	return "ok"
}

func lastLine(texts ...string) string {
	for _, text := range texts {
		lines := strings.Split(strings.TrimSpace(text), "\n")
		for i := len(lines) - 1; i >= 0; i-- {
			if line := strings.TrimSpace(lines[i]); line != "" {
				return line
			}
		}
	}
	return ""
}

// Write saves raw.json.
func Write(path string, raw *Raw) error {
	data, err := json.MarshalIndent(raw, "", "  ")
	if err != nil {
		return err
	}
	return os.WriteFile(path, append(data, '\n'), 0o644)
}

// Load reads raw.json.
func Load(path string) (*Raw, error) {
	data, err := os.ReadFile(path)
	if err != nil {
		return nil, err
	}
	var raw Raw
	if err := json.Unmarshal(data, &raw); err != nil {
		return nil, fmt.Errorf("%s: %w", path, err)
	}
	if raw.Schema != RawSchema {
		return nil, fmt.Errorf("%s: schema %q, want %q", path, raw.Schema, RawSchema)
	}
	return &raw, nil
}
