package host

import "testing"

func TestParseCPUInfoFlags(t *testing.T) {
	x86 := "processor\t: 0\nflags\t\t: fpu sse4_2 avx2 avx512f avx512_vbmi2 gfni vaes sha_ni\n"
	got := ParseCPUInfoFlags(x86)
	for _, flag := range []string{"sse4.2", "avx2", "avx512f", "avx512vbmi2", "gfni", "vaes", "sha_ni"} {
		if !got[flag] {
			t.Errorf("x86: missing %s in %v", flag, got)
		}
	}
	if got["fpu"] {
		t.Error("x86: unrequested flag reported")
	}
	arm := "processor\t: 0\nFeatures\t: fp asimd aes pmull sha2 crc32 sve sve2\n"
	got = ParseCPUInfoFlags(arm)
	for _, flag := range []string{"neon", "aes", "pmull", "sha2", "crc32", "sve", "sve2"} {
		if !got[flag] {
			t.Errorf("arm64: missing %s in %v", flag, got)
		}
	}
}

func TestParseFirstFloat(t *testing.T) {
	if got := parseFirstFloat("1.25 0.80 0.50 1/234 5678"); got != 1.25 {
		t.Fatalf("got %v", got)
	}
	if got := parseFirstFloat(""); got != -1 {
		t.Fatalf("got %v", got)
	}
}
