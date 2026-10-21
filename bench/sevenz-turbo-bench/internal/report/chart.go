package report

import (
	"bytes"
	"fmt"
	"math"
	"os"
	"path/filepath"
	"sort"
	"strings"

	"github.com/scryer-media/sevenz-turbo/bench/sevenz-turbo-bench/internal/suite"
)

// ChartInput is one host's report.json and the name its figures carry on
// the charts; an empty Name uses the machine label.
type ChartInput struct {
	Name   string
	Report *Report
}

const (
	chartWidth  = 960
	chartTop    = 92
	chartToolH  = 28 // a tool's wall-time bar, then its CPU-time and peak-RSS bars
	chartGap    = 10
	chartAxisX0 = 300.0
	chartParity = 610.0
	chartAxisX1 = 920.0
)

// chartExcluded reports whether a scenario stays off the charts. The memory
// budget and adaptive rows are by design (7zz has neither), and the decode
// ledger group repeats lzma2 decodes already charted, with the ledger kept.
func chartExcluded(scenario suite.Scenario) bool {
	return scenario.Group == "memory budget" || scenario.Group == "decode ledger" ||
		scenario.Adaptive || scenario.MemoryLimit != 0
}

// chartCategory is the bar group a scenario is averaged into: the kind of
// work, split into one thread and several. File trees split into solid and
// non-solid too, since sevenz-turbo's per-file parallelism sets the two far
// apart, and extraction streamed through one writer is a row of its own,
// whatever the payload, since it cannot write files in parallel.
func chartCategory(scenario suite.Scenario) string {
	var name string
	switch {
	case scenario.Group == "container parse":
		return "Listing"
	case strings.HasSuffix(scenario.ID, "/stream"):
		name = "Streamed extract"
	default:
		name = chartGroupName(scenario)
	}
	if scenario.Threads == "1" {
		return name + ", 1 thread"
	}
	return name + ", multi-thread"
}

func chartGroupName(scenario suite.Scenario) string {
	var name string
	switch scenario.Group {
	case "lzma2 single-stream", "lzma2 parallel", "lzma2 near-incompressible":
		name = "LZMA2"
	case "lzma":
		name = "LZMA"
	case "solid vs non-solid":
		name = chartTreeName(scenario)
	case "stored":
		name = "Stored"
	case "aes-256", "encode aes-256":
		name = "AES-256"
	case "filters":
		name = "BCJ / BCJ2 / delta"
	case "ppmd (secondary)":
		name = "PPMd"
	case "encode bcj2":
		name = "BCJ2"
	case "encode":
		if strings.HasPrefix(scenario.ID, "encode/payload-sub/") {
			name = "LZMA2"
		} else {
			name = chartTreeName(scenario)
		}
	default:
		name = scenario.Group
	}
	return name
}

func chartTreeName(scenario suite.Scenario) string {
	if strings.Contains(scenario.ID, "nonsolid") || strings.Contains(scenario.ID, "non-solid") {
		return "Non-solid trees"
	}
	return "Solid trees"
}

// chartOp is the chart a scenario belongs on: list rows sit with decode.
func chartOp(scenario suite.Scenario) string {
	if scenario.Op == suite.OpEncode {
		return suite.OpEncode
	}
	return suite.OpDecode
}

// chartSeries is one tool's bars in one category: reference / tool for wall
// time, CPU time and peak RSS, each a geometric mean over the category's scenarios
// on each host and then across hosts, so every host weighs the same.
type chartSeries struct {
	tool      string
	scenarios int
	mean      float64
	cpuMean   float64
	rssMean   float64
	// multiThread marks a series that takes in a several-thread scenario;
	// sevenz-rust2's is asterisked on the decode chart.
	multiThread bool
}

type chartBar struct {
	category string
	series   []chartSeries
}

