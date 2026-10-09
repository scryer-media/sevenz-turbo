package report

import (
	"fmt"
	"sort"
	"strings"

	"github.com/scryer-media/sevenz-turbo/bench/sevenz-turbo-bench/internal/procmeasure"
	"github.com/scryer-media/sevenz-turbo/bench/sevenz-turbo-bench/internal/suite"
)

func seconds(s Stat) string {
	if s.N == 0 {
		return "-"
	}
	return fmt.Sprintf("%.3f [%.3f–%.3f]", s.Median, s.Min, s.Max)
}

func ratioText(value *float64) string {
	if value == nil {
		return "-"
	}
	return fmt.Sprintf("%.3f", *value)
}

// primaryReference reports whether a ratio is against 7zz as every row runs
// it; a report written before ratios named their reference has only those.
func primaryReference(ratio Ratio) bool {
	return ratio.Reference == "" || ratio.Reference == suite.VariantOracle
}

// ratioLabel names a ratio's candidate, and its reference when that is not
// the primary one.
func ratioLabel(ratio Ratio) string {
	if primaryReference(ratio) {
		return ratio.Variant
	}
	return ratio.Variant + " vs " + ratio.Reference
}

// throughputText gives a throughput in MiB/s to three significant figures, so
// a slow row reads as what it is rather than rounding to 0.
func throughputText(mibs float64) string {
	switch {
	case mibs >= 100:
		return fmt.Sprintf("%.0f", mibs)
	case mibs >= 10:
		return fmt.Sprintf("%.1f", mibs)
	case mibs >= 1:
		return fmt.Sprintf("%.2f", mibs)
	default:
		return fmt.Sprintf("%.3g", mibs)
	}
}

func mib(bytes int64) string {
	if bytes <= 0 {
		return "-"
	}
	return procmeasure.MiB(bytes)
}

