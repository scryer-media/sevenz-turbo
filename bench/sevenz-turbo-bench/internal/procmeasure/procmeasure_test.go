package procmeasure

import (
	"context"
	"os"
	"runtime"
	"strconv"
	"strings"
	"testing"
)

// touchEnv makes the test binary a child that touches that many MiB of
// fresh memory and exits: a process whose peak resident set is known to be
// at least that large.
const touchEnv = "PROCMEASURE_TEST_TOUCH_MIB"

// exitEnv makes the test binary a child that writes to both streams and
// exits with the given status.
const exitEnv = "PROCMEASURE_TEST_EXIT"

func TestMain(m *testing.M) {
	if value := os.Getenv(touchEnv); value != "" {
		mebibytes, err := strconv.Atoi(value)
		if err != nil {
			os.Exit(2)
		}
		buffer := make([]byte, mebibytes<<20)
		for index := 0; index < len(buffer); index += 4096 {
			buffer[index] = 1
		}
		runtime.KeepAlive(buffer)
		os.Exit(0)
	}
	if value := os.Getenv(exitEnv); value != "" {
		code, err := strconv.Atoi(value)
		if err != nil {
			os.Exit(2)
		}
		os.Stdout.WriteString("out\n")
		os.Stderr.WriteString("err\n")
		os.Exit(code)
	}
	os.Exit(m.Run())
}

const touchedMiB = 96

func self(t *testing.T) string {
	t.Helper()
	path, err := os.Executable()
	if err != nil {
		t.Fatal(err)
	}
	return path
}

// The peak a child reaches is what Run records, tagged with this platform's
// source.
func TestRunRecordsTheChildsPeak(t *testing.T) {
	result := Run(context.Background(), Command{Path: self(t), Env: []string{touchEnv + "=" + strconv.Itoa(touchedMiB)}})
	if result.Failure != "" || result.ExitCode != 0 {
		t.Fatalf("child failed: %s exit %d: %v %s", result.Failure, result.ExitCode, result.Err, result.Stderr)
	}
	if result.MaxRSSBytes < touchedMiB<<20 {
		t.Fatalf("peak %d bytes, want at least %d MiB", result.MaxRSSBytes, touchedMiB)
	}
	if result.RSSSource != NativeRSSSource {
		t.Fatalf("rss source %q, want %q", result.RSSSource, NativeRSSSource)
	}
}

// A non-zero exit is reported as the exit code with the output tails, not as
// a harness failure: the caller decides what the status means.
func TestRunReportsExitCodesAndOutput(t *testing.T) {
	result := Run(context.Background(), Command{Path: self(t), Env: []string{exitEnv + "=3"}})
	if result.Failure != "" {
		t.Fatalf("unexpected failure %s: %v", result.Failure, result.Err)
	}
	if result.ExitCode != 3 || result.Stdout != "out\n" || result.Stderr != "err\n" {
		t.Fatalf("got exit %d stdout %q stderr %q", result.ExitCode, result.Stdout, result.Stderr)
	}
	if result.WallSeconds <= 0 || result.MaxRSSBytes <= 0 {
		t.Fatalf("missing measurements: %+v", result.Measurement)
	}
}

// A run that never started carries no peak and no source.
func TestRunWithoutAProcessHasNoPeak(t *testing.T) {
	result := Run(context.Background(), Command{Path: "/nonexistent/fixture-tool"})
	if result.Failure != "start-failed" || result.MaxRSSBytes != 0 || result.RSSSource != "" {
		t.Fatalf("got %+v", result)
	}
}

func TestCompleteRSSScenarioFlagsDifferentSources(t *testing.T) {
	scenario := RSSScenario{Scenario: "fixture", CandidateMedianBytes: 30 << 20, ReferenceMedianBytes: 20 << 20, CandidateSource: RSSSourceRusageTasksetExec, ReferenceSource: RSSSourceRusage}
	CompleteRSSScenario(&scenario)
	if scenario.Ratio == nil || *scenario.Ratio != 1.5 {
		t.Fatalf("ratio %v", scenario.Ratio)
	}
	if !strings.Contains(scenario.Note, "measured differently") {
		t.Fatalf("note %q", scenario.Note)
	}
	alone := RSSScenario{Scenario: "fixture", CandidateMedianBytes: 30 << 20, CandidateSource: RSSSourceRusage}
	CompleteRSSScenario(&alone)
	if alone.Ratio != nil || alone.Note != "" {
		t.Fatalf("a scenario without a reference got %#v", alone)
	}
}

func TestSortAndRenderRSSSummary(t *testing.T) {
	ratio := func(value float64) *float64 { return &value }
	scenarios := []RSSScenario{
		{Scenario: "fixture-b", CandidateMedianBytes: 1 << 20, CandidateMinBytes: 1 << 20, CandidateMaxBytes: 1 << 20, CandidateSource: RSSSourceRusage, Note: "no reference row"},
		{Scenario: "fixture-c", CandidateMedianBytes: 10 << 20, CandidateMinBytes: 9 << 20, CandidateMaxBytes: 11 << 20, ReferenceMedianBytes: 20 << 20, ReferenceMinBytes: 20 << 20, ReferenceMaxBytes: 20 << 20, Ratio: ratio(0.5), CandidateSource: RSSSourceRusage, ReferenceSource: RSSSourceRusage},
		{Scenario: "fixture-a", CandidateMedianBytes: 30 << 20, CandidateMinBytes: 30 << 20, CandidateMaxBytes: 30 << 20, ReferenceMedianBytes: 10 << 20, ReferenceMinBytes: 10 << 20, ReferenceMaxBytes: 10 << 20, Ratio: ratio(3), CandidateSource: RSSSourcePeakWorkingSet, ReferenceSource: RSSSourcePeakWorkingSet},
	}
	SortRSSScenarios(scenarios)
	if scenarios[0].Scenario != "fixture-a" || scenarios[1].Scenario != "fixture-c" || scenarios[2].Scenario != "fixture-b" {
		t.Fatalf("order %s %s %s", scenarios[0].Scenario, scenarios[1].Scenario, scenarios[2].Scenario)
	}
	var b strings.Builder
	RenderRSSSummary(&b, scenarios)
	want := strings.Join([]string{
		"| scenario | sevenz-turbo MiB | 7zz MiB | RSS ratio | source | note |",
		"|---|---|---|---|---|---|",
		"| fixture-a | 30.0 [30.0–30.0] | 10.0 [10.0–10.0] | 3.000 | peak-working-set | - |",
		"| fixture-c | 10.0 [9.0–11.0] | 20.0 [20.0–20.0] | 0.500 | rusage | - |",
		"| fixture-b | 1.0 [1.0–1.0] | - | - | rusage | no reference row |",
	}, "\n")
	if !strings.Contains(b.String(), want) {
		t.Fatalf("rendered:\n%s\nwant:\n%s", b.String(), want)
	}
}

func TestPeakStats(t *testing.T) {
	if median, low, high := PeakStats([]int64{30, 10, 20, 40}); median != 25 || low != 10 || high != 40 {
		t.Fatalf("got %d %d %d", median, low, high)
	}
	if median, low, high := PeakStats(nil); median != 0 || low != 0 || high != 0 {
		t.Fatalf("empty got %d %d %d", median, low, high)
	}
}
