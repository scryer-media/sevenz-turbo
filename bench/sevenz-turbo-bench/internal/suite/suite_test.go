package suite

import (
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"testing"

	"github.com/scryer-media/sevenz-turbo/bench/sevenz-turbo-bench/internal/fixtures"
)

// fakeCorpus lays out a manifest for a profile without generating anything:
// the planner only needs the records and each source's top-level entries.
func fakeCorpus(t *testing.T, profile fixtures.Profile) (string, *fixtures.Manifest) {
	t.Helper()
	dir := t.TempDir()
	manifest := &fixtures.Manifest{SchemaVersion: 1, Profile: profile.Name}
	for _, source := range profile.Sources {
		root := fixtures.SourceDir(dir, source.Name)
		if err := os.MkdirAll(root, 0o755); err != nil {
			t.Fatal(err)
		}
		if err := os.WriteFile(filepath.Join(root, source.Name+".bin"), []byte("x"), 0o644); err != nil {
			t.Fatal(err)
		}
		manifest.Sources = append(manifest.Sources, fixtures.SourceRecord{SourceSpec: source, TotalBytes: 1, FileCount: 1})
	}
	for _, archive := range profile.Archives {
		manifest.Archives = append(manifest.Archives, fixtures.ArchiveRecord{ArchiveSpec: archive, Path: archive.Name, Bytes: 1, UnpackedBytes: 1})
	}
	return dir, manifest
}

func TestPlanCoversTheMatrix(t *testing.T) {
	for _, quick := range []bool{true, false} {
		profile := fixtures.Full()
		if quick {
			profile = fixtures.Quick()
		}
		dir, manifest := fakeCorpus(t, profile)
		tools := Tools{Candidate: "decode-bench", Native: "decode-bench-native", Oracle: "7zz"}
		scenarios, err := Plan(manifest, dir, t.TempDir(), tools, DefaultSettings(quick, 32))
		if err != nil {
			t.Fatal(err)
		}
		groups := map[string]bool{}
		ids := map[string]bool{}
		for _, scenario := range scenarios {
			if ids[scenario.ID] {
				t.Fatalf("duplicate scenario %s", scenario.ID)
			}
			ids[scenario.ID] = true
			groups[scenario.Group] = true
			var candidate, reference bool
			for _, run := range scenario.Variants {
				candidate = candidate || run.Role == RoleCandidate
				reference = reference || run.Variant == VariantOracle
				if run.Variant == VariantTurboNative && !scenario.Encrypted {
					t.Errorf("%s: native-crypto row on an unencrypted scenario", scenario.ID)
				}
				if run.Variant == VariantOracle && scenario.Op == OpEncode && run.Dir == "" {
					t.Errorf("%s: 7zz a without a working directory", scenario.ID)
				}
			}
			if !candidate || !reference {
				t.Errorf("%s: candidate=%t reference=%t", scenario.ID, candidate, reference)
			}
		}
		for _, group := range Groups {
			want := !quick || !FullOnlyGroups[group]
			if groups[group] != want {
				t.Errorf("quick=%t: group %q planned=%t, want %t", quick, group, groups[group], want)
			}
		}
		for _, want := range []string{"decode/mt/T1", "decode/mt/Tall", "list/tree_solid", "decode/aes_kdf/T1", "encode/payload-sub/L1/T1", "decode/mt/Tall/adaptive"} {
			if !ids[want] {
				t.Errorf("quick=%t: missing %s", quick, want)
			}
		}
		if !quick {
			for _, want := range []string{"decode/mt/T2", "decode/mt/T16", "encode/payload-sub/L9/Tall", "encode/payload-sub/L5/T8",
				"decode/media_mx1/T1", "decode/media_mx5/Tall", "decode/media_mx5/Tall/adaptive", "decode/media_mx5/Tall/adaptive/budget-4096MiB"} {
				if !ids[want] {
					t.Errorf("full: missing %s", want)
				}
			}
		}
		for id := range ids {
			if quick && strings.HasSuffix(id, "/ledger") {
				t.Errorf("quick plans the ledger row %s", id)
			}
		}
	}
}

