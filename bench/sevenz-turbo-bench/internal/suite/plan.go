// Package suite is the benchmark matrix and its runner: which scenario runs
// which variants with which arguments, and the interleaved loop that measures
// every run as its own child process.
package suite

import (
	"fmt"
	"path/filepath"
	"strconv"
	"strings"

	"github.com/scryer-media/sevenz-turbo/bench/sevenz-turbo-bench/internal/fixtures"
)

// Variant names. The candidate is this crate as the default build ships it
// (AWS-LC cryptography); the reference is the official 7zz.
const (
	VariantTurbo       = "sevenz-turbo"
	VariantTurboNative = "sevenz-turbo native-crypto"
	// VariantTurboPlain is the candidate decoding with no ledger kept, in a
	// scenario whose candidate keeps one: the same decode as a consumer runs
	// it, in the same passes, so the report shows what observing it costs.
	VariantTurboPlain = "sevenz-turbo no-ledger"
	VariantUpstream   = "sevenz-rust2"
	VariantOracle     = "7zz"
)

// Roles. A failing candidate or reference run fails the whole run; a failing
// secondary run is reported and does not.
const (
	RoleCandidate = "candidate"
	RoleReference = "reference"
	RoleSecondary = "secondary"
)

// Ops.
const (
	OpList   = "list"
	OpDecode = "decode"
	OpEncode = "encode"
)

// FullOnlyGroups have no scenario in the quick matrix: their fixtures are
// only worth timing at the full corpus's size.
var FullOnlyGroups = map[string]bool{"lzma2 near-incompressible": true, GroupLedger: true}

// GroupLedger is the decodes measured with the reader's ledger kept: runs
// against threads against memory limits.
const GroupLedger = "decode ledger"

// LedgerFixtures are the archives of the ledger rows that are swept over
// every thread count and limit, the one with the most runs first.
// LedgerControls are decoded at each thread count with no limit: their runs
// are small, so a change made for the large-run fixtures must not move them.
var (
	LedgerFixtures = []string{"media_mx5_3g.7z", "media_mx5_2g.7z", "mt.7z"}
	LedgerControls = []string{"media_mx1.7z", "aes_mx1.7z"}
	// LedgerThreads is the sweep of the ledger rows; a count above the usable
	// cores is left out.
	LedgerThreads = []string{"2", "4", "8", "all"}
	// LedgerLimits are the decode limits of the ledger rows, in MiB, after
	// the row with none: 512 and 1024, and 553, 1065 and 2089, which are what
	// weaver passes for 2, 4 and 8 threads.
	LedgerLimits = []int64{512, 553, 1024, 1065, 2089}
)

// Groups, in report order.
var Groups = []string{
	"container parse",
	"lzma2 single-stream",
	"lzma2 parallel",
	"lzma2 near-incompressible",
	"lzma",
	"memory budget",
	GroupLedger,
	"solid vs non-solid",
	"aes-256",
	"filters",
	"ppmd (secondary)",
	"encode",
	"encode aes-256",
}

// Scenario is one row group of the matrix: one operation on one input, run
// by every variant.
type Scenario struct {
	ID      string `json:"id"`
	Group   string `json:"group"`
	Op      string `json:"op"`
	Fixture string `json:"fixture"`
	// Threads is the requested thread count, "all" for every core.
	Threads string `json:"threads"`
	Level   int    `json:"level,omitempty"`
	// MemoryLimit is the decode budget passed to ArchiveLimits::memory.
	MemoryLimit int64 `json:"memory_limit,omitempty"`
	NonSolid    bool  `json:"non_solid,omitempty"`
	Encrypted   bool  `json:"encrypted,omitempty"`
	NoVerify    bool  `json:"no_verify,omitempty"`
	// Adaptive is weaver's chase decode: the parallel LZMA2 decoder started
	// at one thread and widened as runs queue up.
	Adaptive bool `json:"adaptive,omitempty"`
	Stream   bool `json:"stream,omitempty"`
	// Ledger marks a decode whose candidate keeps the reader's ledger
	// (decode-bench --ledger) and reports it in its result.
	Ledger   bool   `json:"ledger,omitempty"`
	Note     string `json:"note,omitempty"`
	Variants []Run  `json:"variants"`
}

