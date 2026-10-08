// Command sevenz-turbo-bench measures the sevenz-turbo crate against the
// official 7-Zip (7zz) on one host, and merges several hosts' reports.
//
//	sevenz-turbo-bench fixtures  [--dir D] [--profile full|quick] [--oracle 7zz] [--only a.7z,...]
//	sevenz-turbo-bench toolchain [--candidate B] [--candidate-native B] [--oracle 7zz] [--repo R]
//	sevenz-turbo-bench run       --out DIR [--dir D] [--profile quick|full|fleet] [--quick] [--list]
//	                             [--candidate B] [--candidate-native B]
//	                             [--oracle 7zz] [--machine LABEL] [--repeats N] [--warmups N]
//	                             [--only SUBSTR,...] [--pin-cpus 0-7] [--timeout 1h]
//	sevenz-turbo-bench report    --input raw.json --out report.json [--md report.md]
//	sevenz-turbo-bench merge     --out merged.md report.json...
//
// Exit status: 0 success; 1 a measured run failed (or missing RSS); 2 usage;
// 3 a prerequisite is missing (oracle, candidate, fixtures).
package main

import (
	"context"
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"os"
	"os/exec"
	"os/signal"
	"path/filepath"
	"runtime"
	"strings"
	"time"

	"github.com/scryer-media/sevenz-turbo/bench/sevenz-turbo-bench/internal/fixtures"
	"github.com/scryer-media/sevenz-turbo/bench/sevenz-turbo-bench/internal/host"
	"github.com/scryer-media/sevenz-turbo/bench/sevenz-turbo-bench/internal/procmeasure"
	"github.com/scryer-media/sevenz-turbo/bench/sevenz-turbo-bench/internal/report"
	"github.com/scryer-media/sevenz-turbo/bench/sevenz-turbo-bench/internal/suite"
	"github.com/scryer-media/sevenz-turbo/bench/sevenz-turbo-bench/internal/toolchain"
)

const (
	exitOK           = 0
	exitFailed       = 1
	exitUsage        = 2
	exitPrerequisite = 3
)

const usage = `usage: sevenz-turbo-bench <fixtures|toolchain|run|report|merge> [flags]
run "sevenz-turbo-bench <command> -h" for a command's flags`

// prerequisite marks an error that is a missing input, not a failed run.
type prerequisite struct{ error }

func main() {
	if len(os.Args) < 2 {
		fmt.Fprintln(os.Stderr, usage)
		os.Exit(exitUsage)
	}
	ctx, stop := signal.NotifyContext(context.Background(), os.Interrupt)
	defer stop()
	var code int
	switch os.Args[1] {
	case "fixtures":
		code = cmdFixtures(ctx, os.Args[2:])
	case "toolchain":
		code = cmdToolchain(ctx, os.Args[2:])
	case "run":
		code = cmdRun(ctx, os.Args[2:])
	case "report":
		code = cmdReport(os.Args[2:])
	case "merge":
		code = cmdMerge(os.Args[2:])
	case "-h", "--help", "help":
		fmt.Println(usage)
	default:
		fmt.Fprintf(os.Stderr, "unknown command %q\n%s\n", os.Args[1], usage)
		code = exitUsage
	}
	os.Exit(code)
}

func env(name, fallback string) string {
	if value := os.Getenv(name); value != "" {
		return value
	}
	return fallback
}

func fail(err error) int {
	fmt.Fprintln(os.Stderr, "sevenz-turbo-bench:", err)
	var missing prerequisite
	if errors.As(err, &missing) {
		return exitPrerequisite
	}
	return exitFailed
}

func parse(set *flag.FlagSet, args []string) bool {
	set.SetOutput(os.Stderr)
	return set.Parse(args) == nil
}

func defaultFixtureDir(quick bool) string {
	if quick {
		return env("SEVENZ_BENCH_FIXTURES", filepath.Join("bench", "fixtures", "quick"))
	}
	return env("SEVENZ_BENCH_FIXTURES", filepath.Join("bench", "fixtures", "full"))
}