// The one-thread BCJ2 row has two references: 7zz -mmt=1, and 7zz -mmt=1
// -mmtf=off, which takes away the thread 7-Zip gives the BCJ2 stage. No other
// row has the second. Both profiles plan it.
func TestBCJ2RowHasBothOneThreadReferences(t *testing.T) {
	for _, quick := range []bool{true, false} {
		profile := fixtures.Full()
		if quick {
			profile = fixtures.Quick()
		}
		dir, manifest := fakeCorpus(t, profile)
		scenarios, err := Plan(manifest, dir, t.TempDir(), Tools{Candidate: "decode-bench", Oracle: "7zz"}, DefaultSettings(quick, 32))
		if err != nil {
			t.Fatal(err)
		}
		found := false
		for _, scenario := range scenarios {
			references := map[string]string{}
			for _, run := range scenario.Variants {
				if run.Role == RoleReference {
					references[run.Variant] = strings.Join(run.Args[:len(run.Args)-1], " ")
				}
			}
			if scenario.ID != "decode/bcj2/T1" {
				if _, ok := references[VariantOracleOneThread]; ok {
					t.Errorf("quick=%t: %s has the one-thread reference", quick, scenario.ID)
				}
				continue
			}
			found = true
			want := map[string]string{VariantOracle: "t -bso0 -bsp0 -mmt=1", VariantOracleOneThread: "t -bso0 -bsp0 -mmt=1 -mmtf=off"}
			if len(references) != len(want) {
				t.Errorf("quick=%t: references %v, want %v", quick, references, want)
			}
			for variant, args := range want {
				if references[variant] != args {
					t.Errorf("quick=%t: %s runs %q, want %q", quick, variant, references[variant], args)
				}
			}
		}
		if !found {
			t.Errorf("quick=%t: decode/bcj2/T1 not planned", quick)
		}
	}
}

// The ledger group is every ledger fixture at 2, 4, 8 and all threads with no
// limit and under each ledger limit, and the controls at 2, 4 and 8.
func TestLedgerRows(t *testing.T) {
	dir, manifest := fakeCorpus(t, fixtures.Full())
	tools := Tools{Candidate: "decode-bench", Native: "decode-bench-native", Oracle: "7zz"}
	scenarios, err := Plan(manifest, dir, t.TempDir(), tools, DefaultSettings(false, 18))
	if err != nil {
		t.Fatal(err)
	}
	rows := map[string]Scenario{}
	for _, scenario := range scenarios {
		if scenario.Group == GroupLedger {
			rows[scenario.ID] = scenario
		} else if scenario.Ledger {
			t.Errorf("%s keeps a ledger outside the ledger group", scenario.ID)
		}
	}
	if want := len(LedgerFixtures)*4*(1+len(LedgerLimits)) + len(LedgerControls)*3; len(rows) != want {
		t.Fatalf("%d ledger rows, want %d", len(rows), want)
	}
	for _, name := range []string{"media_mx5_3g", "media_mx5_2g", "mt"} {
		for _, threads := range []string{"2", "4", "8", "all"} {
			for _, limit := range []string{"", "/budget-512MiB", "/budget-553MiB", "/budget-1024MiB", "/budget-1065MiB", "/budget-2089MiB"} {
				id := "decode/" + name + "/T" + threads + limit + "/ledger"
				if _, ok := rows[id]; !ok {
					t.Errorf("missing %s", id)
				}
			}
		}
	}
	for id, scenario := range rows {
		if !scenario.Ledger {
			t.Errorf("%s: not marked as a ledger row", id)
		}
		byVariant := map[string]Run{}
		for _, run := range scenario.Variants {
			byVariant[run.Variant] = run
		}
		ours := strings.Join(byVariant[VariantTurbo].Args, " ")
		n := DefaultSettings(false, 18).ResolveThreads(scenario.Threads)
		if !strings.Contains(ours, "--ledger") || !strings.Contains(ours, "--threads "+n) {
			t.Errorf("%s: candidate runs %q", id, ours)
		}
		if oracle := strings.Join(byVariant[VariantOracle].Args, " "); !strings.Contains(oracle, "-mmt="+n+" ") {
			t.Errorf("%s: 7zz runs %q, want -mmt=%s", id, oracle, n)
		}
		// The unobserved twin runs where no limit is set, and is the same
		// command without the ledger.
		plain, twin := byVariant[VariantTurboPlain]
		if twin != (scenario.MemoryLimit == 0) {
			t.Errorf("%s: limit %d, no-ledger twin planned=%t", id, scenario.MemoryLimit, twin)
		}
		if twin {
			if got := strings.Join(plain.Args, " "); got != strings.Replace(ours, " --ledger", "", 1) || plain.Role != RoleCandidate || !plain.JSON {
				t.Errorf("%s: twin runs %q beside %q", id, got, ours)
			}
		}
		if scenario.MemoryLimit > 0 && !strings.Contains(ours, "--memory-limit "+strconv.FormatInt(scenario.MemoryLimit, 10)) {
			t.Errorf("%s: candidate runs %q without its limit", id, ours)
		}
		if _, native := byVariant[VariantTurboNative]; native != scenario.Encrypted {
			t.Errorf("%s: encrypted=%t, native-crypto row planned=%t", id, scenario.Encrypted, native)
		}
	}
	for _, id := range []string{"decode/media_mx1/T2/ledger", "decode/media_mx1/T8/ledger", "decode/aes_mx1/T4/ledger"} {
		if _, ok := rows[id]; !ok {
			t.Errorf("missing control %s", id)
		}
	}
	if _, ok := rows["decode/media_mx1/Tall/ledger"]; ok {
		t.Error("a control is planned at all threads")
	}

	// A fixed thread count above the usable cores is left out; all threads is
	// kept and resolves to what there is.
	narrow, err := Plan(manifest, dir, t.TempDir(), tools, DefaultSettings(false, 4))
	if err != nil {
		t.Fatal(err)
	}
	for _, scenario := range narrow {
		if scenario.Group != GroupLedger {
			continue
		}
		if scenario.Threads == "8" {
			t.Errorf("%s planned on four cores", scenario.ID)
		}
		if scenario.Threads == "all" && !strings.Contains(strings.Join(scenario.Variants[0].Args, " "), "--threads 4") {
			t.Errorf("%s: all threads is not four", scenario.ID)
		}
	}
}

