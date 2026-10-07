// Package report turns raw.json into report.json (schema_version 1) and
// report.md, and merges several hosts' report.json into one cross-arch
// report.md.
package report

import (
	"encoding/json"
	"fmt"
	"os"
	"sort"
	"strings"

	"github.com/scryer-media/sevenz-turbo/bench/sevenz-turbo-bench/internal/host"
	"github.com/scryer-media/sevenz-turbo/bench/sevenz-turbo-bench/internal/procmeasure"
	"github.com/scryer-media/sevenz-turbo/bench/sevenz-turbo-bench/internal/suite"
	"github.com/scryer-media/sevenz-turbo/bench/sevenz-turbo-bench/internal/toolchain"
)

// Schema identifies report.json.
const Schema = "sevenz-turbo-bench/report/1"

// Orientation is stated in every report header.
const Orientation = "ratio = 7zz / sevenz-turbo, >1 = sevenz-turbo better. Every ratio is reference over candidate (oracle/ours) of the medians: above 1.000 sevenz-turbo is faster (wall, CPU), smaller (archive size) or lower (peak RSS); below 1.000 it is slower, larger or higher."

// Stat is a median with its range.
type Stat struct {
	Median float64 `json:"median"`
	Min    float64 `json:"min"`
	Max    float64 `json:"max"`
	N      int     `json:"n"`
}

func stat(values []float64) Stat {
	if len(values) == 0 {
		return Stat{}
	}
	sorted := append([]float64(nil), values...)
	sort.Float64s(sorted)
	middle := len(sorted) / 2
	median := sorted[middle]
	if len(sorted)%2 == 0 {
		median = (sorted[middle-1] + sorted[middle]) / 2
	}
	return Stat{Median: median, Min: sorted[0], Max: sorted[len(sorted)-1], N: len(sorted)}
}

// Row is one variant's summary of one scenario over the measured runs.
type Row struct {
	Scenario  string `json:"scenario"`
	Group     string `json:"group"`
	Op        string `json:"op"`
	Variant   string `json:"variant"`
	Role      string `json:"role"`
	Wall      Stat   `json:"wall_seconds"`
	CPU       Stat   `json:"cpu_seconds"`
	RSS       Stat   `json:"max_rss_bytes"`
	RSSSource string `json:"rss_source"`
	Load      Stat   `json:"load_average"`
	BytesIn   int64  `json:"bytes_in"`
	BytesOut  int64  `json:"bytes_out"`
	// ThroughputMiBs is the uncompressed bytes per wall second: output for a
	// decode, input for an encode.
	ThroughputMiBs float64  `json:"throughput_mib_s,omitempty"`
	OK             int      `json:"ok"`
	Failed         int      `json:"failed"`
	DNF            string   `json:"dnf,omitempty"`
	Failures       []string `json:"failures,omitempty"`
	// Extra is what decode-bench reported that explains the row:
	// parallel_path and max_spawned_threads, crypto backend, block size.
	Extra string `json:"extra,omitempty"`
}

// Ratio compares one candidate variant against 7zz in one scenario.
type Ratio struct {
	Scenario string   `json:"scenario"`
	Group    string   `json:"group"`
	Variant  string   `json:"variant"`
	Wall     *float64 `json:"wall,omitempty"`
	CPU      *float64 `json:"cpu,omitempty"`
	RSS      *float64 `json:"rss,omitempty"`
	// Size is the archive-size ratio of an encode.
	Size *float64 `json:"size,omitempty"`
}

// Report is report.json.
type Report struct {
	SchemaVersion int                       `json:"schema_version"`
	Schema        string                    `json:"schema"`
	Orientation   string                    `json:"orientation"`
	StartedUTC    string                    `json:"started_utc"`
	FinishedUTC   string                    `json:"finished_utc"`
	Machine       host.Machine              `json:"machine"`
	Toolchain     toolchain.Toolchain       `json:"toolchain"`
	Profile       string                    `json:"profile"`
	Quick         bool                      `json:"quick"`
	Warmups       int                       `json:"warmups"`
	Repeats       int                       `json:"repeats"`
	Scenarios     []suite.Scenario          `json:"scenarios"`
	Rows          []Row                     `json:"rows"`
	Ratios        []Ratio                   `json:"ratios"`
	RSS           []procmeasure.RSSScenario `json:"rss_scenarios"`
	// Failures lists every failed or unfinished candidate/reference run;
	// SecondaryFailures the sevenz-rust2 ones, which do not fail the run.
	Failures          []string `json:"failures"`
	SecondaryFailures []string `json:"secondary_failures"`
}

