package report

import (
	"fmt"
	"strings"

	"github.com/scryer-media/sevenz-turbo/bench/sevenz-turbo-bench/internal/suite"
)

// Ledger is the memory and dispatch ledger of one candidate decode row: what
// `decode-bench --ledger` reported of the parallel LZMA2 decode, summarised
// over the measured runs. Byte figures are bytes, and every Stat is over the
// runs, one figure per run.
//
// The memory terms are peaks taken by the reader at its own looks (after every
// read of packed input, every hand-over to the decoder and every drain of
// output), so none is above the true peak. They peak at different moments:
// Together is the most the three came to at one look, and Remainder is
// measured against that, not against their sum.
type Ledger struct {
	Scenario string `json:"scenario"`
	Variant  string `json:"variant"`
	// Threads is the thread ceiling the decode applied and Workers the most
	// worker threads it had; MemoryLimit is the limit the row passed and
	// BudgetBytes what that left the decoder to hold (the default for the
	// thread count where no limit was passed). DictionaryBytes is the
	// stream's dictionary size.
	Threads         int64 `json:"threads"`
	Workers         int64 `json:"worker_threads"`
	MemoryLimit     int64 `json:"memory_limit,omitempty"`
	BudgetBytes     int64 `json:"budget_bytes"`
	DictionaryBytes int64 `json:"dictionary_bytes"`

	RSS Stat `json:"max_rss_bytes"`
	// Held is the peak of lzma-turbo's held_bytes(): input pieces, the buffer
	// of every run out with a worker, decoded runs waiting their turn, and
	// buffers parked for reuse, by capacity.
	Held Stat `json:"peak_held_bytes"`
	// Queue is the peak of the reader's queue of packed input read and not yet
	// handed over, by length (what its 192 MiB allowance is measured against);
	// QueueCapacity is the same queue by the capacity of its pieces.
	Queue         Stat `json:"peak_queue_bytes"`
	QueueCapacity Stat `json:"peak_queue_capacity_bytes"`
	// Spill is the peak of decoded output held between decoder and caller.
	Spill Stat `json:"peak_spill_bytes"`
	// Together is the peak of Held + QueueCapacity + Spill at one look.
	Together Stat `json:"peak_together_bytes"`
	// Dictionaries is the dictionary memory the decode allocated: one
	// dictionary when the calling thread decoded a run itself, none otherwise.
	// A worker has no dictionary of its own; it decodes into the run's output
	// buffer, which Held counts.
	Dictionaries Stat `json:"dictionaries_allocated_bytes"`
	// DictionariesTouched bounds what of that was written: no more than the
	// bytes the calling thread decoded.
	DictionariesTouched Stat `json:"dictionaries_touched_bytes"`
	// DictionariesNominal is the dictionary size times the decoders that ran
	// (the workers, and the calling thread when it decoded): what an
	// accounting of one dictionary per decoder would charge.
	DictionariesNominal Stat `json:"dictionaries_nominal_bytes"`
	// Remainder is RSS less Together and Dictionaries: everything the ledger
	// does not name. It is negative where buffers counted by capacity were
	// never written, since RSS counts touched pages only.
	Remainder Stat `json:"remainder_bytes"`

	// Runs is the runs handed to a decoder. A wave is the runs a decoder
	// claimed between two sleeps of the delivering thread: Waves is how many
	// there were and RunsPerWave the runs over the waves.
	Runs        Stat `json:"runs"`
	Waves       Stat `json:"waves"`
	RunsPerWave Stat `json:"runs_per_wave"`
	// WaveRuns and WaveRunsOut are the runs claimed in each wave and the runs
	// out as the delivering thread went to sleep at its end, in order, of the
	// first measured run. A run is out from its claim until its last byte is
	// delivered, so the figure is at least the threads at work.
	WaveRuns    []int64 `json:"wave_runs"`
	WaveRunsOut []int64 `json:"wave_runs_out"`
	// WaitSecondsByRunsOut is the first measured run's time asleep by the runs
	// out as the sleep began (element n: with n runs out), and
	// MeanRunsOutAsleep that distribution's mean over the measured runs: how
	// many runs were out, on average, while the delivering thread had nothing
	// to do but wait.
	WaitSecondsByRunsOut []float64 `json:"wait_seconds_by_runs_out"`
	MeanRunsOutAsleep    Stat      `json:"mean_runs_out_asleep"`
	PeakRunsOut          Stat      `json:"peak_runs_out"`
	PeakRunsQueued       Stat      `json:"peak_runs_pending"`
	ChaseBytes           Stat      `json:"chase_bytes"`

	// Refused is the input pieces the decoder handed back for want of room,
	// and the next four split it: at a run boundary with no complete run
	// waiting (and, of those, with a run out); part way through a run; at a
	// run boundary with a complete run already waiting.
	Refused               Stat `json:"refused_feeds"`
	RefusedAtBoundary     Stat `json:"refused_at_boundary"`
	RefusedAtBoundaryBusy Stat `json:"refused_at_boundary_busy"`
	RefusedMidRun         Stat `json:"refused_mid_run"`
	RefusedRunPending     Stat `json:"refused_run_pending"`
	// GateRefusals is the times the reader stopped reading ahead because what
	// the decoder held left no room for another run under the budget;
	// BacklogStops the times it stopped with room, the backlog being enough.
	GateRefusals Stat `json:"gate_refusals"`
	BacklogStops Stat `json:"backlog_stops"`
	// Waits is the times the delivering thread slept for a worker, WaitSeconds
	// how long, and IdleWorkerSeconds that time weighted by the threads with
	// no run out while it slept.
	Waits             Stat `json:"waits"`
	WaitSeconds       Stat `json:"wait_seconds"`
	IdleWorkerSeconds Stat `json:"idle_worker_seconds"`
}