// RenderCharts writes sevenz-turbo-decode.svg and sevenz-turbo-encode.svg
// into out and returns their paths. Each bar is 7zz's median over the
// tool's median (the scenario's parity reference, as in
// report.md), as a geometric mean over the category's scenarios on each
// host and then across hosts; sevenz-rust2 has a bar wherever it ran. Every
// host must have run the same scenarios with the same versions. The output
// is a pure function of the inputs: no timestamps or local paths. Scenarios
// some host did not run are left off every chart and returned as skipped.
func RenderCharts(inputs []ChartInput, out string) (paths, skipped []string, err error) {
	if len(inputs) == 0 {
		return nil, nil, fmt.Errorf("no report to chart")
	}
	reports := make([]*Report, len(inputs))
	for i, input := range inputs {
		if input.Report == nil {
			return nil, nil, fmt.Errorf("input %q has no report", input.Name)
		}
		reports[i] = input.Report
	}
	if err := Comparable(reports); err != nil {
		return nil, nil, err
	}
	common, skipped := commonScenarios(inputs)
	if err := os.MkdirAll(out, 0o755); err != nil {
		return nil, nil, err
	}
	for _, op := range []string{suite.OpDecode, suite.OpEncode} {
		bars := chartBars(inputs, common, op)
		if len(bars) == 0 {
			continue
		}
		path := filepath.Join(out, "sevenz-turbo-"+op+".svg")
		if err := os.WriteFile(path, renderChart(inputs, op, bars), 0o644); err != nil {
			return paths, skipped, err
		}
		paths = append(paths, path)
	}
	return paths, skipped, nil
}

// commonScenarios is the IDs every input ran, and in first-input order the
// ones some input did not: hosts of different sizes plan different thread
// rows, and a group's mean across hosts is one comparison only over rows
// every host has.
func commonScenarios(inputs []ChartInput) (map[string]bool, []string) {
	count := map[string]int{}
	var order []string
	for _, input := range inputs {
		for _, scenario := range input.Report.Scenarios {
			if count[scenario.ID] == 0 {
				order = append(order, scenario.ID)
			}
			count[scenario.ID]++
		}
	}
	common := map[string]bool{}
	var skipped []string
	for _, id := range order {
		if count[id] == len(inputs) {
			common[id] = true
		} else {
			skipped = append(skipped, id)
		}
	}
	return common, skipped
}

func chartInputName(input ChartInput) string {
	if input.Name != "" {
		return input.Name
	}
	return firstNonEmpty(input.Report.Machine.Label, input.Report.Machine.CPU)
}

// chartBars is every category of op over the common scenarios, headline
// first, with a bar per tool that has a measured ratio on every host.
func chartBars(inputs []ChartInput, common map[string]bool, op string) []chartBar {
	type key struct{ category, tool string }
	// logs[key][host] are the log ratios of that host's scenarios, and
	// cpuLogs and rssLogs the CPU-time and peak-RSS ones of the same
	// scenarios.
	logs := map[key][][]float64{}
	cpuLogs := map[key][][]float64{}
	rssLogs := map[key][][]float64{}
	multiThread := map[key]bool{}
	var order []string
	seen := map[string]bool{}
	for h, input := range inputs {
		rows := map[[2]string]*Row{}
		for i := range input.Report.Rows {
			row := &input.Report.Rows[i]
			if row.OK > 0 {
				rows[[2]string{row.Scenario, row.Variant}] = row
			}
		}
		for _, scenario := range input.Report.Scenarios {
			if !common[scenario.ID] {
				continue
			}
			var names []string
			if chartHeadline(scenario, op) {
				names = append(names, chartOverall(op))
			}
			if !chartExcluded(scenario) && chartOp(scenario) == op {
				names = append(names, chartCategory(scenario))
			}
			if len(names) == 0 {
				continue
			}
			for _, name := range names {
				if !seen[name] {
					seen[name] = true
					order = append(order, name)
				}
			}
			reference := rows[[2]string{scenario.ID, parityReference(scenario)}]
			if reference == nil {
				continue
			}
			for _, tool := range []string{suite.VariantTurbo, suite.VariantUpstream} {
				row := rows[[2]string{scenario.ID, tool}]
				if row == nil {
					continue
				}
				ratio := divide(reference.Wall.Median, row.Wall.Median)
				cpu := divide(reference.CPU.Median, row.CPU.Median)
				rss := divide(reference.RSS.Median, row.RSS.Median)
				if ratio == nil || cpu == nil || rss == nil {
					continue
				}
				for _, name := range names {
					k := key{name, tool}
					if logs[k] == nil {
						logs[k] = make([][]float64, len(inputs))
						cpuLogs[k] = make([][]float64, len(inputs))
						rssLogs[k] = make([][]float64, len(inputs))
					}
					logs[k][h] = append(logs[k][h], math.Log(*ratio))
					cpuLogs[k][h] = append(cpuLogs[k][h], math.Log(*cpu))
					rssLogs[k][h] = append(rssLogs[k][h], math.Log(*rss))
					multiThread[k] = multiThread[k] || scenario.Threads != "1"
				}
			}
		}
	}
	order = headlineFirst(order, chartOverall(op))
	var bars []chartBar
	for _, category := range order {
		bar := chartBar{category: category}
		for _, tool := range []string{suite.VariantTurbo, suite.VariantUpstream} {
			perHost := logs[key{category, tool}]
			if perHost == nil {
				continue
			}
			series := chartSeries{tool: tool, scenarios: len(perHost[0]), multiThread: multiThread[key{category, tool}]}
			var complete bool
			if series.mean, complete = hostMeans(perHost, series.scenarios); !complete {
				continue
			}
			series.cpuMean, _ = hostMeans(cpuLogs[key{category, tool}], series.scenarios)
			series.rssMean, _ = hostMeans(rssLogs[key{category, tool}], series.scenarios)
			bar.series = append(bar.series, series)
		}
		if len(bar.series) != 0 {
			bars = append(bars, bar)
		}
	}
	// Groups best first by sevenz-turbo's bar, the headline staying on top.
	if len(bars) > 1 && bars[0].category == chartOverall(op) {
		rest := bars[1:]
		sort.SliceStable(rest, func(i, j int) bool { return turboMean(rest[i]) > turboMean(rest[j]) })
	}
	return bars
}

