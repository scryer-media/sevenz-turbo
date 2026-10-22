package report

import (
	"math"
	"os"
	"path/filepath"
	"strings"
	"testing"

	"github.com/scryer-media/sevenz-turbo/bench/sevenz-turbo-bench/internal/fixtures"
	"github.com/scryer-media/sevenz-turbo/bench/sevenz-turbo-bench/internal/host"
	"github.com/scryer-media/sevenz-turbo/bench/sevenz-turbo-bench/internal/suite"
)

// chartReport is one host's report with a one-thread and a four-thread
// decode, a budget row and a list row; turbo's wall is 7zz's over speedup.
func chartReport(label string, speedup float64) *Report {
	scenarios := []suite.Scenario{
		{ID: "list/mt", Group: "container parse", Op: suite.OpList, Threads: "1"},
		{ID: "decode/mt/T1", Group: "lzma2 parallel", Op: suite.OpDecode, Threads: "1"},
		{ID: "decode/mt/T4", Group: "lzma2 parallel", Op: suite.OpDecode, Threads: "4"},
		{ID: "decode/mt/T4/budget-64MiB", Group: "memory budget", Op: suite.OpDecode, Threads: "4", MemoryLimit: 64 << 20},
	}
	r := &Report{Machine: host.Machine{Label: label}, Scenarios: scenarios, Fixtures: &fixtures.Manifest{Profile: "quick"}}
	r.Toolchain.Oracle.Version = "26.04"
	add := func(scenario, variant string, wall float64) {
		r.Rows = append(r.Rows, Row{Scenario: scenario, Variant: variant, OK: 3,
			Wall: Stat{Median: wall}, CPU: Stat{Median: wall}, RSS: Stat{Median: 100}})
	}
	for _, scenario := range scenarios {
		add(scenario.ID, suite.VariantOracle, 2)
		add(scenario.ID, suite.VariantTurbo, 2/speedup)
	}
	add("decode/mt/T1", suite.VariantUpstream, 4)
	add("decode/mt/T4", suite.VariantUpstream, 4)
	return r
}

func TestChartsAverageHostsAndGroupByThreads(t *testing.T) {
	out := t.TempDir()
	inputs := []ChartInput{{Name: "X", Report: chartReport("x", 2)}, {Name: "Y", Report: chartReport("y", 8)}}
	paths, skipped, err := RenderCharts(inputs, out)
	if err != nil || len(skipped) != 0 {
		t.Fatalf("err %v skipped %v", err, skipped)
	}
	if len(paths) != 1 || filepath.Base(paths[0]) != "sevenz-turbo-decode.svg" {
		t.Fatalf("paths %v, want the decode chart alone", paths)
	}
	bars := chartBars(inputs, all(inputs), suite.OpDecode)
	var names []string
	for _, bar := range bars {
		names = append(names, bar.category)
	}
	if got, want := strings.Join(names, "|"), "All decode|Listing|LZMA2, 1 thread|LZMA2, multi-thread"; got != want {
		t.Fatalf("groups %q, want %q", got, want)
	}
	// The headline leaves the list and budget rows out; the two hosts' 2x
	// and 8x average to 4x.
	headline := bars[0].series[0]
	if headline.scenarios != 2 || math.Abs(headline.mean-4) > 1e-9 || math.Abs(headline.rssMean-1) > 1e-9 {
		t.Fatalf("headline %+v, want 2 scenarios at 4x and RSS at 1x", headline)
	}
	if upstream := bars[2].series[1]; upstream.tool != suite.VariantUpstream || math.Abs(upstream.mean-0.5) > 1e-9 || upstream.multiThread {
		t.Fatalf("sevenz-rust2 one-thread bar %+v, want 0.5 and no asterisk", upstream)
	}
	if upstream := bars[3].series[1]; !upstream.multiThread {
		t.Fatalf("sevenz-rust2 multi-thread bar %+v has no asterisk", upstream)
	}
	svg, err := os.ReadFile(filepath.Join(out, "sevenz-turbo-decode.svg"))
	if err != nil {
		t.Fatal(err)
	}
	for _, want := range []string{"7-Zip 26.04 is the 1x line", ">4.00x<", ">2.00x*<", ">0.50x<", "CPU time, 7-Zip ÷ tool", "peak RSS, tool ÷ 7-Zip", "better →", "essentially unimplemented"} {
		if !strings.Contains(string(svg), want) {
			t.Errorf("decode chart lacks %q", want)
		}
	}
}

