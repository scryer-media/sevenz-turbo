package report

import (
	"strings"
	"testing"

	"github.com/scryer-media/sevenz-turbo/bench/sevenz-turbo-bench/internal/suite"
)

const ledgerRow = "decode/media_mx5_3g/T4/ledger"

// ledgerResult is what decode-bench --ledger reports, as the harness reads it
// back from JSON.
func ledgerResult(held, chase float64, waveRuns []any) map[string]any {
	const mib = float64(1 << 20)
	return map[string]any{
		"parallel_path": true, "max_spawned_threads": float64(4),
		"ledger_blocks": float64(1), "ledger_dictionary_bytes": 32 * mib, "ledger_budget_bytes": 1536 * mib,
		"ledger_threads": float64(4), "ledger_worker_threads": float64(4),
		"ledger_peak_held_bytes": held * mib, "ledger_peak_queue_bytes": 150 * mib, "ledger_peak_queue_capacity_bytes": 160 * mib,
		"ledger_peak_spill_bytes": 9 * mib, "ledger_peak_total_bytes": (held + 100) * mib,
		"ledger_peak_runs_out": float64(4), "ledger_peak_runs_pending": float64(1), "ledger_runs": float64(24),
		"ledger_chase_bytes": chase * mib, "ledger_wave_count": float64(len(waveRuns)), "ledger_wave_runs": waveRuns,
		"ledger_wave_runs_out": waveRuns, "ledger_refused_feeds": float64(110), "ledger_refused_at_boundary": float64(100),
		"ledger_refused_at_boundary_busy": float64(99), "ledger_refused_at_boundary_small": float64(100),
		"ledger_refused_mid_run": float64(7), "ledger_refused_run_pending": float64(3), "ledger_gate_refusals": float64(2),
		"ledger_backlog_stops": float64(5), "ledger_waits": float64(40),
		// A second asleep with one run out, half a second with all four out.
		"ledger_wait_nanos_by_runs_out": []any{float64(0), float64(1e9), float64(0), float64(0), float64(5e8)},
	}
}

func ledgerRaw() *suite.Raw {
	raw := sampleRaw()
	scenario := suite.Scenario{ID: ledgerRow, Group: suite.GroupLedger, Op: suite.OpDecode, Fixture: "media_mx5_3g.7z", Threads: "4", Ledger: true,
		Variants: []suite.Run{{Variant: suite.VariantTurbo, Role: suite.RoleCandidate}, {Variant: suite.VariantTurboPlain, Role: suite.RoleCandidate},
			{Variant: suite.VariantOracle, Role: suite.RoleReference}}}
	raw.Scenarios = append(raw.Scenarios, scenario)
	waves := [][]any{{float64(4), float64(3), float64(1), float64(1), float64(1), float64(14)}, {float64(24)}, {float64(4), float64(20)}}
	for i, held := range []float64{600, 500, 700} {
		// The second run's calling thread decoded nothing, so it allocated no
		// dictionary.
		chase := []float64{128, 0, 8}[i]
		ours := run(ledgerRow, suite.VariantTurbo, suite.RoleCandidate, 4, 1000<<20, suite.StatusOK)
		ours.Group, ours.Result = suite.GroupLedger, ledgerResult(held, chase, waves[i])
		plain := run(ledgerRow, suite.VariantTurboPlain, suite.RoleCandidate, 4, 990<<20, suite.StatusOK)
		plain.Group = suite.GroupLedger
		oracle := run(ledgerRow, suite.VariantOracle, suite.RoleReference, 2, 500<<20, suite.StatusOK)
		oracle.Group = suite.GroupLedger
		raw.Runs = append(raw.Runs, ours, plain, oracle)
	}
	return raw
}

