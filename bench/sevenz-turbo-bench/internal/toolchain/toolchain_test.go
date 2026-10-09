package toolchain

import (
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func TestParseBanner(t *testing.T) {
	cases := []struct{ text, version string }{
		{"\n7-Zip (z) 26.01 (arm64) : Copyright (c) 1999-2026 Igor Pavlov : 2026-04-27\n 64-bit", "26.01"},
		{"7-Zip 24.09 (x64) : Copyright (c) 1999-2024 Igor Pavlov : 2024-11-29", "24.09"},
		{"7-Zip [64] 16.02 : Copyright (c) 1999-2016 Igor Pavlov : 2016-05-21\np7zip Version 16.02", "16.02"},
	}
	for _, c := range cases {
		banner, version := ParseBanner(c.text)
		if banner == "" || version != c.version {
			t.Errorf("ParseBanner(%q) = %q, %q; want version %q", c.text, banner, version, c.version)
		}
	}
	if banner, _ := ParseBanner("usage: something else"); banner != "" {
		t.Errorf("non-7-Zip text gave banner %q", banner)
	}
}

func TestLockedVersion(t *testing.T) {
	lock := "[[package]]\nname = \"crc-fast\"\nversion = \"1.10.0\"\n\n[[package]]\nname = \"lzma-turbo\"\nversion = \"0.6.0\"\nsource = \"registry\"\n"
	if got := LockedVersion(lock, "lzma-turbo"); got != "0.6.0" {
		t.Fatalf("got %q", got)
	}
	if got := LockedVersion(lock, "absent"); got != "" {
		t.Fatalf("got %q", got)
	}
}

func TestLastJSON(t *testing.T) {
	object, err := LastJSON([]byte("noise\n{\"ok\":true,\"lzma_turbo\":\"0.6.0\"}\n"))
	if err != nil || object["lzma_turbo"] != "0.6.0" {
		t.Fatalf("got %v, %v", object, err)
	}
	if _, err := LastJSON([]byte("not json\n")); err == nil {
		t.Fatal("want an error")
	}
}

func TestIsP7zipReadsTheWholeOutput(t *testing.T) {
	output := "\n7-Zip [64] 16.02 : Copyright (c) 1999-2016 Igor Pavlov : 2016-05-21\np7zip Version 16.02 (locale=utf8,Utf16=on,HugeFiles=on,64 bits)\n"
	banner, _ := ParseBanner(output)
	if strings.Contains(strings.ToLower(banner), "p7zip") {
		t.Fatalf("the banner line itself carries the marker (%q); the test needs it on the next line", banner)
	}
	if !IsP7zip(output) {
		t.Fatal("p7zip output not recognised")
	}
	if IsP7zip("7-Zip (z) 26.01 (arm64) : Copyright (c) 1999-2026 Igor Pavlov : 2026-04-27") {
		t.Fatal("official 7-Zip taken for p7zip")
	}
}

func TestFindOracleReturnsAnAbsolutePath(t *testing.T) {
	dir := t.TempDir()
	if err := os.WriteFile(filepath.Join(dir, "7zz"), []byte("#!/bin/sh\n"), 0o755); err != nil {
		t.Fatal(err)
	}
	t.Chdir(dir)
	t.Setenv("SEVENZ_BENCH_ORACLE", "")
	path, err := FindOracle(filepath.Join(".", "7zz"))
	if err != nil {
		t.Fatal(err)
	}
	if !filepath.IsAbs(path) {
		t.Fatalf("FindOracle returned relative %q", path)
	}
	t.Setenv("SEVENZ_BENCH_ORACLE", "7zz")
	if path, err = FindOracle(""); err != nil || !filepath.IsAbs(path) {
		t.Fatalf("env oracle: %q, %v", path, err)
	}
}

func TestCheckBackend(t *testing.T) {
	aws := Candidate{Binary: Binary{Path: "a"}, Version: map[string]any{"crypto_backend": BackendDefault}}
	native := Candidate{Binary: Binary{Path: "n"}, Version: map[string]any{"crypto_backend": BackendNative}}
	if CheckBackend(aws, "--candidate", BackendDefault) != nil || CheckBackend(native, "--candidate-native", BackendNative) != nil {
		t.Fatal("a correct pair was refused")
	}
	// Swapped: each differs from the other, and each is refused.
	if CheckBackend(native, "--candidate", BackendDefault) == nil || CheckBackend(aws, "--candidate-native", BackendNative) == nil {
		t.Fatal("a swapped pair was accepted")
	}
}

func TestBindRustOnlyAttachesAMatchingCheckout(t *testing.T) {
	checkout := Rust{Rustc: "rustc 1.97.1", Cargo: "cargo 1.97.1", Commit: "abc", CargoLock: "lock1"}
	matching := Candidate{Version: map[string]any{"git_commit": "abc", "git_dirty": "false", "cargo_lock_sha256": "lock1", "lzma_turbo": "0.6.0"}}
	if bound := BindRust(checkout, matching); bound.Source != "checkout" || bound.Rustc != "rustc 1.97.1" {
		t.Fatalf("matching checkout not attached: %+v", bound)
	}
	editedCheckout := checkout
	editedCheckout.BuildDirty = true
	if bound := BindRust(editedCheckout, matching); bound.Source != "candidate" || bound.Note == "" {
		t.Fatalf("a checkout with uncommitted build changes was attached: %+v", bound)
	}
	for name, candidate := range map[string]Candidate{
		"other commit":    {Version: map[string]any{"git_commit": "def", "git_dirty": "false", "cargo_lock_sha256": "lock1"}},
		"other lock":      {Version: map[string]any{"git_commit": "abc", "git_dirty": "false", "cargo_lock_sha256": "lock2"}},
		"built dirty":     {Version: map[string]any{"git_commit": "abc", "git_dirty": "true", "cargo_lock_sha256": "lock1"}},
		"no dirty record": {Version: map[string]any{"git_commit": "abc", "cargo_lock_sha256": "lock1"}},
		"no record":       {Version: map[string]any{}},
	} {
		bound := BindRust(checkout, candidate)
		if bound.Source != "candidate" || bound.Rustc != "not-collected" || bound.Note == "" {
			t.Errorf("%s: checkout attached: %+v", name, bound)
		}
		if bound.Commit == "abc" && name == "other commit" {
			t.Errorf("%s: kept the checkout's commit: %+v", name, bound)
		}
	}
}

func TestPPMdCratesReadsEitherRecord(t *testing.T) {
	for name, tc := range map[string]struct {
		version map[string]any
		want    string
	}{
		"every ppmd crate": {map[string]any{"ppmd_crates": "ppmd-rust 1.5.0, ppmd-turbo 0.1.0", "ppmd_turbo": "0.1.0"}, "ppmd-rust 1.5.0, ppmd-turbo 0.1.0"},
		"older binary":     {map[string]any{"ppmd_rust": "1.5.0"}, "ppmd-rust 1.5.0"},
		"no record":        {map[string]any{}, "unknown"},
	} {
		if got := (Candidate{Version: tc.version}).PPMdCrates(); got != tc.want {
			t.Errorf("%s: %q, want %q", name, got, tc.want)
		}
	}
}

func TestCheckEncoder(t *testing.T) {
	if CheckEncoder(Candidate{Version: map[string]any{"lzma_encoder": "lzma-turbo"}}, "--candidate") != nil {
		t.Fatal("the default encoder was refused")
	}
	for _, version := range []map[string]any{{"lzma_encoder": "lzma-rust2"}, {}} {
		if CheckEncoder(Candidate{Version: version}, "--candidate") == nil {
			t.Errorf("%v was accepted", version)
		}
	}
}

func TestCheckProfile(t *testing.T) {
	if CheckProfile(Candidate{Version: map[string]any{"build_profile": "release"}}, "--candidate") != nil {
		t.Fatal("a release build was refused")
	}
	for _, version := range []map[string]any{{"build_profile": "debug"}, {"build_profile": "unknown"}, {}} {
		if CheckProfile(Candidate{Version: version}, "--candidate") == nil {
			t.Errorf("%v was accepted", version)
		}
	}
}

func TestSameBuild(t *testing.T) {
	build := func(change func(map[string]any)) Candidate {
		version := map[string]any{"git_commit": "abc", "git_dirty": "false", "cargo_lock_sha256": "l1", "build_profile": "release", "sevenz_turbo": "0.27.0", "lzma_turbo": "0.7.0"}
		if change != nil {
			change(version)
		}
		return Candidate{Version: version}
	}
	if err := SameBuild(build(nil), build(nil)); err != nil {
		t.Fatalf("one build was refused: %v", err)
	}
	for name, change := range map[string]func(map[string]any){
		"commit":  func(v map[string]any) { v["git_commit"] = "def" },
		"lock":    func(v map[string]any) { v["cargo_lock_sha256"] = "l2" },
		"version": func(v map[string]any) { v["lzma_turbo"] = "0.6.0" },
		"profile": func(v map[string]any) { v["build_profile"] = "debug" },
		"dirty":   func(v map[string]any) { v["git_dirty"] = "true" },
		"missing": func(v map[string]any) { delete(v, "git_commit") },
	} {
		if SameBuild(build(nil), build(change)) == nil {
			t.Errorf("%s differs but the pair was accepted", name)
		}
	}
	dirty := func(v map[string]any) { v["git_dirty"] = "true" }
	if SameBuild(build(dirty), build(dirty)) == nil {
		t.Error("a pair built from uncommitted changes was accepted")
	}
}

func TestLockDigestReadsCRLFAsLF(t *testing.T) {
	const lf = "911169ddaaf146aff539f58c26c489af3b892dff0fe283c1c264c65ae5aa59a2"
	for _, lock := range []string{"a\nb\n", "a\r\nb\r\n", "a\r\nb\n"} {
		if got := LockDigest([]byte(lock)); got != lf {
			t.Errorf("LockDigest(%q) = %s, want the LF digest %s", lock, got, lf)
		}
	}
	if got := LockDigest([]byte("a\rb\n")); got != "367d1c77eadc1495a7db4200f46a8b90ea1fa926282722d308c05a65098a4112" {
		t.Errorf("a carriage return not before a line feed was dropped: %s", got)
	}
}