// Build summarises raw runs.
func Build(raw *suite.Raw) *Report {
	report := &Report{
		SchemaVersion: 1, Schema: Schema, Orientation: Orientation,
		StartedUTC: raw.StartedUTC, FinishedUTC: raw.FinishedUTC, Machine: raw.Machine, Toolchain: raw.Toolchain,
		Quick: raw.Quick, Warmups: raw.Warmups, Repeats: raw.Repeats, Scenarios: raw.Scenarios,
		Failures: []string{}, SecondaryFailures: []string{},
	}
	if raw.Fixtures != nil {
		report.Profile = raw.Fixtures.Profile
	}
	type key struct{ scenario, variant string }
	grouped := map[key][]suite.RunRecord{}
	for _, run := range raw.Runs {
		if run.Status != suite.StatusOK {
			line := fmt.Sprintf("%s / %s (repeat %d%s): %s %s", run.Scenario, run.Variant, run.Repeat, warmupTag(run.Warmup), run.Failure, firstNonEmpty(run.Error, run.StderrLine))
			if run.Role == suite.RoleSecondary {
				report.SecondaryFailures = append(report.SecondaryFailures, line)
			} else {
				report.Failures = append(report.Failures, line)
			}
		}
		if run.Warmup && run.Status == suite.StatusOK {
			continue
		}
		k := key{run.Scenario, run.Variant}
		grouped[k] = append(grouped[k], run)
	}
	for _, scenario := range raw.Scenarios {
		byVariant := map[string]*Row{}
		for _, variant := range scenario.Variants {
			runs := grouped[key{scenario.ID, variant.Variant}]
			if len(runs) == 0 {
				continue
			}
			row := summarize(scenario, variant, runs)
			report.Rows = append(report.Rows, row)
			byVariant[variant.Variant] = &row
		}
		oracle := byVariant[suite.VariantOracle]
		for _, variant := range []string{suite.VariantTurbo, suite.VariantTurboNative} {
			ours := byVariant[variant]
			if ours == nil {
				continue
			}
			if oracle != nil && ours.OK > 0 && oracle.OK > 0 {
				ratio := Ratio{Scenario: scenario.ID, Group: scenario.Group, Variant: variant,
					Wall: divide(oracle.Wall.Median, ours.Wall.Median), CPU: divide(oracle.CPU.Median, ours.CPU.Median),
					RSS: divide(oracle.RSS.Median, ours.RSS.Median)}
				if scenario.Op == suite.OpEncode {
					ratio.Size = divide(float64(oracle.BytesOut), float64(ours.BytesOut))
				}
				report.Ratios = append(report.Ratios, ratio)
			}
			if ours.OK > 0 {
				rss := procmeasure.RSSScenario{Scenario: scenario.ID, CandidateSource: ours.RSSSource,
					CandidateMedianBytes: int64(ours.RSS.Median), CandidateMinBytes: int64(ours.RSS.Min), CandidateMaxBytes: int64(ours.RSS.Max)}
				rss.Variant = variant
				if oracle != nil && oracle.OK > 0 {
					rss.ReferenceMedianBytes, rss.ReferenceMinBytes, rss.ReferenceMaxBytes = int64(oracle.RSS.Median), int64(oracle.RSS.Min), int64(oracle.RSS.Max)
					rss.ReferenceSource = oracle.RSSSource
				}
				procmeasure.CompleteRSSScenario(&rss)
				report.RSS = append(report.RSS, rss)
			}
		}
	}
	procmeasure.SortRSSScenarios(report.RSS)
	return report
}

func warmupTag(warmup bool) string {
	if warmup {
		return ", warmup"
	}
	return ""
}

func firstNonEmpty(values ...string) string {
	for _, value := range values {
		if value != "" {
			return value
		}
	}
	return ""
}

func divide(a, b float64) *float64 {
	if a <= 0 || b <= 0 {
		return nil
	}
	value := a / b
	return &value
}