// --only selects ledger rows exactly: a row with no limit does not bring the
// limited rows of the same fixture and thread count with it.
func TestOnlySelectsOneLedgerRow(t *testing.T) {
	dir, manifest := fakeCorpus(t, fixtures.Full())
	settings := DefaultSettings(false, 18)
	settings.Only = []string{"decode/media_mx5_3g/T4/ledger"}
	scenarios, err := Plan(manifest, dir, t.TempDir(), Tools{Candidate: "c", Oracle: "o"}, settings)
	if err != nil {
		t.Fatal(err)
	}
	if len(scenarios) != 1 || scenarios[0].ID != "decode/media_mx5_3g/T4/ledger" {
		t.Fatalf("planned %d scenarios: %+v", len(scenarios), scenarios)
	}
}

func TestPlanSkipsNativeWithoutTheBinary(t *testing.T) {
	dir, manifest := fakeCorpus(t, fixtures.Quick())
	scenarios, err := Plan(manifest, dir, t.TempDir(), Tools{Candidate: "c", Oracle: "o"}, DefaultSettings(true, 4))
	if err != nil {
		t.Fatal(err)
	}
	for _, scenario := range scenarios {
		for _, run := range scenario.Variants {
			if run.Variant == VariantTurboNative {
				t.Fatalf("%s has a native-crypto row with no native binary", scenario.ID)
			}
		}
	}
}

func TestPlanEncodeArgsMatch7zz(t *testing.T) {
	dir, manifest := fakeCorpus(t, fixtures.Quick())
	scenarios, err := Plan(manifest, dir, t.TempDir(), Tools{Candidate: "c", Oracle: "o"}, DefaultSettings(true, 4))
	if err != nil {
		t.Fatal(err)
	}
	for _, scenario := range scenarios {
		if scenario.ID != "encode/tree/L5/Tall/non-solid" {
			continue
		}
		ours := strings.Join(scenario.Variants[0].Args, " ")
		oracle := strings.Join(scenario.Variants[1].Args, " ")
		all := DefaultSettings(true, 4).ResolveThreads("all")
		for _, want := range []string{"--level 5", "--threads " + all, "--non-solid"} {
			if !strings.Contains(ours, want) {
				t.Errorf("ours %q lacks %q", ours, want)
			}
		}
		for _, want := range []string{"-mx=5", "-mmt=" + all, "-ms=off", "-m0=lzma2:d=8m:fb=32:mf=bt4:a=1"} {
			if !strings.Contains(oracle, want) {
				t.Errorf("7zz %q lacks %q", oracle, want)
			}
		}
		return
	}
	t.Fatal("scenario not planned")
}