// Run is one variant's command for a scenario.
type Run struct {
	Variant string   `json:"variant"`
	Role    string   `json:"role"`
	Tool    string   `json:"tool"`
	Args    []string `json:"args"`
	// Dir is the working directory (7zz a adds paths relative to it).
	Dir string `json:"dir,omitempty"`
	// Output is the archive an encode writes, removed after every run.
	Output string `json:"output,omitempty"`
	// JSON marks a decode-bench op run whose last stdout line is its result.
	JSON bool `json:"json"`
}

// Tools are the binaries a plan runs.
type Tools struct {
	Candidate string
	// Native is the native-crypto build; empty skips its rows.
	Native string
	Oracle string
}

// Settings shape the matrix.
type Settings struct {
	Quick bool
	// Threads is the decode/encode sweep; "all" is always last.
	Threads []string
	Levels  []int
	// Budgets are the memory-budget decode limits, low first.
	Budgets []int64
	// BudgetThreads is the thread count the budget rows ask for.
	BudgetThreads string
	// CPUs is what "all" resolves to: the host's cores, or the size of the
	// --pin-cpus range when the run is pinned, so "all" never asks for more
	// threads than the processes may run on.
	CPUs int
	// Only, when set, keeps the scenarios whose id contains one of these
	// substrings. It is applied before a scenario's fixture is looked up, so a
	// corpus generated with `fixtures --only` serves a matching `run --only`.
	Only []string
}

// DefaultSettings is the full matrix, or its --quick subset, for cpus usable
// cores (the host's, or the pinned range's).
func DefaultSettings(quick bool, cpus int) Settings {
	settings := Settings{Quick: quick, CPUs: max(cpus, 1)}
	if quick {
		settings.Threads = []string{"1", "all"}
		settings.Levels = []int{1, 5}
		settings.Budgets = []int64{20 << 20, 96 << 20}
	} else {
		for _, n := range []int{1, 2, 4, 8, 16} {
			if n < cpus {
				settings.Threads = append(settings.Threads, strconv.Itoa(n))
			}
		}
		settings.Threads = append(settings.Threads, "all")
		settings.Levels = []int{1, 3, 5, 7, 9}
		settings.Budgets = []int64{64 << 20, 512 << 20}
	}
	settings.BudgetThreads = strconv.Itoa(min(8, cpus))
	return settings
}

// Run profiles: which corpus, matrix, repeats and warmups `run --profile`
// selects.
const (
	ProfileQuick = "quick" // the smoke subset over the quick corpus
	ProfileFull  = "full"  // the whole matrix over the full corpus, 5 repeats
	ProfileFleet = "fleet" // the whole matrix over the full corpus, 3 repeats
)

// Profiles lists the run profiles in the order the help text gives them.
var Profiles = []string{ProfileQuick, ProfileFull, ProfileFleet}

// RunProfile is a profile's defaults. Corpus is the fixture profile its
// default --dir holds.
type RunProfile struct {
	Name    string
	Quick   bool
	Corpus  string
	Repeats int
	Warmups int
}

// ProfileByName returns a run profile. fleet is the full matrix, every
// scenario, at three repeats instead of five: the fleet's time goes on the
// full corpus's rows, not on more of them.
func ProfileByName(name string) (RunProfile, error) {
	switch name {
	case ProfileQuick:
		return RunProfile{Name: name, Quick: true, Corpus: "quick", Repeats: 2, Warmups: 0}, nil
	case ProfileFull:
		return RunProfile{Name: name, Corpus: "full", Repeats: 5, Warmups: 1}, nil
	case ProfileFleet:
		return RunProfile{Name: name, Corpus: "full", Repeats: 3, Warmups: 1}, nil
	}
	return RunProfile{}, fmt.Errorf("unknown profile %q (want %s)", name, strings.Join(Profiles, ", "))
}

// Processes is how many processes a planned matrix launches.
func Processes(scenarios []Scenario, repeats, warmups int) int {
	n := 0
	for _, scenario := range scenarios {
		n += len(scenario.Variants) * (repeats + warmups)
	}
	return n
}