// Markdown renders one host's report.md.
func Markdown(report *Report) string {
	var b strings.Builder
	m := report.Machine
	fmt.Fprintf(&b, "# sevenz-turbo bench: %s\n\n", m.Label)
	fmt.Fprintf(&b, "%s\n\n", Orientation)
	fmt.Fprintf(&b, "Each cell is the median [min–max] over %d measured runs (%d warmups discarded); variants are interleaved, their order reversed every repeat. Every run is its own process: wall time from the harness's clock, CPU (user+sys) and peak RSS from the kernel's accounting of the exited child.\n\n", report.Repeats, report.Warmups)
	fmt.Fprintln(&b, "## Host")
	fmt.Fprintln(&b)
	fmt.Fprintf(&b, "- label: %s\n", m.Label)
	if m.InstanceType != "" {
		fmt.Fprintf(&b, "- instance type: %s\n", m.InstanceType)
	}
	fmt.Fprintf(&b, "- OS / arch: %s / %s (%s)\n", m.OS, m.Architecture, m.Kernel)
	fmt.Fprintf(&b, "- CPU: %s, %d logical cores\n", m.CPU, m.CPUCount)
	if report.PinCPUs != "" {
		fmt.Fprintf(&b, "- CPU pinning: every measured process confined to CPUs %s\n", report.PinCPUs)
	}
	if m.MemoryBytes > 0 {
		fmt.Fprintf(&b, "- memory: %.1f GiB\n", float64(m.MemoryBytes)/(1<<30))
	}
	fmt.Fprintf(&b, "- ISA: %s (from %s)\n", strings.Join(m.ISA, " "), strings.Join(m.ISASources, ", "))
	if report.RunProfile != "" {
		only := ""
		if len(report.Only) > 0 {
			only = ", only " + strings.Join(report.Only, ",")
		}
		fmt.Fprintf(&b, "- run: %s to %s, run profile %s over the %s corpus%s, %d repeat(s), %d warmup(s)\n\n", report.StartedUTC,
			report.FinishedUTC, report.RunProfile, report.Profile, only, report.Repeats, report.Warmups)
	} else {
		fmt.Fprintf(&b, "- run: %s to %s, profile %s, quick=%t\n\n", report.StartedUTC, report.FinishedUTC, report.Profile, report.Quick)
	}
	t := report.Toolchain
	fmt.Fprintln(&b, "## Toolchain")
	fmt.Fprintln(&b)
	fmt.Fprintf(&b, "- linked lzma-turbo: %s\n", t.LinkedLzmaTurbo)
	for _, candidate := range t.Candidates {
		fmt.Fprintf(&b, "- %s: crypto %s, sevenz-rust2 %s, aws-lc-rs %s, crc-fast %s, PPMd crates %s (sha256 %s)\n", candidate.Label,
			candidate.Field("crypto_backend"), candidate.Field("sevenz_rust2"), candidate.Field("aws_lc_rs"),
			candidate.Field("crc_fast"), candidate.PPMdCrates(), short(candidate.SHA256))
	}
	fmt.Fprintf(&b, "- 7zz: %s (sha256 %s); provenance: %s; official: %t\n", t.Oracle.Banner, short(t.Oracle.SHA256), t.Oracle.Provenance, t.Oracle.Official)
	dirty := ""
	if t.Rust.Dirty {
		dirty = " (dirty)"
	}
	fmt.Fprintf(&b, "- rustc: %s; crate commit %s%s; Cargo.lock %s\n", t.Rust.Rustc, short(t.Rust.Commit), dirty, short(t.Rust.CargoLock))
	if t.Rust.Note != "" {
		fmt.Fprintf(&b, "- note: %s\n", t.Rust.Note)
	}
	fmt.Fprintln(&b)

	if len(report.Failures) > 0 {
		fmt.Fprintln(&b, "## Failures")
		fmt.Fprintln(&b)
		for _, failure := range report.Failures {
			fmt.Fprintf(&b, "- %s\n", failure)
		}
		fmt.Fprintln(&b)
	}

	// ratios holds each candidate's ratio against 7zz; others its ratios
	// against a second reference, printed on lines of their own that name it.
	ratios := map[string]map[string]Ratio{}
	others := map[string]map[string][]Ratio{}
	for _, ratio := range report.Ratios {
		if !primaryReference(ratio) {
			if others[ratio.Scenario] == nil {
				others[ratio.Scenario] = map[string][]Ratio{}
			}
			others[ratio.Scenario][ratio.Variant] = append(others[ratio.Scenario][ratio.Variant], ratio)
			continue
		}
		if ratios[ratio.Scenario] == nil {
			ratios[ratio.Scenario] = map[string]Ratio{}
		}
		ratios[ratio.Scenario][ratio.Variant] = ratio
	}
	notes := map[string]string{}
	for _, scenario := range report.Scenarios {
		notes[scenario.ID] = scenario.Note
	}
	for _, group := range suite.Groups {
		var rows []Row
		for _, row := range report.Rows {
			if row.Group == group {
				rows = append(rows, row)
			}
		}
		if len(rows) == 0 {
			continue
		}
		fmt.Fprintf(&b, "## %s\n\n", group)
		encode := rows[0].Op == suite.OpEncode
		header := "| scenario | variant | wall s | CPU s | peak RSS MiB | MiB/s |"
		rule := "|---|---|---|---|---|---|"
		if encode {
			header += " archive MiB | size ratio |"
			rule += "---|---|"
		}
		fmt.Fprintln(&b, header+" wall ratio | CPU ratio | RSS ratio | load | notes |")
		fmt.Fprintln(&b, rule+"---|---|---|---|---|")
		var groupNotes []string
		lastScenario := ""
		for _, row := range rows {
			ratio, hasRatio := ratios[row.Scenario][row.Variant]
			name := ""
			if row.Scenario != lastScenario {
				name = row.Scenario
				lastScenario = row.Scenario
				if note := notes[row.Scenario]; note != "" {
					groupNotes = append(groupNotes, fmt.Sprintf("`%s`: %s", row.Scenario, note))
				}
			}
			throughput := "-"
			if row.ThroughputMiBs > 0 {
				throughput = throughputText(row.ThroughputMiBs)
			}
			line := fmt.Sprintf("| %s | %s | %s | %s | %s | %s |", name, variantLabel(row), seconds(row.Wall), seconds(row.CPU),
				procmeasure.MiBRange(int64(row.RSS.Median), int64(row.RSS.Min), int64(row.RSS.Max)), throughput)
			if encode {
				size := "-"
				if hasRatio {
					size = ratioText(ratio.Size)
				}
				line += fmt.Sprintf(" %s | %s |", mib(row.BytesOut), size)
			}
			wall, cpu, rss := "-", "-", "-"
			if hasRatio {
				wall, cpu, rss = ratioText(ratio.Wall), ratioText(ratio.CPU), ratioText(ratio.RSS)
			}
			load := "-"
			if row.Load.N > 0 {
				load = fmt.Sprintf("%.2f", row.Load.Median)
			}
			extra := row.Extra
			if row.Failed > 0 {
				extra = strings.TrimSpace(fmt.Sprintf("%s FAILED %d/%d: %s", extra, row.Failed, row.Failed+row.OK, strings.Join(dedupe(row.Failures), ",")))
			}
			line += fmt.Sprintf(" %s | %s | %s | %s | %s |", wall, cpu, rss, load, dash(extra))
			fmt.Fprintln(&b, line)
			for _, other := range others[row.Scenario][row.Variant] {
				line := fmt.Sprintf("| | %s | - | - | - | - |", ratioLabel(other))
				if encode {
					line += fmt.Sprintf(" - | %s |", ratioText(other.Size))
				}
				line += fmt.Sprintf(" %s | %s | %s | - | - |", ratioText(other.Wall), ratioText(other.CPU), ratioText(other.RSS))
				fmt.Fprintln(&b, line)
			}
		}
		fmt.Fprintln(&b)
		for _, note := range groupNotes {
			fmt.Fprintf(&b, "- %s\n", note)
		}
		if len(groupNotes) > 0 {
			fmt.Fprintln(&b)
		}
	}
	renderLedgers(&b, report.Ledgers)
	procmeasure.RenderRSSSummary(&b, report.RSS)
	if len(report.SecondaryFailures) > 0 {
		fmt.Fprintln(&b, "## Secondary reference failures")
		fmt.Fprintln(&b)
		fmt.Fprintln(&b, "sevenz-rust2 runs that failed; they are informational and do not fail the run.")
		fmt.Fprintln(&b)
		for _, failure := range dedupe(report.SecondaryFailures) {
			fmt.Fprintf(&b, "- %s\n", failure)
		}
		fmt.Fprintln(&b)
	}
	return b.String()
}

