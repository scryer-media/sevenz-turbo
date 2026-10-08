package suite

import (
	"context"
	"fmt"
	"os"
	"path/filepath"
	"slices"
	"strings"
	"testing"

	"github.com/scryer-media/sevenz-turbo/bench/sevenz-turbo-bench/internal/fixtures"
)

// TestMain doubles as a stand-in decode-bench: with SUITE_FAKE_DECODE set the
// test binary prints one op JSON line, with a digest only when --digest asked.
func TestMain(m *testing.M) {
	if digest := os.Getenv("SUITE_FAKE_DECODE"); digest != "" {
		if path := os.Getenv("SUITE_FAKE_LOG"); path != "" {
			// One line per invocation: which kind of run, and its last
			// measured argument (the variant's tag).
			kind, tag := "measured", os.Args[len(os.Args)-1]
			if tag == "--digest" {
				kind, tag = "digest", os.Args[len(os.Args)-2]
			}
			file, err := os.OpenFile(path, os.O_APPEND|os.O_CREATE|os.O_WRONLY, 0o644)
			if err != nil {
				os.Exit(3)
			}
			fmt.Fprintf(file, "%s %s\n", kind, tag)
			_ = file.Close()
		}
		if slices.Contains(os.Args, "--digest") {
			fmt.Printf("{\"ok\":true,\"bytes_out\":4,\"digest\":%q}\n", digest)
		} else {
			fmt.Println("{\"ok\":true,\"bytes_out\":4}")
		}
		os.Exit(0)
	}
	os.Exit(m.Run())
}

func TestOutputDigestAsksForTheDigestUntimed(t *testing.T) {
	t.Setenv("SUITE_FAKE_DECODE", "00112233aabbccdd")
	run := Run{Variant: VariantTurbo, Role: RoleCandidate, Tool: os.Args[0], Args: []string{"op", "decode"}, JSON: true}
	digest, err := outputDigest(context.Background(), run, 0)
	if err != nil || digest != "00112233aabbccdd" {
		t.Fatalf("digest %q, %v", digest, err)
	}
	if !slices.Equal(run.Args, []string{"op", "decode"}) {
		t.Fatalf("the measured command was changed: %v", run.Args)
	}
}

func TestCheckDigestComparesTheUntimedDigests(t *testing.T) {
	scenario := Scenario{ID: "decode/mt/T1", Op: OpDecode, Fixture: "mt.7z"}
	digests := map[string]string{}
	first := RunRecord{Status: StatusOK, Digest: "aa"}
	checkDigest(digests, scenario, &first)
	same := RunRecord{Status: StatusOK, Digest: "aa"}
	checkDigest(digests, scenario, &same)
	if first.Status != StatusOK || same.Status != StatusOK {
		t.Fatalf("matching digests failed: %+v %+v", first, same)
	}
	// A timed row carries no digest and is not compared.
	timed := RunRecord{Status: StatusOK}
	checkDigest(digests, scenario, &timed)
	if timed.Status != StatusOK {
		t.Fatalf("a row without a digest failed: %+v", timed)
	}
	other := RunRecord{Status: StatusOK, Digest: "bb"}
	checkDigest(digests, scenario, &other)
	if other.Status != StatusFailed || other.Failure != "digest-mismatch" {
		t.Fatalf("a mismatch passed: %+v", other)
	}
}

func TestAListMustReportTheWholeFixture(t *testing.T) {
	raw := &Raw{Fixtures: &fixtures.Manifest{Archives: []fixtures.ArchiveRecord{
		{ArchiveSpec: fixtures.ArchiveSpec{Name: "mt.7z"}, Bytes: 10, UnpackedBytes: 100},
	}}}
	scenario := Scenario{ID: "list/mt", Op: OpList, Fixture: "mt.7z"}
	run := Run{Variant: VariantTurbo, Role: RoleCandidate, JSON: true}

	whole := RunRecord{Status: StatusOK, Result: map[string]any{"ok": true, "bytes_out": float64(100)}}
	fillBytes(raw, scenario, run, &whole)
	if whole.Status != StatusOK || whole.BytesIn != 10 || whole.BytesOut != 0 {
		t.Fatalf("a complete list: %+v", whole)
	}

	short := RunRecord{Status: StatusOK, Result: map[string]any{"ok": true, "bytes_out": float64(60)}}
	fillBytes(raw, scenario, run, &short)
	if short.Status != StatusFailed || short.Failure != "short-output" {
		t.Fatalf("a list missing members passed: %+v", short)
	}

	oracle := RunRecord{Status: StatusOK}
	fillBytes(raw, scenario, Run{Variant: VariantOracle, Role: RoleReference}, &oracle)
	if oracle.Status != StatusOK {
		t.Fatalf("7zz l, which reports no JSON, failed: %+v", oracle)
	}
}