// ResolveThreads turns "all" into the usable core count, as both sides are
// given a number.
func (s Settings) ResolveThreads(threads string) string {
	if threads == "all" {
		return strconv.Itoa(max(s.CPUs, 1))
	}
	return threads
}

// Keeps reports whether --only selects the scenario id.
func (s Settings) Keeps(id string) bool {
	if len(s.Only) == 0 {
		return true
	}
	for _, part := range s.Only {
		if part != "" && strings.Contains(id, part) {
			return true
		}
	}
	return false
}

// Plan builds the matrix over the corpus in dir. scratch is where encode rows
// write their archives.
func Plan(manifest *fixtures.Manifest, dir, scratch string, tools Tools, settings Settings) ([]Scenario, error) {
	p := planner{manifest: manifest, dir: dir, scratch: scratch, tools: tools, settings: settings}
	for _, name := range []string{"tree_solid.7z", "tree_nonsolid.7z", "mt.7z", "aes_kdf.7z"} {
		p.list(name)
	}
	for _, threads := range []string{"1", "all"} {
		p.decode("lzma2 single-stream", "st.7z", threads, decodeOpts{upstream: true,
			note: "one stream, no dictionary resets: more threads cannot help any decoder"})
	}
	for _, threads := range settings.Threads {
		upstream := threads == "1" || threads == "all"
		p.decode("lzma2 parallel", "mt.7z", threads, decodeOpts{upstream: upstream})
	}
	p.decode("lzma2 parallel", "mt.7z", "all", decodeOpts{noVerify: true, note: "CRC-32 verification off: the cost of checking"})
	p.decode("lzma2 parallel", "mt.7z", "all", decodeOpts{stream: true, note: "the single-parse streaming consumer path (block_decoder per block, sub-stream CRC hook)"})
	p.decode("lzma2 parallel", "mt.7z", "all", decodeOpts{adaptive: true, note: adaptiveNote})
	if !settings.Quick {
		for _, name := range []string{"media_mx1.7z", "media_mx5.7z"} {
			for _, threads := range []string{"1", "all"} {
				p.decode("lzma2 near-incompressible", name, threads, decodeOpts{upstream: threads == "1",
					note: "near-incompressible LZMA2 in parallel blocks: the shape of a usenet download's media"})
			}
		}
		p.decode("lzma2 near-incompressible", "media_mx5.7z", "all", decodeOpts{adaptive: true, note: adaptiveNote})
		p.decode("lzma2 near-incompressible", "media_mx5.7z", "all", decodeOpts{adaptive: true, memoryLimit: 4 << 30,
			note: adaptiveNote + "; under an explicit 4 GiB limit, as weaver passes its granted decode budget"})
	}
	if !settings.Quick {
		p.ledgerRows()
	}
	p.decode("lzma", "lzma.7z", "1", decodeOpts{upstream: true})
	for _, budget := range settings.Budgets {
		p.decode("memory budget", "mt.7z", settings.BudgetThreads, decodeOpts{memoryLimit: budget,
			note: "decode under ArchiveLimits::memory; parallel_path=false means the budget forced the single-threaded fallback. 7zz has no decode budget: it runs unbounded at the same threads"})
	}
	for _, name := range []string{"tree_solid.7z", "tree_nonsolid.7z"} {
		for _, threads := range []string{"1", "all"} {
			p.decode("solid vs non-solid", name, threads, decodeOpts{upstream: threads == "1"})
		}
		p.decode("solid vs non-solid", name, "all", decodeOpts{noVerify: true, note: "per-member CRC-32 off"})
		p.decode("solid vs non-solid", name, "all", decodeOpts{stream: true, note: "streaming consumer path"})
	}
	p.decode("aes-256", "aes_store.7z", "1", decodeOpts{upstream: true, native: true})
	for _, threads := range []string{"1", "all"} {
		p.decode("aes-256", "aes_mx1.7z", threads, decodeOpts{upstream: threads == "1", native: true})
	}
	p.list("aes_kdf.7z")
	p.decode("aes-256", "aes_kdf.7z", "1", decodeOpts{upstream: true, native: true,
		note: "one SHA-256 key derivation per folder unless cached: the key-derivation row"})
	for _, name := range []string{"bcj_x86.7z", "bcj_arm64.7z", "bcj2.7z", "delta.7z"} {
		p.decode("filters", name, "1", decodeOpts{upstream: true})
	}
	p.decode("ppmd (secondary)", "ppmd.7z", "1", decodeOpts{upstream: true, note: "PPMd is an external crate (see the toolchain's PPMd crates), not this crate's code"})

	encodeThreads := []string{"1", "all"}
	for _, level := range settings.Levels {
		for _, threads := range encodeThreads {
			p.encode("encode", "payload-sub", level, threads, encodeOpts{})
		}
	}
	for _, threads := range settings.Threads {
		if threads != "1" && threads != "all" {
			p.encode("encode", "payload-sub", 5, threads, encodeOpts{})
		}
	}
	p.encode("encode", "tree", 5, "all", encodeOpts{})
	p.encode("encode", "tree", 5, "all", encodeOpts{nonSolid: true})
	p.encode("encode aes-256", "payload-sub", 5, "all", encodeOpts{encrypted: true})
	p.encode("encode aes-256", "kdf-tree", 5, "1", encodeOpts{encrypted: true, nonSolid: true,
		note: "tiny members, one encrypted folder each: write-side key-derivation and per-folder cost"})
	return p.scenarios, p.err
}