// field reads a number decode-bench reported.
func field(result map[string]any, name string) (float64, bool) {
	value, ok := result[name].(float64)
	return value, ok
}

func ints(result map[string]any, name string) []int64 {
	list, _ := result[name].([]any)
	out := make([]int64, 0, len(list))
	for _, item := range list {
		value, _ := item.(float64)
		out = append(out, int64(value))
	}
	return out
}

// ledger summarises the ledgers of a scenario's measured runs of one variant,
// or returns nil when none of them carried one.
func ledger(scenario suite.Scenario, variant string, runs []suite.RunRecord) *Ledger {
	out := &Ledger{Scenario: scenario.ID, Variant: variant, MemoryLimit: scenario.MemoryLimit}
	series := map[string][]float64{}
	add := func(name string, value float64) { series[name] = append(series[name], value) }
	n := 0
	for _, run := range runs {
		if run.Status != suite.StatusOK {
			continue
		}
		blocks, ok := field(run.Result, "ledger_blocks")
		if !ok || blocks == 0 {
			continue
		}
		get := func(name string) float64 {
			value, _ := field(run.Result, "ledger_"+name)
			return value
		}
		if n == 0 {
			out.WaveRuns, out.WaveRunsOut = ints(run.Result, "ledger_wave_runs"), ints(run.Result, "ledger_wave_runs_out")
			for _, nanos := range ints(run.Result, "ledger_wait_nanos_by_runs_out") {
				out.WaitSecondsByRunsOut = append(out.WaitSecondsByRunsOut, float64(nanos)/1e9)
			}
		}
		n++
		out.Threads = max(out.Threads, int64(get("threads")))
		out.Workers = max(out.Workers, int64(get("worker_threads")))
		out.BudgetBytes = max(out.BudgetBytes, int64(get("budget_bytes")))
		out.DictionaryBytes = max(out.DictionaryBytes, int64(get("dictionary_bytes")))
		for _, name := range []string{"peak_held_bytes", "peak_queue_bytes", "peak_queue_capacity_bytes", "peak_spill_bytes",
			"peak_total_bytes", "peak_runs_out", "peak_runs_pending", "runs", "chase_bytes", "wave_count", "refused_feeds",
			"refused_at_boundary", "refused_at_boundary_busy", "refused_mid_run", "refused_run_pending", "gate_refusals",
			"backlog_stops", "waits"} {
			add(name, get(name))
		}
		rss := float64(run.MaxRSSBytes)
		add("rss", rss)
		dictionary, chase, decoders := get("dictionary_bytes"), get("chase_bytes"), get("worker_threads")
		allocated := 0.0
		if chase > 0 {
			allocated = dictionary
			decoders++
		}
		add("dictionaries", allocated)
		add("touched", min(allocated, chase))
		add("nominal", dictionary*decoders)
		add("remainder", rss-get("peak_total_bytes")-allocated)
		if waves := get("wave_count"); waves > 0 {
			add("runs_per_wave", get("runs")/waves)
		}
		var waited, idle, weighted float64
		for busy, nanos := range ints(run.Result, "ledger_wait_nanos_by_runs_out") {
			slept := float64(nanos) / 1e9
			waited += slept
			weighted += slept * float64(busy)
			idle += slept * max(get("threads")-float64(busy), 0)
		}
		add("wait_seconds", waited)
		add("idle_worker_seconds", idle)
		if waited > 0 {
			add("mean_runs_out_asleep", weighted/waited)
		}
	}
	if n == 0 {
		return nil
	}
	of := func(name string) Stat { return stat(series[name]) }
	out.RSS, out.Held, out.Queue, out.QueueCapacity = of("rss"), of("peak_held_bytes"), of("peak_queue_bytes"), of("peak_queue_capacity_bytes")
	out.Spill, out.Together = of("peak_spill_bytes"), of("peak_total_bytes")
	out.Dictionaries, out.DictionariesTouched, out.DictionariesNominal = of("dictionaries"), of("touched"), of("nominal")
	out.Remainder = of("remainder")
	out.Runs, out.Waves, out.RunsPerWave = of("runs"), of("wave_count"), of("runs_per_wave")
	out.PeakRunsOut, out.PeakRunsQueued, out.ChaseBytes = of("peak_runs_out"), of("peak_runs_pending"), of("chase_bytes")
	out.Refused, out.RefusedAtBoundary, out.RefusedAtBoundaryBusy = of("refused_feeds"), of("refused_at_boundary"), of("refused_at_boundary_busy")
	out.RefusedMidRun, out.RefusedRunPending = of("refused_mid_run"), of("refused_run_pending")
	out.GateRefusals, out.BacklogStops = of("gate_refusals"), of("backlog_stops")
	out.Waits, out.WaitSeconds, out.IdleWorkerSeconds = of("waits"), of("wait_seconds"), of("idle_worker_seconds")
	out.MeanRunsOutAsleep = of("mean_runs_out_asleep")
	return out
}