// fakeDecodes is a run of `scenarios` decode scenarios over one 4-byte
// fixture, each with a candidate, a secondary and an oracle variant, all
// played by this test binary, which logs every invocation to the returned
// path.
func fakeDecodes(t *testing.T, scenarios int) (*Raw, string) {
	t.Helper()
	log := filepath.Join(t.TempDir(), "invocations")
	t.Setenv("SUITE_FAKE_DECODE", "00112233aabbccdd")
	t.Setenv("SUITE_FAKE_LOG", log)
	raw := &Raw{Fixtures: &fixtures.Manifest{Archives: []fixtures.ArchiveRecord{
		{ArchiveSpec: fixtures.ArchiveSpec{Name: "a.7z"}, Bytes: 4, UnpackedBytes: 4},
	}}}
	for i := range scenarios {
		raw.Scenarios = append(raw.Scenarios, Scenario{
			ID: fmt.Sprintf("decode/a/T%d", i+1), Op: OpDecode, Fixture: "a.7z",
			Variants: []Run{
				{Variant: VariantTurbo, Role: RoleCandidate, Tool: os.Args[0], Args: []string{"turbo"}, JSON: true},
				{Variant: VariantTurboNative, Role: RoleCandidate, Tool: os.Args[0], Args: []string{"native"}, JSON: true},
				{Variant: VariantOracle, Role: RoleReference, Tool: os.Args[0], Args: []string{"oracle"}},
			},
		})
	}
	return raw, log
}

func invocations(t *testing.T, path string) []string {
	t.Helper()
	data, err := os.ReadFile(path)
	if os.IsNotExist(err) {
		return nil
	}
	if err != nil {
		t.Fatal(err)
	}
	return strings.Fields(strings.ReplaceAll(string(data), " ", "_"))
}

func TestTheUntimedChecksWaitForTheWholePass(t *testing.T) {
	raw, log := fakeDecodes(t, 1)
	Execute(context.Background(), raw, Options{Repeats: 2})
	got := invocations(t, log)
	want := []string{
		// The first measured pass, then its digest reruns: none of them
		// sits between two measured variants.
		"measured_turbo", "measured_native", "measured_oracle",
		"digest_turbo", "digest_native",
		// The second pass, reversed, has no untimed checks.
		"measured_oracle", "measured_native", "measured_turbo",
	}
	if !slices.Equal(got, want) {
		t.Fatalf("invocations %v, want %v", got, want)
	}
	if len(raw.Runs) != 6 {
		t.Fatalf("%d runs recorded, want 6", len(raw.Runs))
	}
	for _, record := range raw.Runs[:2] {
		if record.Status != StatusOK || record.Digest != "00112233aabbccdd" {
			t.Fatalf("a checked run: %+v", record)
		}
	}
}

// cancelOn cancels a run when the log announces the given scenario.
type cancelOn struct {
	prefix string
	cancel context.CancelFunc
}

func (c cancelOn) Write(p []byte) (int, error) {
	if strings.HasPrefix(string(p), c.prefix) {
		c.cancel()
	}
	return len(p), nil
}

func TestAnInterruptedRunStopsAtOnce(t *testing.T) {
	raw, log := fakeDecodes(t, 3)
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	Execute(ctx, raw, Options{Repeats: 1, Log: cancelOn{prefix: "[2/3]", cancel: cancel}})
	// The first scenario ran whole; nothing after the interrupt was started
	// or recorded.
	if got := invocations(t, log); len(got) != 5 {
		t.Fatalf("invocations %v, want the first scenario's 3 runs and 2 digests", got)
	}
	if len(raw.Runs) != 3 {
		t.Fatalf("%d runs recorded, want the first scenario's 3", len(raw.Runs))
	}
	for _, record := range raw.Runs {
		if record.Scenario != "decode/a/T1" || record.Status != StatusOK {
			t.Fatalf("recorded after the interrupt or failed: %+v", record)
		}
	}

	// Interrupted before it starts, a run starts nothing.
	raw, log = fakeDecodes(t, 1)
	Execute(ctx, raw, Options{Repeats: 1})
	if got := invocations(t, log); len(got) != 0 || len(raw.Runs) != 0 {
		t.Fatalf("a cancelled run started %v and recorded %d runs", got, len(raw.Runs))
	}
}

func TestAnEncodeMustTakeTheWholeSource(t *testing.T) {
	output := filepath.Join(t.TempDir(), "out.7z")
	if err := os.WriteFile(output, []byte("7z"), 0o644); err != nil {
		t.Fatal(err)
	}
	raw := &Raw{Fixtures: &fixtures.Manifest{Sources: []fixtures.SourceRecord{
		{SourceSpec: fixtures.SourceSpec{Name: "tree"}, TotalBytes: 100, FileCount: 3},
	}}}
	scenario := Scenario{ID: "encode/tree/L5", Op: OpEncode, Fixture: "tree"}
	run := Run{Variant: VariantTurbo, Role: RoleCandidate, Output: output, JSON: true}
	result := func(files, bytes float64) map[string]any {
		return map[string]any{"ok": true, "files": files, "bytes_in": bytes}
	}

	whole := RunRecord{Status: StatusOK, Result: result(3, 100)}
	fillBytes(raw, scenario, run, &whole)
	if whole.Status != StatusOK || whole.BytesIn != 100 || whole.BytesOut != 2 {
		t.Fatalf("a whole encode: %+v", whole)
	}
	for name, short := range map[string]map[string]any{
		"a member skipped": result(2, 60),
		"bytes missing":    result(3, 90),
	} {
		record := RunRecord{Status: StatusOK, Result: short}
		fillBytes(raw, scenario, run, &record)
		if record.Status != StatusFailed || record.Failure != "short-input" {
			t.Fatalf("%s passed: %+v", name, record)
		}
	}

	// 7zz a reports no JSON and is not checked this way.
	oracle := RunRecord{Status: StatusOK}
	fillBytes(raw, scenario, Run{Variant: VariantOracle, Role: RoleReference, Output: output}, &oracle)
	if oracle.Status != StatusOK {
		t.Fatalf("7zz a failed: %+v", oracle)
	}
}