// hostMeans is the geometric mean of each host's log ratios, averaged
// across hosts; not complete unless every host has exactly n of them.
func hostMeans(perHost [][]float64, n int) (float64, bool) {
	var total float64
	for _, values := range perHost {
		if len(values) == 0 || len(values) != n {
			return 0, false
		}
		var sum float64
		for _, value := range values {
			sum += value
		}
		total += sum / float64(n)
	}
	return math.Exp(total / float64(len(perHost))), len(perHost) != 0
}

func turboMean(bar chartBar) float64 {
	for _, series := range bar.series {
		if series.tool == suite.VariantTurbo {
			return series.mean
		}
	}
	return 0
}

// chartHeadline reports whether a scenario is in op's headline group,
// which is the benchmarks page's summary: every decode (ledger rows too) or
// encode row but the memory-budget and adaptive ones.
func chartHeadline(scenario suite.Scenario, op string) bool {
	if scenario.Group == "memory budget" || scenario.Adaptive || scenario.MemoryLimit != 0 {
		return false
	}
	if op == suite.OpEncode {
		return scenario.Op == suite.OpEncode
	}
	return scenario.Op == suite.OpDecode
}

// chartOverall is op's headline group.
func chartOverall(op string) string {
	return "All " + op
}

// toolLabel is "sevenz-turbo 0.29.1" from the primary candidate's own
// version report.
func toolLabel(r *Report, tool string) string {
	for _, candidate := range r.Toolchain.Candidates {
		if candidate.Label != suite.VariantTurbo {
			continue
		}
		field := "sevenz_turbo"
		if tool == suite.VariantUpstream {
			field = "sevenz_rust2"
		}
		if version, ok := candidate.Version[field].(string); ok && version != "" {
			return tool + " " + version
		}
	}
	return tool
}