func summarize(scenario suite.Scenario, variant suite.Run, runs []suite.RunRecord) Row {
	row := Row{Scenario: scenario.ID, Group: scenario.Group, Op: scenario.Op, Variant: variant.Variant, Role: variant.Role}
	var wall, cpu, rss, load, out, parse []float64
	var sources []string
	extras := map[string]bool{}
	var extraOrder []string
	for _, run := range runs {
		if run.Status != suite.StatusOK {
			row.Failed++
			if run.Status == suite.StatusDNF {
				row.DNF = run.Failure
			}
			row.Failures = append(row.Failures, run.Failure)
			continue
		}
		row.OK++
		wall = append(wall, run.WallSeconds)
		cpu = append(cpu, run.UserSeconds+run.SysSeconds)
		rss = append(rss, float64(run.MaxRSSBytes))
		if run.LoadAverage >= 0 {
			load = append(load, run.LoadAverage)
		}
		out = append(out, float64(run.BytesOut))
		if seconds, ok := run.Result["parse_seconds"].(float64); ok {
			parse = append(parse, seconds)
		}
		sources = append(sources, run.RSSSource)
		row.BytesIn = run.BytesIn
		if extra := describe(scenario, run.Result); extra != "" && !extras[extra] {
			extras[extra] = true
			extraOrder = append(extraOrder, extra)
		}
		if run.Verified != "" {
			if extra := "7zz t: " + run.Verified; !extras[extra] {
				extras[extra] = true
				extraOrder = append(extraOrder, extra)
			}
		}
	}
	row.Wall, row.CPU, row.RSS, row.Load = stat(wall), stat(cpu), stat(rss), stat(load)
	row.BytesOut = int64(stat(out).Median)
	row.RSSSource = procmeasure.JoinSources(sources)
	if len(parse) > 0 {
		extraOrder = append(extraOrder, fmt.Sprintf("header parse %.4fs", stat(parse).Median))
	}
	row.Extra = strings.Join(extraOrder, "; ")
	if row.Wall.Median > 0 {
		bytes := row.BytesOut
		if scenario.Op == suite.OpEncode {
			bytes = row.BytesIn
		}
		if bytes > 0 {
			row.ThroughputMiBs = float64(bytes) / (1 << 20) / row.Wall.Median
		}
	}
	return row
}

func describe(scenario suite.Scenario, result map[string]any) string {
	if result == nil {
		return ""
	}
	var parts []string
	switch scenario.Op {
	case suite.OpDecode:
		if path, ok := result["parallel_path"].(bool); ok {
			parts = append(parts, fmt.Sprintf("parallel_path=%t", path))
			if spawned, ok := result["max_spawned_threads"].(float64); ok && path {
				parts = append(parts, fmt.Sprintf("spawned=%d", int(spawned)))
			}
		}
	case suite.OpList:
		if memory, ok := result["decoder_memory_estimate"].(float64); ok && memory > 0 {
			parts = append(parts, fmt.Sprintf("decoder_memory_estimate=%s MiB", procmeasure.MiB(int64(memory))))
		}
	case suite.OpEncode:
		if block, ok := result["block_size"].(float64); ok && block > 0 {
			parts = append(parts, fmt.Sprintf("block=%s MiB", procmeasure.MiB(int64(block))))
		}
	}
	if backend, ok := result["crypto_backend"].(string); ok && scenario.Encrypted {
		parts = append(parts, "crypto="+backend)
	}
	return strings.Join(parts, " ")
}

// Write saves report.json.
func Write(path string, report *Report) error {
	data, err := json.MarshalIndent(report, "", "  ")
	if err != nil {
		return err
	}
	return os.WriteFile(path, append(data, '\n'), 0o644)
}

// Load reads a report.json.
func Load(path string) (*Report, error) {
	data, err := os.ReadFile(path)
	if err != nil {
		return nil, err
	}
	var report Report
	if err := json.Unmarshal(data, &report); err != nil {
		return nil, fmt.Errorf("%s: %w", path, err)
	}
	if report.SchemaVersion != 1 || report.Schema != Schema {
		return nil, fmt.Errorf("%s: schema %q version %d, want %q version 1", path, report.Schema, report.SchemaVersion, Schema)
	}
	return &report, nil
}
