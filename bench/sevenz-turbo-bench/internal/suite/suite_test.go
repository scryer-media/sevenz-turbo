package suite

import (
	"os"
	"path/filepath"
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
			if !groups[group] {
				t.Errorf("quick=%t: no scenario in group %q", quick, group)
			}
		}
		for _, want := range []string{"decode/mt/T1", "decode/mt/Tall", "list/tree_solid", "decode/aes_kdf/T1", "encode/payload-sub/L1/T1"} {
			if !ids[want] {
				t.Errorf("quick=%t: missing %s", quick, want)
			}
		}
		if !quick {
			for _, want := range []string{"decode/mt/T2", "decode/mt/T16", "encode/payload-sub/L9/Tall", "encode/payload-sub/L5/T8"} {
				if !ids[want] {
					t.Errorf("full: missing %s", want)
				}
			}
		}
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
		all := ResolveThreads("all")
		for _, want := range []string{"--level 5", "--threads " + all, "--non-solid"} {
			if !strings.Contains(ours, want) {
				t.Errorf("ours %q lacks %q", ours, want)
			}
		}
		for _, want := range []string{"-mx=5", "-mmt=" + all, "-ms=off", "-m0=lzma2"} {
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