func renderChart(inputs []ChartInput, op string, bars []chartBar) []byte {
	first := inputs[0].Report
	oracle := "7-Zip " + first.Toolchain.Oracle.Version
	var hostNames []string
	for _, input := range inputs {
		hostNames = append(hostNames, chartInputName(input))
	}
	turbo := toolLabel(first, suite.VariantTurbo)
	upstream := toolLabel(first, suite.VariantUpstream)
	title := fmt.Sprintf("sevenz-turbo %s vs %s", op, oracle)
	subtitle := fmt.Sprintf("Geometric mean over %s, weighted equally. %s is the 1x line.", strings.Join(hostNames, " and "), oracle)
	description := fmt.Sprintf("%s %s wall time, CPU time and peak memory against %s, grouped by kind of work and thread count. Each thick bar is %s's median wall time over the tool's, and the two thinner bars under it the same for CPU time and peak RSS (labelled as 7-Zip's CPU time over the tool's, and as the tool's peak RSS over 7-Zip's), as a geometric mean of the group's scenarios on each host and then across %d hosts (%s), on a symmetric log scale: right of the line the tool is better. Memory-budget, adaptive and decode-ledger rows are left out.",
		turbo, op, oracle, oracle, len(inputs), strings.Join(hostNames, ", "))
	hasUpstream, asterisk := false, false
	height := chartTop + 48
	for _, bar := range bars {
		height += len(bar.series)*chartToolH + chartGap
		for _, series := range bar.series {
			hasUpstream = hasUpstream || series.tool == suite.VariantUpstream
			asterisk = asterisk || op == suite.OpDecode && series.tool == suite.VariantUpstream && series.multiThread
		}
	}
	if asterisk {
		height += 18
	}

	var b bytes.Buffer
	fmt.Fprintf(&b, "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"%d\" height=\"%d\" viewBox=\"0 0 %d %d\" role=\"img\" aria-labelledby=\"title desc\">\n", chartWidth, height, chartWidth, height)
	fmt.Fprintf(&b, "  <title id=\"title\">%s</title>\n  <desc id=\"desc\">%s</desc>\n", escapeXML(title), escapeXML(description))
	b.WriteString(chartStyle)
	var digests []string
	for _, input := range inputs {
		var sha string
		if len(input.Report.Toolchain.Candidates) != 0 {
			sha = input.Report.Toolchain.Candidates[0].SHA256
		}
		digests = append(digests, fmt.Sprintf("%s:%s:%s:%d/%d", chartInputName(input), input.Report.Machine.Label, sha, input.Report.Warmups, input.Report.Repeats))
	}
	fmt.Fprintf(&b, "  <metadata>%s</metadata>\n", escapeXML(fmt.Sprintf("schema=%s; op=%s; reference=%s; inputs=%s", Schema, op, oracle, strings.Join(digests, ", "))))
	fmt.Fprintf(&b, "  <rect class=\"bg\" width=\"%d\" height=\"%d\"/>\n", chartWidth, height)
	fmt.Fprintf(&b, "  <text class=\"title\" x=\"32\" y=\"38\">%s</text>\n", escapeXML(title))
	fmt.Fprintf(&b, "  <text class=\"subtitle\" x=\"32\" y=\"60\">%s</text>\n", escapeXML(subtitle))
	// Legend: one swatch per tool, and the thin bar for memory.
	b.WriteString("  <g aria-label=\"Legend\">")
	x := 32
	legend := func(class string, h int, text string) {
		fmt.Fprintf(&b, "<rect class=\"%s\" x=\"%d\" y=\"%d\" width=\"14\" height=\"%d\" rx=\"1\"/><text class=\"legend\" x=\"%d\" y=\"80\">%s</text>", class, x, 76-h/2, h, x+20, escapeXML(text))
		x += 20 + textWidth(text, 6.6) + 22
	}
	legend("turbo", 10, turbo)
	if hasUpstream {
		legend("upstream", 10, upstream)
	}
	legend("turbo-cpu", 6, "CPU time, 7-Zip ÷ tool")
	legend("turbo-rss", 4, "peak RSS, tool ÷ 7-Zip")
	b.WriteString("</g>\n")

	axisBottom := height - 34
	if asterisk {
		axisBottom -= 18
	}
	b.WriteString("  <g aria-label=\"Axis\">\n")
	for _, tick := range []struct {
		ratio float64
		label string
	}{{0.125, "8x"}, {0.25, "4x"}, {0.5, "2x"}, {1, "1x"}, {2, "2x"}, {4, "4x"}, {8, "8x"}} {
		tx := chartRatioX(tick.ratio, chartParity, chartAxisX1)
		class := "grid"
		if tick.ratio == 1 {
			class = "parity"
		}
		fmt.Fprintf(&b, "    <line class=\"%s\" x1=\"%.1f\" y1=\"%d\" x2=\"%.1f\" y2=\"%d\"/><text class=\"tick\" x=\"%.1f\" y=\"%d\" text-anchor=\"middle\">%s</text>\n", class, tx, chartTop-6, tx, axisBottom, tx, axisBottom+14, tick.label)
	}
	fmt.Fprintf(&b, "    <text class=\"tick\" x=\"%.1f\" y=\"%d\" text-anchor=\"middle\">← worse</text><text class=\"tick\" x=\"%.1f\" y=\"%d\" text-anchor=\"middle\">better →</text>\n",
		(chartAxisX0+chartParity)/2, axisBottom+28, (chartParity+chartAxisX1)/2, axisBottom+28)
	b.WriteString("  </g>\n")

	b.WriteString("  <g aria-label=\"Groups\">\n")
	y := chartTop
	for i, bar := range bars {
		class := "group"
		if i == 0 {
			class = "group headline"
		}
		blockH := len(bar.series) * chartToolH
		fmt.Fprintf(&b, "    <text class=\"%s\" x=\"32\" y=\"%d\">%s</text>\n", class, y+blockH/2+4, escapeXML(bar.category))
		for _, series := range bar.series {
			timeClass, cpuClass, rssClass := "turbo", "turbo-cpu", "turbo-rss"
			mark := ""
			if series.tool == suite.VariantUpstream {
				timeClass, cpuClass, rssClass = "upstream", "upstream-cpu", "upstream-rss"
				if op == suite.OpDecode && series.multiThread {
					mark = "*"
				}
			}
			drawBar(&b, timeClass, "value", series.mean, ratioMultiplier(series.mean)+mark, y+3, 10)
			// CPU reads as the plain ratio, 0.9x left of the line; peak RSS as
			// the tool's share of 7-Zip's, 0.9x right of it.
			drawBar(&b, cpuClass, "value cpu-value", series.cpuMean, plainRatio(series.cpuMean), y+15, 5)
			drawBar(&b, rssClass, "value rss-value", series.rssMean, plainRatio(1/series.rssMean), y+22, 4)
			y += chartToolH
		}
		if i+1 < len(bars) {
			fmt.Fprintf(&b, "    <line class=\"rule\" x1=\"32\" y1=\"%d\" x2=\"%.0f\" y2=\"%d\"/>\n", y+chartGap/2, chartAxisX1, y+chartGap/2)
		}
		y += chartGap
	}
	b.WriteString("  </g>\n")
	if asterisk {
		fmt.Fprintf(&b, "  <text class=\"note\" x=\"32\" y=\"%d\">%s</text>\n", height-12,
			escapeXML("* sevenz-rust2's multi-threaded decode is essentially unimplemented."))
	}
	b.WriteString("</svg>\n")
	return b.Bytes()
}