func TestALedgerRowIsSummarised(t *testing.T) {
	built := Build(ledgerRaw())
	if len(built.Ledgers) != 1 {
		t.Fatalf("%d ledgers, want the one row that kept one", len(built.Ledgers))
	}
	const mib = float64(1 << 20)
	l := built.Ledgers[0]
	if l.Scenario != ledgerRow || l.Variant != suite.VariantTurbo || l.Threads != 4 || l.Workers != 4 ||
		l.BudgetBytes != 1536<<20 || l.DictionaryBytes != 32<<20 {
		t.Fatalf("header %+v", l)
	}
	for name, got := range map[string][2]Stat{
		"held":     {l.Held, {Median: 600 * mib, Min: 500 * mib, Max: 700 * mib, N: 3}},
		"queue":    {l.Queue, {Median: 150 * mib, Min: 150 * mib, Max: 150 * mib, N: 3}},
		"together": {l.Together, {Median: 700 * mib, Min: 600 * mib, Max: 800 * mib, N: 3}},
		// One dictionary where the calling thread decoded, none where it did not.
		"dictionaries": {l.Dictionaries, {Median: 32 * mib, Min: 0, Max: 32 * mib, N: 3}},
		// No more of it written than the calling thread decoded.
		"touched": {l.DictionariesTouched, {Median: 8 * mib, Min: 0, Max: 32 * mib, N: 3}},
		// Dictionary size times the decoders that ran: four workers, and the
		// calling thread in two of the three runs.
		"nominal": {l.DictionariesNominal, {Median: 160 * mib, Min: 128 * mib, Max: 160 * mib, N: 3}},
		// 1000 MiB of RSS less what was held together and the dictionary.
		"remainder":     {l.Remainder, {Median: 268 * mib, Min: 168 * mib, Max: 400 * mib, N: 3}},
		"runs":          {l.Runs, {Median: 24, Min: 24, Max: 24, N: 3}},
		"waves":         {l.Waves, {Median: 2, Min: 1, Max: 6, N: 3}},
		"runs per wave": {l.RunsPerWave, {Median: 12, Min: 4, Max: 24, N: 3}},
		"at boundary":   {l.RefusedAtBoundary, {Median: 100, Min: 100, Max: 100, N: 3}},
		"gate":          {l.GateRefusals, {Median: 2, Min: 2, Max: 2, N: 3}},
		"wait seconds":  {l.WaitSeconds, {Median: 1.5, Min: 1.5, Max: 1.5, N: 3}},
		// A second with one run out and half a second with four: two on average.
		"out asleep": {l.MeanRunsOutAsleep, {Median: 2, Min: 2, Max: 2, N: 3}},
		// Three of four threads idle for a second, none for the half second.
		"idle": {l.IdleWorkerSeconds, {Median: 3, Min: 3, Max: 3, N: 3}},
	} {
		if got[0] != got[1] {
			t.Errorf("%s: %+v, want %+v", name, got[0], got[1])
		}
	}
	if waves(l.WaveRuns) != "4 3 1x3 14" || waves(l.WaveRunsOut) != "4 3 1x3 14" {
		t.Errorf("wave sequences %q and %q, want the first measured run's", waves(l.WaveRuns), waves(l.WaveRunsOut))
	}
	if len(l.WaitSecondsByRunsOut) != 5 || l.WaitSecondsByRunsOut[1] != 1 || l.WaitSecondsByRunsOut[4] != 0.5 {
		t.Errorf("time asleep by runs out %v, want the first measured run's", l.WaitSecondsByRunsOut)
	}

	md := Markdown(built)
	for _, want := range []string{"## decode ledger: memory", "## decode ledger: dispatch",
		"| " + ledgerRow + " | sevenz-turbo | 4 | - | 1536 | 1000.0 | 600.0 [500.0–700.0] | 150.0 / 160.0 | 9.0 | 700.0 [600.0–800.0] | 32.0 [0.0–32.0] / 8.0 [0.0–32.0] / 160.0 [128.0–160.0] | 268.0 [168.0–400.0] |",
		"| " + ledgerRow + " | sevenz-turbo | 24 | 2 [1–6] | 4 3 1x3 14 | 4 3 1x3 14 | 4 | 2.00 | 100 (99) | 7 | 3 | 2 | 5 | 40 | 1.500 [1.500–1.500] | 3.000 [3.000–3.000] |"} {
		if !strings.Contains(md, want) {
			t.Errorf("report.md lacks %q", want)
		}
	}
}

// The candidate that kept no ledger is compared with 7zz like the one that
// did, so the two can be read side by side.
func TestTheNoLedgerTwinHasItsOwnRatio(t *testing.T) {
	built := Build(ledgerRaw())
	ratios := map[string]Ratio{}
	for _, ratio := range built.Ratios {
		if ratio.Scenario == ledgerRow {
			ratios[ratio.Variant] = ratio
		}
	}
	for _, variant := range []string{suite.VariantTurbo, suite.VariantTurboPlain} {
		ratio, ok := ratios[variant]
		if !ok || *ratio.Wall != 0.5 {
			t.Errorf("%s: ratio %+v, want a wall ratio of 0.5", variant, ratio)
		}
	}
	if len(ratios) != 2 {
		t.Errorf("ratios for %v", ratios)
	}
}

// A report with no ledger row has no ledger section, and a run that kept a
// ledger but took the single-threaded path (no block) adds none.
func TestNoLedgerNoSection(t *testing.T) {
	if md := Markdown(Build(sampleRaw())); strings.Contains(md, "decode ledger") {
		t.Error("a report with no ledger row has a ledger section")
	}
	raw := ledgerRaw()
	for i := range raw.Runs {
		if raw.Runs[i].Variant == suite.VariantTurbo && raw.Runs[i].Scenario == ledgerRow {
			raw.Runs[i].Result["ledger_blocks"] = float64(0)
		}
	}
	if built := Build(raw); len(built.Ledgers) != 0 {
		t.Errorf("ledgers %+v for a decode whose parallel coder never ran", built.Ledgers)
	}
}

func TestWaveSequencesFold(t *testing.T) {
	for want, runs := range map[string][]int64{"-": nil, "24": {24}, "1x3": {1, 1, 1}, "4 3 1x2 3 1 2": {4, 3, 1, 1, 3, 1, 2}} {
		if got := waves(runs); got != want {
			t.Errorf("%v: %q, want %q", runs, got, want)
		}
	}
	// A sequence that does not fold is cut at the terms a cell prints.
	long := make([]int64, 0, 2*wavesShown)
	for i := range 2 * wavesShown {
		long = append(long, int64(1+i%2))
	}
	got := strings.Fields(waves(long))
	if len(got) != wavesShown+1 || got[wavesShown] != "…" || got[0] != "1" || got[wavesShown-1] != "2" {
		t.Errorf("a long sequence prints as %v", got)
	}
}