func variantLabel(row Row) string {
	if row.Role == suite.RoleSecondary {
		return row.Variant + " (secondary)"
	}
	return row.Variant
}

func dedupe(values []string) []string {
	seen := map[string]bool{}
	var out []string
	for _, value := range values {
		if !seen[value] {
			seen[value] = true
			out = append(out, value)
		}
	}
	return out
}

func dash(value string) string {
	if value == "" {
		return "-"
	}
	return value
}

func short(digest string) string {
	if len(digest) > 12 {
		return digest[:12]
	}
	return digest
}

// Merge renders a cross-host report.md from several hosts' reports: per
// scenario, each host's sevenz-turbo median wall time and its wall and RSS
// ratios against that host's 7zz, plus encode size ratios. Reports that did
// not measure the same workload (see Comparable) are refused.
func Merge(reports []*Report) (string, error) {
	if err := Comparable(reports); err != nil {
		return "", err
	}
	var b strings.Builder
	fmt.Fprintln(&b, "# sevenz-turbo bench: cross-host summary")
	fmt.Fprintln(&b)
	fmt.Fprintln(&b, Orientation)
	fmt.Fprintln(&b, "Each host is compared with its own 7zz; cells are `sevenz-turbo median wall s / wall ratio / RSS ratio` (encode rows add `/ size ratio`).")
	fmt.Fprintln(&b)
	fmt.Fprintln(&b, "## Hosts")
	fmt.Fprintln(&b)
	fmt.Fprintln(&b, "| label | instance | OS/arch | CPU | cores | pinned CPUs | ISA | lzma-turbo | 7zz | quick | failures |")
	fmt.Fprintln(&b, "|---|---|---|---|---|---|---|---|---|---|---|")
	for _, r := range reports {
		m := r.Machine
		fmt.Fprintf(&b, "| %s | %s | %s/%s | %s | %d | %s | %s | %s | %s | %t | %d |\n", m.Label, dash(m.InstanceType), m.OS, m.Architecture, m.CPU,
			m.CPUCount, dash(r.PinCPUs), strings.Join(m.ISA, " "), r.Toolchain.LinkedLzmaTurbo, r.Toolchain.Oracle.Version, r.Quick, len(r.Failures))
	}
	fmt.Fprintln(&b)

	type cellKey struct {
		host              int
		scenario, variant string
	}
	cells := map[cellKey]string{}
	groupOf := map[string]string{}
	var order []string
	seen := map[string]bool{}
	for index, r := range reports {
		walls := map[string]Stat{}
		for _, row := range r.Rows {
			walls[row.Scenario+"\x00"+row.Variant] = row.Wall
		}
		for _, ratio := range r.Ratios {
			key := ratio.Scenario + "\x00" + ratioLabel(ratio)
			if !seen[key] {
				seen[key] = true
				order = append(order, key)
				groupOf[key] = ratio.Group
			}
			cell := fmt.Sprintf("%.3f / %s / %s", walls[ratio.Scenario+"\x00"+ratio.Variant].Median, ratioText(ratio.Wall), ratioText(ratio.RSS))
			if ratio.Size != nil {
				cell += " / " + ratioText(ratio.Size)
			}
			cells[cellKey{index, ratio.Scenario, ratioLabel(ratio)}] = cell
		}
	}
	groupIndex := map[string]int{}
	for i, group := range suite.Groups {
		groupIndex[group] = i
	}
	sort.SliceStable(order, func(i, j int) bool { return groupIndex[groupOf[order[i]]] < groupIndex[groupOf[order[j]]] })
	currentGroup := ""
	for _, key := range order {
		group := groupOf[key]
		if group != currentGroup {
			currentGroup = group
			fmt.Fprintf(&b, "## %s\n\n", group)
			header, rule := "| scenario | variant |", "|---|---|"
			for _, r := range reports {
				header += " " + r.Machine.Label + " |"
				rule += "---|"
			}
			fmt.Fprintln(&b, header)
			fmt.Fprintln(&b, rule)
		}
		scenario, variant, _ := strings.Cut(key, "\x00")
		line := fmt.Sprintf("| %s | %s |", scenario, variant)
		for index := range reports {
			line += " " + dash(cells[cellKey{index, scenario, variant}]) + " |"
		}
		fmt.Fprintln(&b, line)
		if next := nextGroup(order, key, groupOf); next != group {
			fmt.Fprintln(&b)
		}
	}

	fmt.Fprintln(&b, "## Worst peak RSS ratio per host")
	fmt.Fprintln(&b)
	fmt.Fprintln(&b, "| host | scenario | variant | sevenz-turbo MiB | 7zz MiB | RSS ratio |")
	fmt.Fprintln(&b, "|---|---|---|---|---|---|")
	for _, r := range reports {
		for i, rss := range r.RSS {
			if i == 5 || rss.Ratio == nil {
				break
			}
			fmt.Fprintf(&b, "| %s | %s | %s | %s | %s | %.3f |\n", r.Machine.Label, rss.Scenario, dash(rss.Variant),
				procmeasure.MiB(rss.CandidateMedianBytes), procmeasure.MiB(rss.ReferenceMedianBytes), *rss.Ratio)
		}
	}
	fmt.Fprintln(&b)
	for _, r := range reports {
		if len(r.Failures) > 0 {
			fmt.Fprintf(&b, "## Failures on %s\n\n", r.Machine.Label)
			for _, failure := range r.Failures {
				fmt.Fprintf(&b, "- %s\n", failure)
			}
			fmt.Fprintln(&b)
		}
	}
	return b.String(), nil
}

func nextGroup(order []string, key string, groupOf map[string]string) string {
	for i, k := range order {
		if k == key && i+1 < len(order) {
			return groupOf[order[i+1]]
		}
	}
	return ""
}