func cmdFixtures(ctx context.Context, args []string) int {
	set := flag.NewFlagSet("fixtures", flag.ContinueOnError)
	profileName := set.String("profile", "full", "corpus profile: full or quick")
	dir := set.String("dir", "", "corpus directory (default bench/fixtures/<profile>, or $SEVENZ_BENCH_FIXTURES)")
	oraclePath := set.String("oracle", "", "7zz to write the archives with (default $SEVENZ_BENCH_ORACLE, then PATH)")
	allowP7zip := set.Bool("allow-p7zip", false, "accept a p7zip build as the oracle")
	only := set.String("only", "", "comma-separated archive names to generate")
	if !parse(set, args) {
		return exitUsage
	}
	profile, err := fixtures.ByName(*profileName)
	if err != nil {
		fmt.Fprintln(os.Stderr, err)
		return exitUsage
	}
	if *dir == "" {
		*dir = defaultFixtureDir(profile.Name == "quick")
	}
	oracle, err := resolveOracle(ctx, *oraclePath, *allowP7zip)
	if err != nil {
		return fail(err)
	}
	var names []string
	if *only != "" {
		names = strings.Split(*only, ",")
	}
	manifest, err := fixtures.Generate(ctx, fixtures.Options{
		Dir: *dir, Profile: profile, Only: names, Log: os.Stderr,
		SevenZip: fixtures.Oracle{Path: oracle.Path, Banner: oracle.Banner, SHA256: oracle.SHA256},
	})
	if err != nil {
		return fail(err)
	}
	for _, archive := range manifest.Archives {
		fmt.Printf("%-18s %12d bytes  unpacked %12d  entries %5d  sha256 %s\n", archive.Name, archive.Bytes, archive.UnpackedBytes, archive.Entries, archive.SHA256)
	}
	fmt.Printf("wrote %s\n", filepath.Join(*dir, fixtures.ManifestName))
	return exitOK
}

func resolveOracle(ctx context.Context, explicit string, allowP7zip bool) (toolchain.Oracle, error) {
	path, err := toolchain.FindOracle(explicit)
	if err != nil {
		return toolchain.Oracle{}, prerequisite{err}
	}
	oracle, err := toolchain.ProbeOracle(ctx, path, allowP7zip)
	if err != nil {
		return toolchain.Oracle{}, prerequisite{err}
	}
	return oracle, nil
}

type toolFlags struct {
	candidate, native, oracle, repo *string
	allowP7zip                      *bool
}

func addToolFlags(set *flag.FlagSet) toolFlags {
	return toolFlags{
		candidate:  set.String("candidate", env("SEVENZ_BENCH_CANDIDATE", ""), "decode-bench built with default features (AWS-LC); default $SEVENZ_BENCH_CANDIDATE, then <repo>/target/release/decode-bench"),
		native:     set.String("candidate-native", env("SEVENZ_BENCH_CANDIDATE_NATIVE", ""), "decode-bench built with --features native-crypto; its AES rows are skipped when empty"),
		oracle:     set.String("oracle", "", "official 7zz (default $SEVENZ_BENCH_ORACLE, then 7zz/7zz.exe/7z/7za on PATH)"),
		repo:       set.String("repo", env("SEVENZ_BENCH_REPO", ""), "sevenz-turbo checkout, for rustc/commit/Cargo.lock provenance (default: git toplevel of the working directory)"),
		allowP7zip: set.Bool("allow-p7zip", false, "accept a p7zip build as the oracle"),
	}
}