// drawBar draws a bar of height h from the 1x line to ratio, with label
// past its end, or inside it when the bar is clamped at 8x.
func drawBar(b *bytes.Buffer, class, labelClass string, ratio float64, label string, top, h int) {
	x := chartRatioX(ratio, chartParity, chartAxisX1)
	text := escapeXML(label)
	left, width := chartParity, x-chartParity
	if ratio < 1 {
		left, width = x, chartParity-x
	}
	fmt.Fprintf(b, "    <rect class=\"%s\" x=\"%.1f\" y=\"%d\" width=\"%.1f\" height=\"%d\" rx=\"1\"/>\n", class, left, top, math.Max(width, 1), h)
	textY := top + h/2 + 4
	// Only the thick bar can carry its label inside; a thin one clamped
	// at 8x is labelled past the axis end.
	inside := h >= 10
	switch {
	case ratio > 8 && !inside:
		fmt.Fprintf(b, "    <text class=\"%s\" x=\"%.1f\" y=\"%d\">%s</text>\n", labelClass, x+4, textY, text)
	case ratio < 0.125 && !inside:
		fmt.Fprintf(b, "    <text class=\"%s\" x=\"%.1f\" y=\"%d\" text-anchor=\"end\">%s</text>\n", labelClass, x-4, textY, text)
	case ratio > 8:
		fmt.Fprintf(b, "    <text class=\"%s inbar\" x=\"%.1f\" y=\"%d\" text-anchor=\"end\">%s</text>\n", labelClass, x-4, textY, text)
	case ratio < 0.125:
		fmt.Fprintf(b, "    <text class=\"%s inbar\" x=\"%.1f\" y=\"%d\">%s</text>\n", labelClass, x+4, textY, text)
	case ratio >= 1:
		fmt.Fprintf(b, "    <text class=\"%s\" x=\"%.1f\" y=\"%d\">%s</text>\n", labelClass, math.Max(x, chartParity+1)+5, textY, text)
	default:
		fmt.Fprintf(b, "    <text class=\"%s\" x=\"%.1f\" y=\"%d\" text-anchor=\"end\">%s</text>\n", labelClass, x-5, textY, text)
	}
}

