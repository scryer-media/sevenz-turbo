package suite

import (
	"context"
	"fmt"
	"os"
	"slices"
	"testing"
)

// TestMain doubles as a stand-in decode-bench: with SUITE_FAKE_DECODE set the
// test binary prints one op JSON line, with a digest only when --digest asked.
func TestMain(m *testing.M) {
	if digest := os.Getenv("SUITE_FAKE_DECODE"); digest != "" {
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