func (f toolFlags) collect(ctx context.Context) (toolchain.Toolchain, suite.Tools, error) {
	rust := toolchain.ProbeRust(ctx, *f.repo)
	candidate := *f.candidate
	if candidate == "" {
		repo := *f.repo
		if repo == "" && rust.Commit != "not-collected" {
			if top, err := gitTop(ctx); err == nil {
				repo = top
			}
		}
		if repo != "" {
			candidate = toolchain.DefaultCandidate(repo)
		}
	}
	if candidate == "" {
		return toolchain.Toolchain{}, suite.Tools{}, prerequisite{errors.New("no candidate: pass --candidate (cargo build --locked --release -p decode-bench)")}
	}
	primary, err := toolchain.ProbeCandidate(ctx, suite.VariantTurbo, candidate)
	if err != nil {
		return toolchain.Toolchain{}, suite.Tools{}, prerequisite{err}
	}
	if err := toolchain.CheckBackend(primary, "--candidate", toolchain.BackendDefault); err != nil {
		return toolchain.Toolchain{}, suite.Tools{}, prerequisite{fmt.Errorf("%w: build it with default features", err)}
	}
	if err := toolchain.CheckEncoder(primary, "--candidate"); err != nil {
		return toolchain.Toolchain{}, suite.Tools{}, prerequisite{err}
	}
	if err := toolchain.CheckProfile(primary, "--candidate"); err != nil {
		return toolchain.Toolchain{}, suite.Tools{}, prerequisite{err}
	}
	// The checkout's rustc, commit and Cargo.lock describe the candidate only
	// when the candidate says it was built from that commit and lock.
	chain := toolchain.Toolchain{Candidates: []toolchain.Candidate{primary}, Rust: toolchain.BindRust(rust, primary), LinkedLzmaTurbo: primary.Field("lzma_turbo")}
	tools := suite.Tools{Candidate: candidate}
	if *f.native != "" {
		native, err := toolchain.ProbeCandidate(ctx, suite.VariantTurboNative, *f.native)
		if err != nil {
			return toolchain.Toolchain{}, suite.Tools{}, prerequisite{err}
		}
		if err := toolchain.CheckBackend(native, "--candidate-native", toolchain.BackendNative); err != nil {
			return toolchain.Toolchain{}, suite.Tools{}, prerequisite{fmt.Errorf("%w: build it with --features native-crypto", err)}
		}
		if err := toolchain.CheckEncoder(native, "--candidate-native"); err != nil {
			return toolchain.Toolchain{}, suite.Tools{}, prerequisite{err}
		}
		if err := toolchain.CheckProfile(native, "--candidate-native"); err != nil {
			return toolchain.Toolchain{}, suite.Tools{}, prerequisite{err}
		}
		if err := toolchain.SameBuild(primary, native); err != nil {
			return toolchain.Toolchain{}, suite.Tools{}, prerequisite{err}
		}
		chain.Candidates = append(chain.Candidates, native)
		tools.Native = *f.native
	}
	oracle, err := resolveOracle(ctx, *f.oracle, *f.allowP7zip)
	if err != nil {
		return toolchain.Toolchain{}, suite.Tools{}, err
	}
	chain.Oracle = oracle
	tools.Oracle = oracle.Path
	return chain, tools, nil
}

func gitTop(ctx context.Context) (string, error) {
	output, err := exec.CommandContext(ctx, "git", "rev-parse", "--show-toplevel").Output()
	return strings.TrimSpace(string(output)), err
}

func cmdToolchain(ctx context.Context, args []string) int {
	set := flag.NewFlagSet("toolchain", flag.ContinueOnError)
	tools := addToolFlags(set)
	if !parse(set, args) {
		return exitUsage
	}
	chain, _, err := tools.collect(ctx)
	if err != nil {
		return fail(err)
	}
	data, _ := json.MarshalIndent(struct {
		Machine   host.Machine        `json:"machine"`
		Toolchain toolchain.Toolchain `json:"toolchain"`
	}{host.Collect(ctx, env("SEVENZ_BENCH_MACHINE", hostname())), chain}, "", "  ")
	fmt.Println(string(data))
	return exitOK
}

func hostname() string {
	name, err := os.Hostname()
	if err != nil {
		return runtime.GOOS + "-" + runtime.GOARCH
	}
	return name
}