// mibStat is a byte Stat in MiB to one decimal, with its range when the runs
// differed. The remainder can be negative, so the sign is kept.
func mibStat(s Stat) string {
	if s.N == 0 {
		return "-"
	}
	const unit = 1 << 20
	if s.Min == s.Max {
		return fmt.Sprintf("%.1f", s.Median/unit)
	}
	return fmt.Sprintf("%.1f [%.1f–%.1f]", s.Median/unit, s.Min/unit, s.Max/unit)
}

// countStat is a count Stat: whole when every run agreed, else the median
// with its range.
func countStat(s Stat) string {
	if s.N == 0 {
		return "-"
	}
	if s.Min == s.Max {
		return fmt.Sprintf("%.0f", s.Median)
	}
	return fmt.Sprintf("%s [%.0f–%.0f]", strings.TrimSuffix(fmt.Sprintf("%.1f", s.Median), ".0"), s.Min, s.Max)
}

// wavesShown is the folded terms of a wave sequence a table cell prints; the
// whole sequence is in report.json.
const wavesShown = 24

// waves writes a wave sequence with repeats folded: 4 3 1 1 1 2 is
// "4 3 1x3 2". A sequence of more than wavesShown terms is cut there.
func waves(runs []int64) string {
	if len(runs) == 0 {
		return "-"
	}
	var parts []string
	for i := 0; i < len(runs); {
		if len(parts) == wavesShown {
			parts = append(parts, "…")
			break
		}
		j := i
		for j < len(runs) && runs[j] == runs[i] {
			j++
		}
		if j-i > 1 {
			parts = append(parts, fmt.Sprintf("%dx%d", runs[i], j-i))
		} else {
			parts = append(parts, fmt.Sprint(runs[i]))
		}
		i = j
	}
	return strings.Join(parts, " ")
}

// meanStat is a mean to two decimals, with its range when the runs differed.
func meanStat(s Stat) string {
	if s.N == 0 {
		return "-"
	}
	if s.Min == s.Max {
		return fmt.Sprintf("%.2f", s.Median)
	}
	return fmt.Sprintf("%.2f [%.2f–%.2f]", s.Median, s.Min, s.Max)
}