func TestOrderForAlternates(t *testing.T) {
	if got := orderFor(3, 0); got[0] != 0 || got[2] != 2 {
		t.Fatalf("repeat 0: %v", got)
	}
	if got := orderFor(3, 1); got[0] != 2 || got[2] != 0 {
		t.Fatalf("repeat 1: %v", got)
	}
}

func TestDefaultSettingsSweep(t *testing.T) {
	got := DefaultSettings(false, 8).Threads
	if strings.Join(got, ",") != "1,2,4,all" {
		t.Fatalf("8 cores: %v", got)
	}
	if got := DefaultSettings(true, 64).Threads; strings.Join(got, ",") != "1,all" {
		t.Fatalf("quick: %v", got)
	}
}

func TestRunProfiles(t *testing.T) {
	for name, want := range map[string]RunProfile{
		ProfileQuick: {Name: ProfileQuick, Quick: true, Corpus: "quick", Repeats: 2, Warmups: 0},
		ProfileFull:  {Name: ProfileFull, Corpus: "full", Repeats: 5, Warmups: 1},
		ProfileFleet: {Name: ProfileFleet, Corpus: "full", Repeats: 3, Warmups: 1},
	} {
		got, err := ProfileByName(name)
		if err != nil || got != want {
			t.Errorf("%s = %+v, %v; want %+v", name, got, err, want)
		}
	}
	if _, err := ProfileByName("nightly"); err == nil {
		t.Error("an unknown profile is accepted")
	}
	scenarios := []Scenario{{Variants: make([]Run, 2)}, {Variants: make([]Run, 4)}}
	if n := Processes(scenarios, 3, 1); n != 24 {
		t.Errorf("Processes = %d, want 24", n)
	}
}

// "all" is the usable core count the settings were built with (the pinned
// range's under --pin-cpus), not the host's.
func TestAllResolvesToTheSettingsCPUs(t *testing.T) {
	dir, manifest := fakeCorpus(t, fixtures.Quick())
	scenarios, err := Plan(manifest, dir, t.TempDir(), Tools{Candidate: "c", Oracle: "o"}, DefaultSettings(true, 3))
	if err != nil {
		t.Fatal(err)
	}
	for _, scenario := range scenarios {
		if scenario.Threads != "all" {
			continue
		}
		for _, run := range scenario.Variants {
			args := strings.Join(run.Args, " ")
			if !strings.Contains(args, "--threads 3") && !strings.Contains(args, "-mmt=3") {
				t.Errorf("%s / %s: %q does not ask for 3 threads", scenario.ID, run.Variant, args)
			}
		}
	}
}

// --only applies before fixtures are looked up: a corpus holding only mt.7z
// plans the mt rows, and still refuses a selection that needs a missing one.
func TestOnlyFiltersBeforeFixturesAreRequired(t *testing.T) {
	dir, manifest := fakeCorpus(t, fixtures.Quick())
	mt, _ := manifest.Archive("mt.7z")
	manifest.Archives = []fixtures.ArchiveRecord{mt}
	manifest.Sources = nil
	settings := DefaultSettings(true, 4)
	settings.Only = []string{"decode/mt"}
	scenarios, err := Plan(manifest, dir, t.TempDir(), Tools{Candidate: "c", Oracle: "o"}, settings)
	if err != nil {
		t.Fatal(err)
	}
	if len(scenarios) == 0 {
		t.Fatal("no scenarios planned")
	}
	for _, scenario := range scenarios {
		if !strings.HasPrefix(scenario.ID, "decode/mt/") {
			t.Errorf("unselected scenario %s planned", scenario.ID)
		}
	}
	settings.Only = []string{"decode/st"}
	if _, err := Plan(manifest, dir, t.TempDir(), Tools{Candidate: "c", Oracle: "o"}, settings); err == nil {
		t.Error("a selection whose fixture is missing was planned")
	}
}