func cmdRun(ctx context.Context, args []string) int {
	set := flag.NewFlagSet("run", flag.ContinueOnError)
	tools := addToolFlags(set)
	out := set.String("out", "", "results directory (raw.json, report.json, report.md)")
	dir := set.String("dir", "", "corpus directory (default bench/fixtures/<the profile's corpus>, or $SEVENZ_BENCH_FIXTURES)")
	profileName := set.String("profile", "", "run profile: quick (the smoke subset over the quick corpus: threads 1/all, levels 1/5, 2 repeats, no warmup), "+
		"full (every scenario over the full corpus, 5 repeats, 1 warmup; the default) or "+
		"fleet (every scenario over the full corpus, 3 repeats, 1 warmup)")
	quick := set.Bool("quick", false, "the same as --profile quick")
	list := set.Bool("list", false, "print the planned scenarios and the plan's size, then exit without running")
	machine := set.String("machine", env("SEVENZ_BENCH_MACHINE", hostname()), "host label in the report (e.g. c7i.4xlarge-us-east-1)")
	repeats := set.Int("repeats", -1, "measured runs per variant (default: the profile's, full 5, fleet 3, quick 2)")
	warmups := set.Int("warmups", -1, "discarded runs per variant before the measured ones (default: the profile's, full and fleet 1, quick 0)")
	only := set.String("only", "", "comma-separated substrings; run only scenarios whose id contains one")
	pin := set.String("pin-cpus", env("SEVENZ_BENCH_PIN_CPUS", ""), "inclusive CPU range to confine every process to (Linux taskset, Windows affinity)")
	timeout := set.Duration("timeout", time.Hour, "per-process bound; a run past it is recorded as DNF")
	if !parse(set, args) {
		return exitUsage
	}
	if *quick {
		if *profileName != "" && *profileName != suite.ProfileQuick {
			fmt.Fprintf(os.Stderr, "run: --quick and --profile %s disagree\n", *profileName)
			return exitUsage
		}
		*profileName = suite.ProfileQuick
	}
	if *profileName == "" {
		*profileName = suite.ProfileFull
	}
	profile, err := suite.ProfileByName(*profileName)
	if err != nil {
		fmt.Fprintf(os.Stderr, "run: %v\n", err)
		return exitUsage
	}
	*quick = profile.Quick
	if *out == "" && !*list {
		fmt.Fprintln(os.Stderr, "run: --out is required")
		return exitUsage
	}
	if *repeats < 0 {
		*repeats = profile.Repeats
	}
	if *warmups < 0 {
		*warmups = profile.Warmups
	}
	if *repeats < 1 {
		fmt.Fprintln(os.Stderr, "run: --repeats must be at least 1")
		return exitUsage
	}
	// Thread counts follow the CPUs the processes may use: the pinned range
	// when --pin-cpus confines them, else every CPU.
	cpus := runtime.NumCPU()
	if *pin != "" {
		if !procmeasure.PinSupported() {
			fmt.Fprintf(os.Stderr, "run: --pin-cpus is not supported on %s (Linux with taskset, or Windows)\n", runtime.GOOS)
			return exitUsage
		}
		count, err := procmeasure.PinCount(*pin)
		if err != nil {
			fmt.Fprintf(os.Stderr, "run: --pin-cpus: %v\n", err)
			return exitUsage
		}
		cpus = count
	}
	if *dir == "" {
		*dir = defaultFixtureDir(profile.Corpus == "quick")
	}
	manifest, err := fixtures.Load(*dir)
	if err != nil {
		return fail(prerequisite{fmt.Errorf("fixtures in %s: %w (run `sevenz-turbo-bench fixtures` first)", *dir, err)})
	}
	if manifest.Profile != profile.Corpus {
		fmt.Fprintf(os.Stderr, "run: note: profile %s over the %s corpus in %s\n", profile.Name, manifest.Profile, *dir)
	}
	// A run measures the corpus as the manifest records it: every source and
	// archive is rehashed before planning. --list measures nothing and skips it.
	if !*list {
		if err := manifest.Verify(*dir); err != nil {
			return fail(prerequisite{fmt.Errorf("fixtures in %s: %w", *dir, err)})
		}
	}
	chain, binaries, err := tools.collect(ctx)
	if err != nil {
		return fail(err)
	}
	absolute, err := filepath.Abs(*dir)
	if err != nil {
		return fail(err)
	}
	// --list plans against a scratch path it never creates.
	scratch := filepath.Join(os.TempDir(), "sevenz-turbo-bench-list-scratch")
	if !*list {
		scratch = filepath.Join(*out, "scratch")
		// The run creates this directory and removes it when it finishes,
		// so one that already exists was not ours and may hold the
		// operator's files; refuse it rather than delete it.
		if _, err := os.Stat(scratch); err == nil {
			return fail(fmt.Errorf("%s already exists; remove it or choose another --out", scratch))
		} else if !os.IsNotExist(err) {
			return fail(err)
		}
		if err := os.MkdirAll(scratch, 0o755); err != nil {
			return fail(err)
		}
		// From here the directory is the run's, so it goes whichever way
		// the run ends: a planning failure must not leave it to refuse the
		// next run with the same --out.
		defer func() { _ = os.RemoveAll(scratch) }()
	}
	scratch, _ = filepath.Abs(scratch)
	settings := suite.DefaultSettings(*quick, cpus)
	if *only != "" {
		settings.Only = strings.Split(*only, ",")
	}
	scenarios, err := suite.Plan(manifest, absolute, scratch, binaries, settings)
	if err != nil {
		return fail(prerequisite{err})
	}
	planLine := fmt.Sprintf("profile %s over the %s corpus: %d scenarios, %d processes at %d repeat(s) + %d warmup(s)",
		profile.Name, manifest.Profile, len(scenarios), suite.Processes(scenarios, *repeats, *warmups), *repeats, *warmups)
	if *list {
		for _, scenario := range scenarios {
			fmt.Printf("%s (%d variants)\n", scenario.ID, len(scenario.Variants))
		}
		fmt.Println(planLine)
		return exitOK
	}
	fmt.Fprintf(os.Stderr, "run: %s\n", planLine)
	raw := &suite.Raw{
		SchemaVersion: 1, Schema: suite.RawSchema, StartedUTC: time.Now().UTC().Format(time.RFC3339),
		Machine: host.Collect(ctx, *machine), Toolchain: chain, Fixtures: manifest, RunProfile: profile.Name, Quick: *quick,
		Warmups: *warmups, Repeats: *repeats, Threads: settings.Threads, PinCPUs: *pin,
		Only: settings.Only, TimeoutSeconds: timeout.Seconds(), Scenarios: scenarios, Runs: []suite.RunRecord{},
	}
	fmt.Fprintf(os.Stderr, "run: %d scenarios on %s (%s/%s, %d cores), 7zz %s, lzma-turbo %s\n", len(scenarios), raw.Machine.Label,
		raw.Machine.OS, raw.Machine.Architecture, raw.Machine.CPUCount, chain.Oracle.Version, chain.LinkedLzmaTurbo)
	suite.Execute(ctx, raw, suite.Options{Warmups: *warmups, Repeats: *repeats, PinCPUs: *pin, Timeout: *timeout, Log: os.Stderr, Oracle: binaries.Oracle})
	raw.FinishedUTC = time.Now().UTC().Format(time.RFC3339)
	if err := suite.Write(filepath.Join(*out, "raw.json"), raw); err != nil {
		return fail(err)
	}
	built := report.Build(raw)
	if err := writeReport(built, filepath.Join(*out, "report.json"), filepath.Join(*out, "report.md")); err != nil {
		return fail(err)
	}
	fmt.Printf("wrote %s, %s, %s\n", filepath.Join(*out, "raw.json"), filepath.Join(*out, "report.json"), filepath.Join(*out, "report.md"))
	if ctx.Err() != nil {
		fmt.Fprintln(os.Stderr, "run: interrupted")
		return exitFailed
	}
	if len(built.Failures) > 0 {
		fmt.Fprintf(os.Stderr, "run: %d failed runs (see report.md)\n", len(built.Failures))
		return exitFailed
	}
	return exitOK
}

