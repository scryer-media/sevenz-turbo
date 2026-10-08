// Package toolchain records what a run measured with: the candidate binaries
// (decode-bench built in its two cryptography configurations) and the
// versions they link, the 7-Zip oracle with its provenance, and the Rust
// toolchain and crate commit when the run is next to a checkout.
package toolchain

import (
	"bufio"
	"bytes"
	"context"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
	"strings"
)

// Binary is one executable the harness ran, identified by its digest.
type Binary struct {
	Path         string `json:"path"`
	ResolvedPath string `json:"resolved_path"`
	SHA256       string `json:"sha256"`
}

// Candidate is a decode-bench binary and what its `op version` reports.
type Candidate struct {
	Binary
	Label string `json:"label"`
	// Version is the `op version` object: crypto_backend, lzma_turbo,
	// sevenz_rust2, aws_lc_rs, crc_fast, ppmd_rust, available_parallelism.
	Version map[string]any `json:"version"`
}

// Field returns a string field of the version object, or "".
func (c Candidate) Field(name string) string {
	if value, ok := c.Version[name].(string); ok {
		return value
	}
	return ""
}

// Oracle is the 7-Zip the candidate is measured against.
type Oracle struct {
	Binary
	Banner string `json:"banner"`
	// Version is the release in the banner ("26.01").
	Version string `json:"version"`
	// Provenance says where the binary came from: the operator's
	// SEVENZ_BENCH_ORACLE_PROVENANCE (e.g. the release asset URL it was
	// downloaded from), else what the harness can tell from the path.
	Provenance string `json:"provenance"`
	// Official is true only when the operator vouched for the binary as an
	// official 7-Zip release (SEVENZ_BENCH_ORACLE_OFFICIAL=1).
	Official bool `json:"official"`
}

// Rust is the build environment, when the run is next to a checkout.
type Rust struct {
	// Source says where Commit and CargoLock come from: "checkout" when the
	// checkout's commit and Cargo.lock digest match the ones the candidate
	// binary embeds (so its rustc and cargo describe the build too), or
	// "candidate" when only the binary's own record is trusted.
	Source        string `json:"source,omitempty"`
	Note          string `json:"note,omitempty"`
	Rustc         string `json:"rustc"`
	RustcHost     string `json:"rustc_host,omitempty"`
	Cargo         string `json:"cargo"`
	Commit        string `json:"commit"`
	Dirty         bool   `json:"dirty"`
	CargoLock     string `json:"cargo_lock_sha256,omitempty"`
	LockedTurbo   string `json:"locked_lzma_turbo,omitempty"`
	LockedVersion string `json:"locked_sevenz_turbo,omitempty"`
}

// Toolchain is the whole record.
type Toolchain struct {
	Candidates []Candidate `json:"candidates"`
	Oracle     Oracle      `json:"oracle"`
	Rust       Rust        `json:"rust"`
	// LinkedLzmaTurbo is the lzma-turbo version the primary candidate was
	// built against (from its own `op version`).
	LinkedLzmaTurbo string `json:"linked_lzma_turbo"`
}

// ErrNoOracle is returned when no 7-Zip is found.
var ErrNoOracle = errors.New("no 7-Zip oracle found: pass --oracle or set SEVENZ_BENCH_ORACLE (the official 7zz/7zz.exe from https://www.7-zip.org/download.html)")

// FindOracle resolves the oracle: the explicit path, $SEVENZ_BENCH_ORACLE,
// then 7zz, 7zz.exe, 7z, 7za on PATH. The path returned is absolute: fixture
// generation and encode rows start the oracle with its working directory set
// to a source directory, where a relative path would no longer resolve.
func FindOracle(explicit string) (string, error) {
	for _, candidate := range []string{explicit, os.Getenv("SEVENZ_BENCH_ORACLE")} {
		if candidate != "" {
			if _, err := os.Stat(candidate); err != nil {
				return "", fmt.Errorf("oracle %s: %w", candidate, err)
			}
			return filepath.Abs(candidate)
		}
	}
	for _, name := range []string{"7zz", "7zz.exe", "7z", "7za"} {
		if path, err := exec.LookPath(name); err == nil {
			return filepath.Abs(path)
		}
	}
	return "", ErrNoOracle
}

// IsP7zip reports whether a 7-Zip's startup output is p7zip's. The marker is
// on a line of its own after the banner ("p7zip Version 16.02"), so the whole
// output is searched, not only the banner line.
func IsP7zip(output string) bool {
	return strings.Contains(strings.ToLower(output), "p7zip")
}

