package report

import (
	"strings"
	"testing"

	"github.com/scryer-media/sevenz-turbo/bench/sevenz-turbo-bench/internal/fixtures"
	"github.com/scryer-media/sevenz-turbo/bench/sevenz-turbo-bench/internal/host"
	"github.com/scryer-media/sevenz-turbo/bench/sevenz-turbo-bench/internal/procmeasure"
	"github.com/scryer-media/sevenz-turbo/bench/sevenz-turbo-bench/internal/suite"
	"github.com/scryer-media/sevenz-turbo/bench/sevenz-turbo-bench/internal/toolchain"
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
	if *ratio.Wall != 2 || *ratio.RSS != 0.5 {
		t.Fatalf("wall %v rss %v, want 2 and 0.5 (7zz over sevenz-turbo)", *ratio.Wall, *ratio.RSS)
	}
	if built.Rows[0].Wall.Median != 1.1 || built.Rows[0].Wall.N != 3 {
		t.Fatalf("median %+v", built.Rows[0].Wall)
	}
	if len(built.RSS) != 1 || *built.RSS[0].Ratio != 0.5 {
		t.Fatalf("rss %+v", built.RSS)
	}
	md := Markdown(built)
	for _, want := range []string{Orientation, "## lzma2 parallel", "## Peak RSS per scenario", "spawned=4", "Secondary reference failures"} {
		if !strings.Contains(md, want) {
			t.Errorf("report.md lacks %q", want)
		}
	}
	if strings.Contains(md, "CPU pinning") {
		t.Error("an unpinned run's report.md mentions pinning")
	}
}