func writeReport(built *report.Report, jsonPath, mdPath string) error {
	if err := report.Write(jsonPath, built); err != nil {
		return err
	}
	return os.WriteFile(mdPath, []byte(report.Markdown(built)), 0o644)
}

func cmdReport(args []string) int {
	set := flag.NewFlagSet("report", flag.ContinueOnError)
	input := set.String("input", "", "raw.json from run")
	out := set.String("out", "", "report.json to write")
	md := set.String("md", "", "report.md to write (default: next to --out)")
	if !parse(set, args) {
		return exitUsage
	}
	if *input == "" || *out == "" {
		fmt.Fprintln(os.Stderr, "report: --input and --out are required")
		return exitUsage
	}
	raw, err := suite.Load(*input)
	if err != nil {
		return fail(prerequisite{err})
	}
	if *md == "" {
		*md = strings.TrimSuffix(*out, filepath.Ext(*out)) + ".md"
	}
	built := report.Build(raw)
	if err := writeReport(built, *out, *md); err != nil {
		return fail(err)
	}
	fmt.Printf("wrote %s, %s\n", *out, *md)
	return exitOK
}

func cmdMerge(args []string) int {
	set := flag.NewFlagSet("merge", flag.ContinueOnError)
	out := set.String("out", "", "merged report.md to write")
	if !parse(set, args) {
		return exitUsage
	}
	if *out == "" || set.NArg() == 0 {
		fmt.Fprintln(os.Stderr, "merge: --out and at least one report.json are required")
		return exitUsage
	}
	var reports []*report.Report
	for _, path := range set.Args() {
		loaded, err := report.Load(path)
		if err != nil {
			return fail(prerequisite{err})
		}
		reports = append(reports, loaded)
	}
	merged, err := report.Merge(reports)
	if err != nil {
		return fail(prerequisite{err})
	}
	if err := os.WriteFile(*out, []byte(merged), 0o644); err != nil {
		return fail(err)
	}
	fmt.Printf("wrote %s (%d hosts)\n", *out, len(reports))
	return exitOK
}