// renderLedgers writes the ledger tables of a host's report.
func renderLedgers(b *strings.Builder, ledgers []Ledger) {
	if len(ledgers) == 0 {
		return
	}
	fmt.Fprintln(b, "## decode ledger: memory")
	fmt.Fprintln(b)
	fmt.Fprintln(b, "What the parallel LZMA2 decode of each ledger row held, in MiB, as the median over the measured runs with the range where runs differed. `held` is the peak of lzma-turbo's `held_bytes()`; `queue` the peak of the reader's packed-input queue by length and by capacity; `spill` decoded output between decoder and caller; `together` the most those three came to at one look, which is what a memory limit governs; `dictionaries` what the decode allocated, the part of it that can have been written, and dictionary size times the decoders that ran; `remainder` is peak RSS less `together` and the allocated dictionaries. Each term is sampled where the reader looks, so none is above its true peak; a remainder is negative where buffers counted by capacity were never written.")
	fmt.Fprintln(b)
	fmt.Fprintln(b, "| scenario | variant | threads | limit | budget | peak RSS | held | queue (length / capacity) | spill | together | dictionaries (allocated / touched / nominal) | remainder |")
	fmt.Fprintln(b, "|---|---|---|---|---|---|---|---|---|---|---|---|")
	for _, l := range ledgers {
		limit := "-"
		if l.MemoryLimit > 0 {
			limit = fmt.Sprint(l.MemoryLimit >> 20)
		}
		fmt.Fprintf(b, "| %s | %s | %d | %s | %d | %s | %s | %s / %s | %s | %s | %s / %s / %s | %s |\n", l.Scenario, l.Variant, l.Threads, limit,
			l.BudgetBytes>>20, mibStat(l.RSS), mibStat(l.Held), mibStat(l.Queue), mibStat(l.QueueCapacity), mibStat(l.Spill),
			mibStat(l.Together), mibStat(l.Dictionaries), mibStat(l.DictionariesTouched), mibStat(l.DictionariesNominal), mibStat(l.Remainder))
	}
	fmt.Fprintln(b)
	fmt.Fprintln(b, "## decode ledger: dispatch")
	fmt.Fprintln(b)
	fmt.Fprintln(b, "How each ledger row's runs went out. A wave is the runs a decoder claimed between two sleeps of the delivering thread; `runs per wave` and `runs out per wave` are the first measured run's sequences, repeats folded (`1x3` is three waves of one), the second being the runs out as that thread went to sleep at the wave's end: a decode that keeps its threads supplied shows the thread count there wave after wave, one that lets them run dry shows what it claimed. A run is out from its claim until its last byte is delivered, so a run decoded and waiting its turn still counts. `out asleep` is the mean runs out over the time the delivering thread slept. `refused` is the input pieces the decoder handed back for want of room: at a run boundary with no complete run waiting (in brackets, those with a run out), part way through a run, and at a boundary with a run already waiting. `gate` is the times the reader stopped reading ahead for want of room for a run under the budget, `backlog` the times it stopped with room. `idle` is the seconds the delivering thread slept for a worker, weighted by the threads with no run out.")
	fmt.Fprintln(b)
	fmt.Fprintln(b, "| scenario | variant | runs | waves | runs per wave | runs out per wave | peak runs out | out asleep | refused at boundary (busy) | refused mid-run | refused, run waiting | gate | backlog | waits | wait s | idle thread s |")
	fmt.Fprintln(b, "|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|")
	for _, l := range ledgers {
		fmt.Fprintf(b, "| %s | %s | %s | %s | %s | %s | %s | %s | %s (%s) | %s | %s | %s | %s | %s | %s | %s |\n", l.Scenario, l.Variant, countStat(l.Runs),
			countStat(l.Waves), waves(l.WaveRuns), waves(l.WaveRunsOut), countStat(l.PeakRunsOut), meanStat(l.MeanRunsOutAsleep),
			countStat(l.RefusedAtBoundary), countStat(l.RefusedAtBoundaryBusy),
			countStat(l.RefusedMidRun), countStat(l.RefusedRunPending), countStat(l.GateRefusals), countStat(l.BacklogStops),
			countStat(l.Waits), seconds(l.WaitSeconds), seconds(l.IdleWorkerSeconds))
	}
	fmt.Fprintln(b)
}