func TestPinnedRunIsReported(t *testing.T) {
	raw := sampleRaw()
	raw.PinCPUs = "0-7"
	built := Build(raw)
	if built.PinCPUs != "0-7" {
		t.Fatalf("report.json pin_cpus %q, want 0-7", built.PinCPUs)
	}
	if md := Markdown(built); !strings.Contains(md, "confined to CPUs 0-7") {
		t.Error("report.md does not state the pinned CPU range")
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
	md, err := Merge([]*Report{first, second})
	if err != nil {
		t.Fatal(err)
	}
	for _, want := range []string{"test-host", "other-host", "decode/mt/T4", "## lzma2 parallel", "Worst peak RSS ratio"} {
		if !strings.Contains(md, want) {
			t.Errorf("merged report lacks %q", want)
		}
	}
}

func TestMergeRefusesDifferentWorkloads(t *testing.T) {
	base := func() *suite.Raw {
		raw := sampleRaw()
		raw.Fixtures.Oracle.Banner = "7-Zip (z) 26.01 (arm64) : Copyright (c) 1999-2026 Igor Pavlov : 2026-04-27"
		raw.Fixtures.Sources = []fixtures.SourceRecord{{SourceSpec: fixtures.SourceSpec{Name: "text"}, SHA256: "aaa"}}
		raw.Fixtures.Archives = []fixtures.ArchiveRecord{{ArchiveSpec: fixtures.ArchiveSpec{Name: "mt.7z", Source: "text", Args: []string{"-mx=5"}}, SHA256: "host-specific"}}
		raw.Toolchain.Candidates = []toolchain.Candidate{{Label: "sevenz-turbo", Version: map[string]any{"git_commit": "c1", "git_dirty": "false", "cargo_lock_sha256": "l1"}}}
		return raw
	}
	first := Build(base())
	sameRaw := base()
	sameRaw.Machine.Label = "other-host"
	sameRaw.Fixtures.Archives[0].SHA256 = "differs-by-salt"
	// The same release built for another architecture wrote the same corpus.
	sameRaw.Fixtures.Oracle.Banner = "7-Zip (z) 26.01 (x64) : Copyright (c) 1999-2026 Igor Pavlov : 2026-04-27"
	if _, err := Merge([]*Report{first, Build(sameRaw)}); err != nil {
		t.Fatalf("matching workloads refused: %v", err)
	}
	for name, change := range map[string]func(*suite.Raw){
		"profile": func(r *suite.Raw) { r.Fixtures.Profile = "full" },
		"source":  func(r *suite.Raw) { r.Fixtures.Sources[0].SHA256 = "bbb" },
		"args":    func(r *suite.Raw) { r.Fixtures.Archives[0].Args = []string{"-mx=9"} },
		"commit":  func(r *suite.Raw) { r.Toolchain.Candidates[0].Version["git_commit"] = "c2" },
		"lock":    func(r *suite.Raw) { r.Toolchain.Candidates[0].Version["cargo_lock_sha256"] = "l2" },
		"oracle":  func(r *suite.Raw) { r.Toolchain.Oracle.Version = "99.0" },
		"quick":   func(r *suite.Raw) { r.Quick = !r.Quick },
		"pinning": func(r *suite.Raw) { r.PinCPUs = "0-7" },
		"fixture 7zz": func(r *suite.Raw) {
			r.Fixtures.Oracle.Banner = "7-Zip (z) 25.01 (x64) : Copyright (c) 1999-2025 Igor Pavlov : 2025-08-03"
		},
		"only":    func(r *suite.Raw) { r.Only = []string{"decode/mt"} },
		"repeats": func(r *suite.Raw) { r.Repeats++ },
		"warmups": func(r *suite.Raw) { r.Warmups++ },
		"dirty":   func(r *suite.Raw) { r.Toolchain.Candidates[0].Version["git_dirty"] = "true" },
		"unknown": func(r *suite.Raw) { delete(r.Toolchain.Candidates[0].Version, "git_dirty") },
	} {
		raw := base()
		raw.Machine.Label = "other-host"
		change(raw)
		if _, err := Merge([]*Report{first, Build(raw)}); err == nil {
			t.Errorf("%s differs but the reports were merged", name)
		}
	}
	legacy := Build(base())
	legacy.Fixtures = nil
	if _, err := Merge([]*Report{first, legacy}); err == nil || !strings.Contains(err.Error(), "report") {
		t.Errorf("a report without a fixture manifest was merged: %v", err)
	}
}

func TestSlowThroughputIsNotRoundedToZero(t *testing.T) {
	for _, c := range []struct {
		mibs float64
		want string
	}{
		{1234.5, "1234"},
		{123.4, "123"},
		{12.34, "12.3"},
		{1.234, "1.23"},
		{0.4567, "0.457"},
		{0.25, "0.25"},
		{0.0123, "0.0123"},
	} {
		if got := throughputText(c.mibs); got != c.want {
			t.Errorf("throughputText(%v) = %q, want %q", c.mibs, got, c.want)
		}
	}
}

// A row with a second reference gets a ratio against each, and each ratio
// names the reference it is against, in report.json and in both reports.
func TestEveryRatioNamesItsReference(t *testing.T) {
	raw := sampleRaw()
	raw.Scenarios[0].Variants = append(raw.Scenarios[0].Variants, suite.Run{Variant: suite.VariantOracleOneThread, Role: suite.RoleReference})
	for _, wall := range []float64{1.0, 1.2, 1.1} {
		raw.Runs = append(raw.Runs, run(raw.Scenarios[0].ID, suite.VariantOracleOneThread, suite.RoleReference, wall*3, 40<<20, suite.StatusOK))
	}
	built := Build(raw)
	against := map[string]float64{}
	for _, ratio := range built.Ratios {
		against[ratio.Reference] = *ratio.Wall
	}
	if len(built.Ratios) != 2 || against[suite.VariantOracle] != 2 || against[suite.VariantOracleOneThread] != 3 {
		t.Fatalf("ratios %+v, want 2 against 7zz and 3 against 7zz -mmtf=off", built.Ratios)
	}
	md := Markdown(built)
	if !strings.Contains(md, "| | sevenz-turbo vs 7zz -mmtf=off | - | - | - | - | 3.000 |") {
		t.Errorf("report.md does not label the second reference's ratio:\n%s", md)
	}
	merged, err := Merge([]*Report{built})
	if err != nil {
		t.Fatal(err)
	}
	for _, want := range []string{"| decode/mt/T4 | sevenz-turbo | 1.100 / 2.000 /", "| decode/mt/T4 | sevenz-turbo vs 7zz -mmtf=off | 1.100 / 3.000 /"} {
		if !strings.Contains(merged, want) {
			t.Errorf("merged report lacks %q:\n%s", want, merged)
		}
	}
}