// chartRatioX places a ratio on the symmetric log axis, clamped to 8x either
// way: the same scale as rarpar's charts.
func chartRatioX(ratio, parity, end float64) float64 {
	clamped := math.Max(0.125, math.Min(8, ratio))
	return parity + math.Log2(clamped)*(end-parity)/3
}

// ratioMultiplier is the better side's multiplier, 1.25x for a ratio of 0.8
// as for 1.25: the bar's side says the direction.
func ratioMultiplier(ratio float64) string {
	m := ratio
	if m < 1 {
		m = 1 / m
	}
	if m >= 9.95 {
		return fmt.Sprintf("%.0fx", m)
	}
	return fmt.Sprintf("%.2fx", m)
}

// plainRatio is a ratio as it stands, 0.80x as 0.80x.
func plainRatio(ratio float64) string {
	if ratio >= 9.95 {
		return fmt.Sprintf("%.0fx", ratio)
	}
	return fmt.Sprintf("%.2fx", ratio)
}

func textWidth(text string, perChar float64) int {
	return int(float64(len([]rune(text)))*perChar + 0.5)
}

func escapeXML(value string) string {
	var b strings.Builder
	for _, r := range value {
		switch r {
		case '&':
			b.WriteString("&amp;")
		case '<':
			b.WriteString("&lt;")
		case '>':
			b.WriteString("&gt;")
		case '"':
			b.WriteString("&quot;")
		case '\'':
			b.WriteString("&apos;")
		default:
			b.WriteRune(r)
		}
	}
	return b.String()
}

// chartStyle follows rarpar's benchmark chart style, so these read as the
// same family.
const chartStyle = `  <style>
    .bg { fill: #ffffff; } text { font-family: Arial, sans-serif; fill: #172033; }
    .title { font-size: 22px; font-weight: 600; } .subtitle, .legend, .note { font-size: 12px; fill: #667085; }
    .group { font-size: 13px; } .headline { font-weight: 600; }
    .tick { font-size: 11px; fill: #667085; } .value { font-size: 11px; font-weight: 600; } .cpu-value, .rss-value { font-size: 9px; font-weight: 500; } .cpu-value { fill: #0f766e; } .rss-value { fill: #7c3aed; }
    .grid { stroke: #e7eaf0; stroke-width: 1; } .parity { stroke: #172033; stroke-width: 1.5; } .rule { stroke: #f0f2f5; stroke-width: 1; }
    .turbo { fill: #2563eb; } .upstream { fill: #d97706; } .turbo-rss { fill: #7c3aed; } .upstream-rss { fill: #c4b5fd; } .turbo-cpu { fill: #0d9488; } .upstream-cpu { fill: #99f6e4; } .inbar { fill: #ffffff; }
    @media (prefers-color-scheme: dark) { .bg { fill: #0d1117; } text { fill: #e6edf3; } .subtitle, .legend, .note, .tick { fill: #9ca7b5; } .cpu-value { fill: #5eead4; } .rss-value { fill: #bc8cff; } .grid { stroke: #21262d; } .parity { stroke: #e6edf3; } .rule { stroke: #161b22; } .turbo { fill: #58a6ff; } .upstream { fill: #f2a65a; } .turbo-rss { fill: #bc8cff; } .upstream-rss { fill: #6e5a9e; } .turbo-cpu { fill: #2dd4bf; } .upstream-cpu { fill: #1f6f66; } .inbar { fill: #0d1117; } }
  </style>
`

// headlineFirst moves the headline group to the top, wherever its first
// scenario fell.
func headlineFirst(order []string, headline string) []string {
	sorted := []string{}
	for _, name := range order {
		if name == headline {
			sorted = append(sorted, name)
		}
	}
	for _, name := range order {
		if name != headline {
			sorted = append(sorted, name)
		}
	}
	return sorted
}
