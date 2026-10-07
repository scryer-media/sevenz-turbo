package report

import (
	"strings"
	"testing"

	"github.com/scryer-media/sevenz-turbo/bench/sevenz-turbo-bench/internal/fixtures"
	"github.com/scryer-media/sevenz-turbo/bench/sevenz-turbo-bench/internal/host"
	"github.com/scryer-media/sevenz-turbo/bench/sevenz-turbo-bench/internal/procmeasure"
	"github.com/scryer-media/sevenz-turbo/bench/sevenz-turbo-bench/internal/suite"
)

func run(scenario, variant, role string, wall float64, rss int64, status string) suite.RunRecord {
	return suite.RunRecord{
		Scenario: scenario, Group: "lzma2 parallel", Op: suite.OpDecode, Variant: variant, Role: role, Status: status,
		BytesOut:    100 << 20,
		Measurement: procmeasure.Measurement{WallSeconds: wall, UserSeconds: wall * 4, MaxRSSBytes: rss, RSSSource: procmeasure.RSSSourceRusage},
		Result:      map[string]any{"parallel_path": true, "max_spawned_threads": float64(4), "parse_seconds": 0.001},
	}
}

func sampleRaw() *suite.Raw {
	scenario := suite.Scenario{ID: "decode/mt/T4", Group: "lzma2 parallel", Op: suite.OpDecode, Fixture: "mt.7z", Threads: "4",
		Variants: []suite.Run{{Variant: suite.VariantTurbo, Role: suite.RoleCandidate}, {Variant: suite.VariantOracle, Role: suite.RoleReference},
			{Variant: suite.VariantUpstream, Role: suite.RoleSecondary}}}
	raw := &suite.Raw{Schema: suite.RawSchema, Machine: host.Machine{Label: "test-host", OS: "linux", Architecture: "arm64"},
		Fixtures: &fixtures.Manifest{Profile: "quick"}, Repeats: 3, Scenarios: []suite.Scenario{scenario}}
	for _, wall := range []float64{1.0, 1.2, 1.1} {
		raw.Runs = append(raw.Runs, run(scenario.ID, suite.VariantTurbo, suite.RoleCandidate, wall, 80<<20, suite.StatusOK))
		raw.Runs = append(raw.Runs, run(scenario.ID, suite.VariantOracle, suite.RoleReference, wall*2, 40<<20, suite.StatusOK))
	}
	failed := run(scenario.ID, suite.VariantUpstream, suite.RoleSecondary, 0, 0, suite.StatusFailed)
	failed.Failure = "op-error"
	raw.Runs = append(raw.Runs, failed)
	return raw
}

func TestBuildRatiosAndOrientation(t *testing.T) {
	built := Build(sampleRaw())
	if len(built.Failures) != 0 || len(built.SecondaryFailures) != 1 {
		t.Fatalf("failures %v secondary %v", built.Failures, built.SecondaryFailures)
	}
	if len(built.Ratios) != 1 {
		t.Fatalf("ratios %v", built.Ratios)
	}
	ratio := built.Ratios[0]
	if *ratio.Wall != 0.5 || *ratio.RSS != 2 {
		t.Fatalf("wall %v rss %v, want 0.5 and 2 (sevenz-turbo over 7zz)", *ratio.Wall, *ratio.RSS)
	}
	if built.Rows[0].Wall.Median != 1.1 || built.Rows[0].Wall.N != 3 {
		t.Fatalf("median %+v", built.Rows[0].Wall)
	}
	if len(built.RSS) != 1 || *built.RSS[0].Ratio != 2 {
		t.Fatalf("rss %+v", built.RSS)
	}
	md := Markdown(built)
	for _, want := range []string{Orientation, "## lzma2 parallel", "## Peak RSS per scenario", "spawned=4", "Secondary reference failures"} {
		if !strings.Contains(md, want) {
			t.Errorf("report.md lacks %q", want)
		}
	}
}

func TestCandidateFailureIsReported(t *testing.T) {
	raw := sampleRaw()
	missing := run("decode/mt/T4", suite.VariantTurbo, suite.RoleCandidate, 1, 0, suite.StatusFailed)
	missing.Failure = procmeasure.FailureMissingRSS
	raw.Runs = append(raw.Runs, missing)
	built := Build(raw)
	if len(built.Failures) != 1 || !strings.Contains(built.Failures[0], procmeasure.FailureMissingRSS) {
		t.Fatalf("failures %v", built.Failures)
	}
}

func TestMergeListsEveryHost(t *testing.T) {
	first := Build(sampleRaw())
	secondRaw := sampleRaw()
	secondRaw.Machine.Label = "other-host"
	second := Build(secondRaw)
	md := Merge([]*Report{first, second})
	for _, want := range []string{"test-host", "other-host", "decode/mt/T4", "## lzma2 parallel", "Worst peak RSS ratio"} {
		if !strings.Contains(md, want) {
			t.Errorf("merged report lacks %q", want)
		}
	}
}