// ParseBanner returns the banner line and release of 7-Zip's startup text.
func ParseBanner(text string) (banner, version string) {
	scanner := bufio.NewScanner(strings.NewReader(text))
	for scanner.Scan() {
		line := strings.TrimSpace(scanner.Text())
		if !strings.Contains(line, "7-Zip") {
			continue
		}
		fields := strings.Fields(line)
		for i, field := range fields {
			if field == "7-Zip" || strings.HasPrefix(field, "(") {
				continue
			}
			if i > 0 && len(field) > 0 && field[0] >= '0' && field[0] <= '9' {
				return line, field
			}
		}
		return line, ""
	}
	return "", ""
}

// ProbeOracle identifies the oracle at path. A p7zip build is refused unless
// allowP7zip: p7zip is an unofficial fork frozen at 16.02 and is not the
// oracle these numbers are compared against.
func ProbeOracle(ctx context.Context, path string, allowP7zip bool) (Oracle, error) {
	binary, err := Identify(path)
	if err != nil {
		return Oracle{}, err
	}
	output, _ := exec.CommandContext(ctx, path).CombinedOutput()
	banner, version := ParseBanner(string(output))
	if banner == "" {
		return Oracle{}, fmt.Errorf("%s: no 7-Zip banner in its output", path)
	}
	if IsP7zip(string(output)) && !allowP7zip {
		return Oracle{}, fmt.Errorf("%s is p7zip (%s), not the official 7-Zip; install 7zz from https://www.7-zip.org/download.html or pass --allow-p7zip", path, banner)
	}
	oracle := Oracle{Binary: binary, Banner: banner, Version: version}
	oracle.Provenance, oracle.Official = provenance(binary.ResolvedPath)
	return oracle, nil
}

func provenance(resolved string) (string, bool) {
	official := os.Getenv("SEVENZ_BENCH_ORACLE_OFFICIAL") == "1"
	if value := os.Getenv("SEVENZ_BENCH_ORACLE_PROVENANCE"); value != "" {
		return value, official
	}
	slashed := filepath.ToSlash(resolved)
	switch {
	case strings.Contains(slashed, "/Cellar/") || strings.HasPrefix(slashed, "/opt/homebrew/"):
		return "Homebrew build (not an official 7-Zip release asset)", official
	case strings.HasPrefix(slashed, "/usr/bin/") || strings.HasPrefix(slashed, "/usr/lib/"):
		return "distribution package (not an official 7-Zip release asset)", official
	}
	return "unknown: set SEVENZ_BENCH_ORACLE_PROVENANCE to the release asset it came from", official
}

// Identify digests a binary and resolves its symlinks.
func Identify(path string) (Binary, error) {
	resolved, err := filepath.EvalSymlinks(path)
	if err != nil {
		return Binary{}, err
	}
	file, err := os.Open(resolved)
	if err != nil {
		return Binary{}, err
	}
	defer file.Close()
	digest := sha256.New()
	if _, err := io.Copy(digest, file); err != nil {
		return Binary{}, err
	}
	return Binary{Path: path, ResolvedPath: resolved, SHA256: hex.EncodeToString(digest.Sum(nil))}, nil
}

// Backends the two candidate builds must report: the default build is
// AWS-LC, the native-crypto one RustCrypto.
const (
	BackendDefault = "aws-lc"
	BackendNative  = "rustcrypto"
)

// CheckBackend fails unless the candidate reports the backend its role
// requires, so a swapped or misbuilt pair cannot label one backend's rows as
// the other's.
func CheckBackend(candidate Candidate, flag, want string) error {
	if got := candidate.Field("crypto_backend"); got != want {
		return fmt.Errorf("%s %s reports crypto backend %q, want %q", flag, candidate.Path, got, want)
	}
	return nil
}

// BindRust attaches the checkout's provenance to the candidate only when the
// checkout is the one the binary was built from: the commit and Cargo.lock
// digest the binary embeds (`op version`'s git_commit and cargo_lock_sha256)
// must equal the checkout's. Otherwise the record keeps only what the binary
// says about itself, and says why.
func BindRust(checkout Rust, candidate Candidate) Rust {
	commit, lock := candidate.Field("git_commit"), candidate.Field("cargo_lock_sha256")
	if commit != "" && commit != "unknown" && commit == checkout.Commit && lock != "" && lock == checkout.CargoLock {
		checkout.Source = "checkout"
		return checkout
	}
	bound := Rust{
		Source: "candidate", Rustc: "not-collected", Cargo: "not-collected",
		Commit: firstNonEmpty(commit, "unknown"), CargoLock: lock,
		LockedTurbo: candidate.Field("lzma_turbo"), LockedVersion: candidate.Field("sevenz_turbo"),
	}
	switch {
	case commit == "" || lock == "":
		bound.Note = "the candidate does not embed its commit and Cargo.lock digest (built before decode-bench recorded them); the checkout's provenance is not attached"
	case checkout.Commit == "not-collected":
		bound.Note = "no checkout to compare with; commit and Cargo.lock digest are the candidate's own"
	default:
		bound.Note = fmt.Sprintf("the checkout (commit %s, Cargo.lock %s) is not the one the candidate was built from; its provenance is not attached", checkout.Commit, short(checkout.CargoLock))
	}
	return bound
}