type planner struct {
	manifest  *fixtures.Manifest
	dir       string
	scratch   string
	tools     Tools
	settings  Settings
	scenarios []Scenario
	seen      map[string]bool
	err       error
}

func (p *planner) add(scenario Scenario) {
	if p.seen == nil {
		p.seen = map[string]bool{}
	}
	if p.seen[scenario.ID] {
		return
	}
	p.seen[scenario.ID] = true
	p.scenarios = append(p.scenarios, scenario)
}

func (p *planner) archive(name string) (fixtures.ArchiveRecord, bool) {
	record, ok := p.manifest.Archive(name)
	if !ok && p.err == nil {
		p.err = fmt.Errorf("fixture %s is not in the manifest: run `sevenz-turbo-bench fixtures` first", name)
	}
	return record, ok
}

func stem(name string) string { return name[:len(name)-len(filepath.Ext(name))] }

func (p *planner) list(name string) {
	if !p.settings.Keeps("list/" + stem(name)) {
		return
	}
	record, ok := p.archive(name)
	if !ok {
		return
	}
	path := filepath.Join(p.dir, name)
	ours := []string{"op", "list", "--archive", path}
	oracle := []string{"l", "-slt"}
	if record.Encrypted {
		ours = append(ours, "--password", fixtures.Password)
		oracle = append(oracle, "-p"+fixtures.Password)
	}
	oracle = append(oracle, path)
	p.add(Scenario{
		ID: "list/" + stem(name), Group: "container parse", Op: OpList, Fixture: name, Threads: "1",
		Encrypted: record.Encrypted,
		Note:      "header parse and entry walk only; 7zz l -slt also formats every entry's properties",
		Variants: []Run{
			{Variant: VariantTurbo, Role: RoleCandidate, Tool: p.tools.Candidate, Args: ours, JSON: true},
			{Variant: VariantOracle, Role: RoleReference, Tool: p.tools.Oracle, Args: oracle},
		},
	})
}

// adaptiveNote describes the adaptive rows.
const adaptiveNote = "weaver's chase decode: set_adaptive_lzma2 + set_threads(1), widened every 100 ms to the queued runs + 1, up to the thread count; 7zz runs fixed at the same threads"

type decodeOpts struct {
	upstream, native, noVerify, stream, adaptive bool
	// ledger has the candidate keep the reader's ledger. A ledger row with no
	// memory limit also runs the candidate without one, as VariantTurboPlain.
	ledger      bool
	memoryLimit int64
	note        string
}

// ledgerRows plans the ledger group: every ledger fixture at every ledger
// thread count with no limit and under each ledger limit, and the controls at
// the fixed thread counts with no limit.
func (p *planner) ledgerRows() {
	for _, name := range LedgerFixtures {
		for _, threads := range LedgerThreads {
			if !p.affords(threads) {
				continue
			}
			p.decode(GroupLedger, name, threads, decodeOpts{ledger: true})
			for _, limit := range LedgerLimits {
				p.decode(GroupLedger, name, threads, decodeOpts{ledger: true, memoryLimit: limit << 20})
			}
		}
	}
	for _, name := range LedgerControls {
		for _, threads := range LedgerThreads {
			if threads == "all" || !p.affords(threads) {
				continue
			}
			p.decode(GroupLedger, name, threads, decodeOpts{ledger: true, native: true})
		}
	}
}