func TestChartsSortGroupsBestFirst(t *testing.T) {
	r := chartReport("x", 2)
	for i := range r.Rows {
		if r.Rows[i].Scenario == "decode/mt/T4" && r.Rows[i].Variant == suite.VariantTurbo {
			r.Rows[i].Wall.Median = 0.25 // 8x, ahead of the others' 2x
		}
	}
	inputs := []ChartInput{{Report: r}}
	bars := chartBars(inputs, all(inputs), suite.OpDecode)
	if bars[0].category != "All decode" || bars[1].category != "LZMA2, multi-thread" {
		t.Fatalf("order %q then %q, want the headline then the fastest group", bars[0].category, bars[1].category)
	}
}

func all(inputs []ChartInput) map[string]bool {
	common, _ := commonScenarios(inputs)
	return common
}

func TestChartsLeaveOutScenariosAHostDidNotRun(t *testing.T) {
	other := chartReport("y", 2)
	other.Scenarios = other.Scenarios[:2]
	inputs := []ChartInput{{Report: chartReport("x", 2)}, {Report: other}}
	_, skipped, err := RenderCharts(inputs, t.TempDir())
	if err != nil {
		t.Fatal(err)
	}
	if got := strings.Join(skipped, ","); got != "decode/mt/T4,decode/mt/T4/budget-64MiB" {
		t.Fatalf("skipped %q", got)
	}
	for _, bar := range chartBars(inputs, all(inputs), suite.OpDecode) {
		if bar.category == "LZMA2, several threads" {
			t.Fatal("a scenario one host did not run was charted")
		}
	}
}

// A tool that failed a different scenario on each host has as many ratios on
// each, but not of the same workloads, so it gets no bar there.
func TestChartsDropASeriesMeasuredOnDifferentScenarios(t *testing.T) {
	fail := func(r *Report, scenario string) *Report {
		for i := range r.Rows {
			if r.Rows[i].Scenario == scenario && r.Rows[i].Variant == suite.VariantUpstream {
				r.Rows[i].OK = 0
			}
		}
		return r
	}
	inputs := []ChartInput{{Report: fail(chartReport("x", 2), "decode/mt/T1")}, {Report: fail(chartReport("y", 2), "decode/mt/T4")}}
	bars := chartBars(inputs, all(inputs), suite.OpDecode)
	if bars[0].category != "All decode" {
		t.Fatalf("first group %q, want the headline", bars[0].category)
	}
	for _, series := range bars[0].series {
		if series.tool == suite.VariantUpstream {
			t.Fatalf("sevenz-rust2 has a headline bar %+v over different scenarios on each host", series)
		}
	}
}

// Charting into a directory that holds an earlier run's encode chart, from
// reports with no encode rows, removes it rather than leave it beside the
// new decode chart.
func TestChartsRemoveAStaleChart(t *testing.T) {
	out := t.TempDir()
	stale := filepath.Join(out, "sevenz-turbo-encode.svg")
	if err := os.WriteFile(stale, []byte("<svg/>"), 0o644); err != nil {
		t.Fatal(err)
	}
	if _, _, err := RenderCharts([]ChartInput{{Report: chartReport("x", 2)}}, out); err != nil {
		t.Fatal(err)
	}
	if _, err := os.Stat(stale); !os.IsNotExist(err) {
		t.Fatal("an encode chart from other inputs was left in place")
	}
}