func firstNonEmpty(values ...string) string {
	for _, value := range values {
		if value != "" {
			return value
		}
	}
	return ""
}

func short(digest string) string {
	if len(digest) > 12 {
		return digest[:12]
	}
	return digest
}

// ProbeCandidate runs `<path> op version`.
func ProbeCandidate(ctx context.Context, label, path string) (Candidate, error) {
	binary, err := Identify(path)
	if err != nil {
		return Candidate{}, err
	}
	output, err := exec.CommandContext(ctx, path, "op", "version").Output()
	if err != nil {
		return Candidate{}, fmt.Errorf("%s op version: %w (is it decode-bench from this branch?)", path, err)
	}
	version, err := LastJSON(output)
	if err != nil {
		return Candidate{}, fmt.Errorf("%s op version: %w", path, err)
	}
	return Candidate{Binary: binary, Label: label, Version: version}, nil
}

// LastJSON parses the last non-empty line of output as a JSON object.
func LastJSON(output []byte) (map[string]any, error) {
	lines := bytes.Split(bytes.TrimSpace(output), []byte("\n"))
	if len(lines) == 0 || len(lines[len(lines)-1]) == 0 {
		return nil, errors.New("no output")
	}
	var object map[string]any
	if err := json.Unmarshal(bytes.TrimSpace(lines[len(lines)-1]), &object); err != nil {
		return nil, fmt.Errorf("last line is not JSON: %w", err)
	}
	return object, nil
}

// LockedVersion returns the version of the first [[package]] named name in a
// Cargo.lock.
func LockedVersion(lock, name string) string {
	var current string
	for _, line := range strings.Split(lock, "\n") {
		line = strings.TrimSpace(line)
		switch {
		case line == "[[package]]":
			current = ""
		case strings.HasPrefix(line, "name = "):
			current = strings.Trim(strings.TrimPrefix(line, "name = "), `"`)
		case strings.HasPrefix(line, "version = ") && current == name:
			return strings.Trim(strings.TrimPrefix(line, "version = "), `"`)
		}
	}
	return ""
}

// ProbeRust records the toolchain and commit of the checkout at repo, or
// "not-collected" for what it cannot read (a fleet host given only binaries).
func ProbeRust(ctx context.Context, repo string) Rust {
	rust := Rust{Rustc: "not-collected", Cargo: "not-collected", Commit: "not-collected"}
	run := func(dir, name string, args ...string) string {
		cmd := exec.CommandContext(ctx, name, args...)
		cmd.Dir = dir
		output, err := cmd.Output()
		if err != nil {
			return ""
		}
		return strings.TrimSpace(string(output))
	}
	if repo == "" {
		repo = run("", "git", "rev-parse", "--show-toplevel")
	}
	if repo == "" {
		return rust
	}
	if output := run(repo, "rustc", "-Vv"); output != "" {
		lines := strings.Split(output, "\n")
		rust.Rustc = lines[0]
		for _, line := range lines {
			if host, found := strings.CutPrefix(line, "host: "); found {
				rust.RustcHost = host
			}
		}
	}
	if output := run(repo, "cargo", "-V"); output != "" {
		rust.Cargo = output
	}
	if output := run(repo, "git", "rev-parse", "HEAD"); output != "" {
		rust.Commit = output
		rust.Dirty = run(repo, "git", "status", "--porcelain", "--untracked-files=no") != ""
	}
	if lock, err := os.ReadFile(filepath.Join(repo, "Cargo.lock")); err == nil {
		sum := sha256.Sum256(lock)
		rust.CargoLock = hex.EncodeToString(sum[:])
		rust.LockedTurbo = LockedVersion(string(lock), "lzma-turbo")
		rust.LockedVersion = LockedVersion(string(lock), "sevenz-turbo")
	}
	return rust
}

// DefaultCandidate is where `cargo build --release -p decode-bench` puts the
// binary, relative to the checkout.
func DefaultCandidate(repo string) string {
	name := "decode-bench"
	if runtime.GOOS == "windows" {
		name += ".exe"
	}
	return filepath.Join(repo, "target", "release", name)
}