// affords reports whether the usable cores cover a thread count; "all" always
// does.
func (p *planner) affords(threads string) bool {
	n, err := strconv.Atoi(threads)
	return err != nil || n <= p.settings.CPUs
}

func (p *planner) decode(group, name, threads string, o decodeOpts) {
	path := filepath.Join(p.dir, name)
	n := p.settings.ResolveThreads(threads)
	id := fmt.Sprintf("decode/%s/T%s", stem(name), threads)
	ours := []string{"op", "decode", "--archive", path, "--threads", n}
	switch {
	case o.noVerify:
		id += "/no-verify"
		ours = append(ours, "--no-verify")
	case o.stream:
		id += "/stream"
		ours = append(ours, "--stream")
	}
	if o.adaptive {
		id += "/adaptive"
		ours = append(ours, "--adaptive")
	}
	if o.memoryLimit > 0 {
		id += fmt.Sprintf("/budget-%dMiB", o.memoryLimit>>20)
		ours = append(ours, "--memory-limit", strconv.FormatInt(o.memoryLimit, 10))
	}
	// The twin of a ledger row is the same command without the ledger.
	plain := append([]string(nil), ours...)
	if o.ledger {
		id += "/ledger"
		ours = append(ours, "--ledger")
	}
	if !p.settings.Keeps(id) {
		return
	}
	record, ok := p.archive(name)
	if !ok {
		return
	}
	oracle := []string{"t", "-bso0", "-bsp0", "-mmt=" + n}
	if record.Encrypted {
		ours = append(ours, "--password", fixtures.Password)
		plain = append(plain, "--password", fixtures.Password)
		oracle = append(oracle, "-p"+fixtures.Password)
	}
	oracle = append(oracle, path)
	variants := []Run{{Variant: VariantTurbo, Role: RoleCandidate, Tool: p.tools.Candidate, Args: ours, JSON: true}}
	if o.native && record.Encrypted && p.tools.Native != "" {
		variants = append(variants, Run{Variant: VariantTurboNative, Role: RoleCandidate, Tool: p.tools.Native, Args: ours, JSON: true})
	}
	if o.ledger && o.memoryLimit == 0 {
		variants = append(variants, Run{Variant: VariantTurboPlain, Role: RoleCandidate, Tool: p.tools.Candidate, Args: plain, JSON: true})
	}
	variants = append(variants, Run{Variant: VariantOracle, Role: RoleReference, Tool: p.tools.Oracle, Args: oracle})
	if o.upstream {
		upstream := []string{"op", "decode", "--engine", "upstream", "--archive", path, "--threads", n}
		if record.Encrypted {
			upstream = append(upstream, "--password", fixtures.Password)
		}
		variants = append(variants, Run{Variant: VariantUpstream, Role: RoleSecondary, Tool: p.tools.Candidate, Args: upstream, JSON: true})
	}
	p.add(Scenario{
		ID: id, Group: group, Op: OpDecode, Fixture: name, Threads: threads,
		MemoryLimit: o.memoryLimit, Encrypted: record.Encrypted, NoVerify: o.noVerify, Stream: o.stream,
		Adaptive: o.adaptive, Ledger: o.ledger, Note: o.note, Variants: variants,
	})
}

type encodeOpts struct {
	nonSolid, encrypted bool
	note                string
}

// The crate's encoder levels (src/encoder_options.rs, LzmaSettings): xz's
// table, not 7-Zip's. 7zz's own -mx<L> picks a different dictionary, match
// finder and fast-bytes (-mx1 is a 256 KiB dictionary where this crate's level
// 1 is 1 MiB; -mx5 is 16 MiB where the crate's is 8 MiB, so the multi-threaded
// block, four dictionaries, differs too). The oracle is given the crate's
// settings outright so the time and size ratios compare the same work. Keep
// this in step with LzmaSettings.
var (
	levelDictMiB  = [10]string{"256k", "1m", "2m", "4m", "4m", "8m", "8m", "16m", "32m", "64m"}
	levelNiceLen  = [10]int{128, 128, 273, 273, 16, 32, 64, 64, 64, 64}
	levelHC4Depth = [4]int{4, 8, 24, 48}
)

// OracleLZMA2Method is 7zz's -m0 switch for the crate's level: levels 0-3 are
// the fast parser over HC4 with an explicit depth, the rest the optimal
// parser over BT4 with the encoder's default depth, as in LzmaSettings.
func OracleLZMA2Method(level int) string {
	level = max(0, min(level, 9))
	method := fmt.Sprintf("-m0=lzma2:d=%s:fb=%d", levelDictMiB[level], levelNiceLen[level])
	if level <= 3 {
		return method + fmt.Sprintf(":mf=hc4:a=0:mc=%d", levelHC4Depth[level])
	}
	return method + ":mf=bt4:a=1"
}

func (p *planner) encode(group, source string, level int, threads string, o encodeOpts) {
	// The id names solid or non-solid for a tree source. The kind is the
	// recipe's (both corpus profiles share it), so the id is known before the
	// manifest is asked for the source.
	spec, _ := fixtures.Full().Source(source)
	n := p.settings.ResolveThreads(threads)
	id := fmt.Sprintf("encode/%s/L%d/T%s", source, level, threads)
	solid := "solid"
	if o.nonSolid {
		solid = "non-solid"
	}
	if spec.Kind == fixtures.KindTree {
		id += "/" + solid
	}
	if o.encrypted {
		id += "/aes"
	}
	if !p.settings.Keeps(id) {
		return
	}
	if _, ok := p.manifest.Source(source); !ok {
		if p.err == nil {
			p.err = fmt.Errorf("source %s is not in the manifest: run `sevenz-turbo-bench fixtures` first", source)
		}
		return
	}
	slug := filepath.Base(id)
	ourOut := filepath.Join(p.scratch, fmt.Sprintf("%d-%s-ours.7z", len(p.scenarios), slug))
	oracleOut := filepath.Join(p.scratch, fmt.Sprintf("%d-%s-7zz.7z", len(p.scenarios), slug))
	input := fixtures.SourceDir(p.dir, source)
	ours := []string{"op", "encode", "--input", input, "--out", ourOut, "--level", strconv.Itoa(level), "--threads", n}
	oracle := []string{"a", "-bso0", "-bsp0", "-y", "-t7z", OracleLZMA2Method(level), fmt.Sprintf("-mx=%d", level), "-mmt=" + n}
	if o.nonSolid {
		ours = append(ours, "--non-solid")
		oracle = append(oracle, "-ms=off")
	} else {
		oracle = append(oracle, "-ms=on")
	}
	if o.encrypted {
		ours = append(ours, "--password", fixtures.Password)
		oracle = append(oracle, "-p"+fixtures.Password, "-mhe=on")
	}
	entries, err := fixtures.Entries(p.dir, source)
	if err != nil && p.err == nil {
		p.err = err
	}
	oracle = append(oracle, oracleOut)
	oracle = append(oracle, entries...)
	variants := []Run{{Variant: VariantTurbo, Role: RoleCandidate, Tool: p.tools.Candidate, Args: ours, Output: ourOut, JSON: true}}
	if o.encrypted && p.tools.Native != "" {
		nativeOut := filepath.Join(p.scratch, fmt.Sprintf("%d-%s-native.7z", len(p.scenarios), slug))
		native := append([]string(nil), ours...)
		for i := range native {
			if native[i] == ourOut {
				native[i] = nativeOut
			}
		}
		variants = append(variants, Run{Variant: VariantTurboNative, Role: RoleCandidate, Tool: p.tools.Native, Args: native, Output: nativeOut, JSON: true})
	}
	variants = append(variants, Run{Variant: VariantOracle, Role: RoleReference, Tool: p.tools.Oracle, Args: oracle, Dir: input, Output: oracleOut})
	p.add(Scenario{
		ID: id, Group: group, Op: OpEncode, Fixture: source, Threads: threads, Level: level,
		NonSolid: o.nonSolid, Encrypted: o.encrypted, Note: o.note, Variants: variants,
	})
}
