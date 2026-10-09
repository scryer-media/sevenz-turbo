package fixtures

import (
	"bytes"
	"context"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"io/fs"
	"os"
	"os/exec"
	"path/filepath"
	"slices"
	"sort"
	"strings"
	"time"

	"github.com/scryer-media/sevenz-turbo/bench/sevenz-turbo-bench/internal/payload"
)

// ManifestName is the corpus manifest written next to the archives.
const ManifestName = "fixtures.json"

// Schema identifies the manifest layout.
const Schema = "sevenz-turbo-bench/fixtures/1"

// fixedTime is every generated file's mtime, so the archives 7zz writes do not
// carry the generation time.
var fixedTime = time.Date(2026, 1, 1, 0, 0, 0, 0, time.UTC)

// Oracle identifies the 7zz that wrote the corpus.
type Oracle struct {
	Path   string `json:"path"`
	Banner string `json:"banner"`
	SHA256 string `json:"sha256"`
}

// SourceRecord is a generated source as it is on disk.
type SourceRecord struct {
	SourceSpec
	Path string `json:"path"`
	// TotalBytes is the payload size (the sum over a tree's members).
	TotalBytes int64 `json:"total_bytes"`
	FileCount  int   `json:"file_count"`
	// SHA256 is the file's digest, or for a tree the digest over every
	// member's "path\x00size\x00contents" in path order.
	SHA256 string `json:"sha256"`
}

// ArchiveRecord is a written fixture archive.
type ArchiveRecord struct {
	ArchiveSpec
	Path          string `json:"path"`
	Bytes         int64  `json:"bytes"`
	SHA256        string `json:"sha256"`
	UnpackedBytes int64  `json:"unpacked_bytes"`
	Entries       int    `json:"entries"`
	// LZMA2 is the archive's run layout as counted from its stream, for an
	// archive whose recipe sets MinRuns.
	LZMA2 *LZMA2Runs `json:"lzma2,omitempty"`
}

// Manifest is fixtures.json.
type Manifest struct {
	SchemaVersion int             `json:"schema_version"`
	Schema        string          `json:"schema"`
	Profile       string          `json:"profile"`
	GeneratedUTC  string          `json:"generated_utc"`
	Oracle        Oracle          `json:"oracle"`
	Sources       []SourceRecord  `json:"sources"`
	Archives      []ArchiveRecord `json:"archives"`
}

// Archive returns the record for name.
func (m *Manifest) Archive(name string) (ArchiveRecord, bool) {
	for _, archive := range m.Archives {
		if archive.Name == name {
			return archive, true
		}
	}
	return ArchiveRecord{}, false
}

// Source returns the record for name.
func (m *Manifest) Source(name string) (SourceRecord, bool) {
	for _, source := range m.Sources {
		if source.Name == name {
			return source, true
		}
	}
	return SourceRecord{}, false
}

// Load reads dir/fixtures.json.
func Load(dir string) (*Manifest, error) {
	data, err := os.ReadFile(filepath.Join(dir, ManifestName))
	if err != nil {
		return nil, err
	}
	var manifest Manifest
	if err := json.Unmarshal(data, &manifest); err != nil {
		return nil, fmt.Errorf("%s: %w", ManifestName, err)
	}
	if manifest.SchemaVersion != 1 {
		return nil, fmt.Errorf("%s: schema_version %d, want 1", ManifestName, manifest.SchemaVersion)
	}
	return &manifest, nil
}

// Verify re-reads the corpus in dir against the manifest, before a run trusts
// it: every source's members (exactly the recipe's, no more) and digest, and
// every archive's size and SHA-256. A file changed after fixtures.json was
// written (a partial copy, a manual edit, a bad disk) is reported here rather
// than measured under the manifest's numbers.
func (m *Manifest) Verify(dir string) error {
	for _, recorded := range m.Sources {
		current, err := measureSource(dir, recorded.SourceSpec)
		if err != nil {
			return fmt.Errorf("source %s: %w (rerun `sevenz-turbo-bench fixtures`)", recorded.Name, err)
		}
		if current.TotalBytes != recorded.TotalBytes || current.FileCount != recorded.FileCount || current.SHA256 != recorded.SHA256 {
			return fmt.Errorf("source %s: on disk %d bytes in %d files, sha256 %s; the manifest records %d bytes in %d files, sha256 %s (rerun `sevenz-turbo-bench fixtures`)",
				recorded.Name, current.TotalBytes, current.FileCount, current.SHA256, recorded.TotalBytes, recorded.FileCount, recorded.SHA256)
		}
	}
	for _, recorded := range m.Archives {
		digest, size, err := FileSHA256(filepath.Join(dir, recorded.Name))
		if err != nil {
			return fmt.Errorf("archive %s: %w (rerun `sevenz-turbo-bench fixtures`)", recorded.Name, err)
		}
		if size != recorded.Bytes || digest != recorded.SHA256 {
			return fmt.Errorf("archive %s: on disk %d bytes, sha256 %s; the manifest records %d bytes, sha256 %s (rerun `sevenz-turbo-bench fixtures`)",
				recorded.Name, size, digest, recorded.Bytes, recorded.SHA256)
		}
	}
	return nil
}

// Options controls Generate.
type Options struct {
	Dir     string
	Profile Profile
	// SevenZip is the oracle used to write the archives.
	SevenZip Oracle
	// Only restricts generation to these archives (and their sources).
	Only []string
	Log  io.Writer
}

// Generate builds every source and archive of the profile that is not
// already on disk as the recipe and oracle would write it, then writes the
// manifest with every size and digest. A source is kept only when its marker
// names the same recipe and its members are exactly the recipe's; an archive
// only when its provenance file names the same 7zz, switches and source
// content. Anything else is regenerated, so the manifest never attributes
// cached bytes to a recipe or an oracle that did not produce them.
func Generate(ctx context.Context, options Options) (*Manifest, error) {
	logf := func(format string, args ...any) {
		if options.Log != nil {
			fmt.Fprintf(options.Log, format+"\n", args...)
		}
	}
	if err := os.MkdirAll(filepath.Join(options.Dir, "src"), 0o755); err != nil {
		return nil, err
	}
	wanted := map[string]bool{}
	for _, name := range options.Only {
		if _, ok := options.Profile.Archive(name); !ok {
			return nil, fmt.Errorf("unknown fixture %q", name)
		}
		wanted[name] = true
	}
	archives := []ArchiveSpec{}
	sourcesNeeded := map[string]bool{}
	for _, archive := range options.Profile.Archives {
		if len(wanted) > 0 && !wanted[archive.Name] {
			continue
		}
		archives = append(archives, archive)
		sourcesNeeded[archive.Source] = true
	}
	// Encode rows read the sources directly.
	if len(wanted) == 0 {
		for _, source := range options.Profile.Sources {
			sourcesNeeded[source.Name] = true
		}
	}

	manifest := &Manifest{SchemaVersion: 1, Schema: Schema, Profile: options.Profile.Name,
		GeneratedUTC: time.Now().UTC().Format(time.RFC3339), Oracle: options.SevenZip}
	for _, source := range options.Profile.Sources {
		if !sourcesNeeded[source.Name] {
			continue
		}
		record, err := ensureSource(options.Dir, source, logf)
		if err != nil {
			return nil, fmt.Errorf("source %s: %w", source.Name, err)
		}
		manifest.Sources = append(manifest.Sources, record)
	}
	for _, archive := range archives {
		source, _ := manifest.Source(archive.Source)
		record, err := ensureArchive(ctx, options, archive, source, logf)
		if err != nil {
			return nil, fmt.Errorf("archive %s: %w", archive.Name, err)
		}
		manifest.Archives = append(manifest.Archives, record)
	}
	data, err := json.MarshalIndent(manifest, "", "  ")
	if err != nil {
		return nil, err
	}
	if err := os.WriteFile(filepath.Join(options.Dir, ManifestName), append(data, '\n'), 0o644); err != nil {
		return nil, err
	}
	return manifest, nil
}

// SourceDir is where a source's files live.
func SourceDir(dir, source string) string { return filepath.Join(dir, "src", source) }

// sourceGenerator versions the payload generators. Bump it when package
// payload changes what a recipe writes, so cached sources are regenerated.
const sourceGenerator = 1

// markerName is the file a generated source directory is finished with. Its
// contents bind it to the recipe that wrote it. It is a top-level dot name,
// so neither 7zz's entry list nor the candidate's encode includes it.
const markerName = ".complete"

// sourceMarker is the marker's contents: every recipe field that shapes the
// bytes (not the note), the generator version, and the content digest taken
// when generation finished, so a member edited in place (same size) is
// caught on reuse.
type sourceMarker struct {
	Generator    int    `json:"generator"`
	Name         string `json:"name"`
	Kind         string `json:"kind"`
	Bytes        int64  `json:"bytes,omitempty"`
	Files        int    `json:"files,omitempty"`
	AverageBytes int64  `json:"average_bytes,omitempty"`
	Seed         uint64 `json:"seed,omitempty"`
	SHA256       string `json:"sha256"`
}

func markerFor(source SourceSpec, digest string) sourceMarker {
	return sourceMarker{sourceGenerator, source.Name, source.Kind, source.Bytes, source.Files, source.AverageBytes, source.Seed, digest}
}

// sourceContent is a source's digest, as the manifest records it, with its
// byte and file counts.
type sourceContent struct {
	sha256 string
	bytes  int64
	files  int
}

// hashSource digests the source at root: a tree's members in recipe order,
// each prefixed by its path and size; a single file's bytes.
func hashSource(root string, source SourceSpec) (sourceContent, error) {
	if source.Kind == KindTree {
		digest := sha256.New()
		var content sourceContent
		files := payload.Tree(source.Files, source.AverageBytes, source.Seed)
		for _, file := range files {
			fmt.Fprintf(digest, "%s\x00%d\x00", file.Path, file.Size)
			if err := hashInto(digest, filepath.Join(root, filepath.FromSlash(file.Path))); err != nil {
				return content, err
			}
			content.bytes += file.Size
		}
		content.files = len(files)
		content.sha256 = hex.EncodeToString(digest.Sum(nil))
		return content, nil
	}
	digest, size, err := FileSHA256(filepath.Join(root, source.FileName()))
	return sourceContent{sha256: digest, bytes: size, files: 1}, err
}

// expectedMembers is every file a source holds, by slash-separated path
// relative to its directory, with its size.
func expectedMembers(source SourceSpec) map[string]int64 {
	members := map[string]int64{}
	if source.Kind == KindTree {
		for _, file := range payload.Tree(source.Files, source.AverageBytes, source.Seed) {
			members[file.Path] = file.Size
		}
		return members
	}
	members[source.FileName()] = source.Bytes
	return members
}

// diskMembers lists the regular files under root as expectedMembers does,
// skipping top-level dot names (the marker), as Entries does. Any other
// entry (a symlink, a FIFO, a device) is an error: 7zz and decode-bench would
// treat it differently, and a FIFO could stall either.
func diskMembers(root string) (map[string]int64, error) {
	members := map[string]int64{}
	err := filepath.WalkDir(root, func(path string, entry fs.DirEntry, err error) error {
		if err != nil {
			return err
		}
		relative, err := filepath.Rel(root, path)
		if err != nil {
			return err
		}
		if relative == "." {
			return nil
		}
		relative = filepath.ToSlash(relative)
		if !strings.Contains(relative, "/") && strings.HasPrefix(relative, ".") {
			if entry.IsDir() {
				return filepath.SkipDir
			}
			return nil
		}
		if entry.IsDir() {
			return nil
		}
		if !entry.Type().IsRegular() {
			return fmt.Errorf("%s is not a regular file", relative)
		}
		info, err := entry.Info()
		if err != nil {
			return err
		}
		members[relative] = info.Size()
		return nil
	})
	return members, err
}

// sourceProblem says why the source directory at root is not exactly what the
// recipe writes, or "" when it is.
func sourceProblem(root string, source SourceSpec) string {
	_, problem := checkSource(root, source)
	return problem
}

// checkSource hashes the source at root when it is exactly what the recipe
// wrote, or says why not: the marker must name the recipe, the files must be
// the recipe's members at their sizes with nothing extra, and their content
// must still hash to the digest the marker took when generation finished.
func checkSource(root string, source SourceSpec) (sourceContent, string) {
	data, err := os.ReadFile(filepath.Join(root, markerName))
	if err != nil {
		return sourceContent{}, "no completion marker"
	}
	var marker sourceMarker
	if err := json.Unmarshal(data, &marker); err != nil || marker != markerFor(source, marker.SHA256) || marker.SHA256 == "" {
		return sourceContent{}, "written by a different recipe"
	}
	if problem := memberProblem(root, source); problem != "" {
		return sourceContent{}, problem
	}
	content, err := hashSource(root, source)
	if err != nil {
		return content, err.Error()
	}
	if content.sha256 != marker.SHA256 {
		return content, "contents changed since it was generated"
	}
	return content, ""
}

// memberProblem says why the files at root are not the recipe's members at
// their sizes, with nothing extra, or "" when they are.
func memberProblem(root string, source SourceSpec) string {
	onDisk, err := diskMembers(root)
	if err != nil {
		return err.Error()
	}
	want := expectedMembers(source)
	for path, size := range want {
		got, ok := onDisk[path]
		if !ok {
			return "missing " + path
		}
		if got != size {
			return fmt.Sprintf("%s is %d bytes, want %d", path, got, size)
		}
	}
	for path := range onDisk {
		if _, ok := want[path]; !ok {
			return "unexpected member " + path
		}
	}
	return ""
}

// measureSource hashes the source as it is on disk, refusing it unless its
// members are exactly the recipe's.
func measureSource(dir string, source SourceSpec) (SourceRecord, error) {
	root := SourceDir(dir, source.Name)
	record := SourceRecord{SourceSpec: source, Path: filepath.Join("src", source.Name)}
	content, problem := checkSource(root, source)
	if problem != "" {
		return record, errors.New(problem)
	}
	record.TotalBytes, record.FileCount, record.SHA256 = content.bytes, content.files, content.sha256
	return record, nil
}

func ensureSource(dir string, source SourceSpec, logf func(string, ...any)) (SourceRecord, error) {
	root := SourceDir(dir, source.Name)
	if problem := sourceProblem(root, source); problem != "" {
		if err := generateSource(root, source, problem, logf); err != nil {
			return SourceRecord{SourceSpec: source}, err
		}
	}
	return measureSource(dir, source)
}

// generateSource writes the source from scratch: whatever was at root is
// removed first, so no member of an earlier recipe survives.
func generateSource(root string, source SourceSpec, reason string, logf func(string, ...any)) error {
	if err := os.RemoveAll(root); err != nil {
		return err
	}
	if err := os.MkdirAll(root, 0o755); err != nil {
		return err
	}
	if source.Kind == KindTree {
		files := payload.Tree(source.Files, source.AverageBytes, source.Seed)
		logf("fixtures: generating %s (%d members): %s", source.Name, len(files), reason)
		for index, file := range files {
			path := filepath.Join(root, filepath.FromSlash(file.Path))
			if err := os.MkdirAll(filepath.Dir(path), 0o755); err != nil {
				return err
			}
			if err := writeFile(path, func(w io.Writer) error { return payload.WriteTreeFile(w, index, file.Size) }); err != nil {
				return err
			}
		}
	} else {
		logf("fixtures: generating %s (%d MiB): %s", source.Name, source.Bytes>>20, reason)
		generate := map[string]func(io.Writer) error{
			KindText:      func(w io.Writer) error { return payload.WriteText(w, int(source.Bytes/payload.MiB)) },
			KindCodeX86:   func(w io.Writer) error { return payload.WriteCodeX86(w, source.Bytes) },
			KindCodeARM64: func(w io.Writer) error { return payload.WriteCodeARM64(w, source.Bytes) },
			KindAudio:     func(w io.Writer) error { return payload.WriteAudio(w, source.Bytes) },
			KindMedia:     func(w io.Writer) error { return payload.WriteMedia(w, source.Bytes) },
		}[source.Kind]
		if generate == nil {
			return fmt.Errorf("unknown source kind %q", source.Kind)
		}
		if err := writeFile(filepath.Join(root, source.FileName()), generate); err != nil {
			return err
		}
	}
	if err := fixMtimes(root); err != nil {
		return err
	}
	if problem := memberProblem(root, source); problem != "" {
		return fmt.Errorf("generated %s is not its recipe: %s", source.Name, problem)
	}
	content, err := hashSource(root, source)
	if err != nil {
		return err
	}
	marker, err := json.Marshal(markerFor(source, content.sha256))
	if err != nil {
		return err
	}
	return os.WriteFile(filepath.Join(root, markerName), marker, 0o644)
}

// Entries is what 7zz is told to add for a source, relative to its directory:
// its top-level names, sorted.
func Entries(dir, source string) ([]string, error) {
	items, err := os.ReadDir(SourceDir(dir, source))
	if err != nil {
		return nil, err
	}
	var names []string
	for _, item := range items {
		if strings.HasPrefix(item.Name(), ".") {
			continue
		}
		names = append(names, item.Name())
	}
	sort.Strings(names)
	return names, nil
}

// provenance is what an archive was written from, kept next to it as
// <name>.provenance.json: the 7zz that wrote it, the switches, the source's
// digest, and the archive's own digest. A cached archive is reused only when
// all of it still holds.
type provenance struct {
	Oracle    Oracle   `json:"oracle"`
	Source    string   `json:"source"`
	SourceSHA string   `json:"source_sha256"`
	Args      []string `json:"args"`
	Encrypted bool     `json:"encrypted,omitempty"`
	SHA256    string   `json:"sha256"`
}

func provenancePath(dir, name string) string { return filepath.Join(dir, name+".provenance.json") }

// archiveProblem says why the archive at path cannot be reused for want, or ""
// when it can.
func archiveProblem(dir, path string, want provenance) string {
	data, err := os.ReadFile(provenancePath(dir, filepath.Base(path)))
	if err != nil {
		return "no provenance record"
	}
	var have provenance
	if err := json.Unmarshal(data, &have); err != nil {
		return "unreadable provenance record"
	}
	switch {
	case have.Oracle.SHA256 != want.Oracle.SHA256:
		return fmt.Sprintf("written by another 7zz (%s)", have.Oracle.Banner)
	case have.Source != want.Source || have.SourceSHA != want.SourceSHA:
		return "written from other source content"
	case !slices.Equal(have.Args, want.Args) || have.Encrypted != want.Encrypted:
		return "written with other switches"
	}
	digest, _, err := FileSHA256(path)
	if err != nil {
		return err.Error()
	}
	if digest != have.SHA256 {
		return "changed since it was written"
	}
	return ""
}

func ensureArchive(ctx context.Context, options Options, archive ArchiveSpec, source SourceRecord, logf func(string, ...any)) (ArchiveRecord, error) {
	record := ArchiveRecord{ArchiveSpec: archive, Path: archive.Name, UnpackedBytes: source.TotalBytes, Entries: source.FileCount}
	path := filepath.Join(options.Dir, archive.Name)
	want := provenance{Oracle: options.SevenZip, Source: archive.Source, SourceSHA: source.SHA256, Args: archive.Args, Encrypted: archive.Encrypted}
	problem := "not on disk"
	if _, err := os.Stat(path); err == nil {
		problem = archiveProblem(options.Dir, path, want)
	}
	if problem != "" {
		if options.SevenZip.Path == "" {
			return record, errors.New("no 7zz to write it with")
		}
		logf("fixtures: 7zz a %s %s: %s", strings.Join(archive.Args, " "), archive.Name, problem)
		_ = os.Remove(provenancePath(options.Dir, archive.Name))
		entries, err := Entries(options.Dir, archive.Source)
		if err != nil {
			return record, err
		}
		absolute, err := filepath.Abs(path + ".tmp")
		if err != nil {
			return record, err
		}
		_ = os.Remove(absolute)
		args := append([]string{"a", "-bso0", "-bsp0", "-y", "-t7z"}, archive.Args...)
		args = append(args, absolute)
		args = append(args, entries...)
		cmd := exec.CommandContext(ctx, options.SevenZip.Path, args...)
		cmd.Dir = SourceDir(options.Dir, archive.Source)
		var stderr bytes.Buffer
		cmd.Stderr = &stderr
		if err := cmd.Run(); err != nil {
			_ = os.Remove(absolute)
			return record, fmt.Errorf("7zz a: %v: %s", err, strings.TrimSpace(stderr.String()))
		}
		if err := os.Rename(absolute, path); err != nil {
			return record, err
		}
	}
	digest, size, err := FileSHA256(path)
	if err != nil {
		return record, err
	}
	record.Bytes, record.SHA256 = size, digest
	if archive.MinRuns > 0 {
		runs, err := countRuns(path, source.TotalBytes)
		if err != nil {
			return record, err
		}
		if runs.Runs < archive.MinRuns {
			return record, fmt.Errorf("7zz wrote %d LZMA2 run(s), the largest %d bytes; the recipe needs at least %d", runs.Runs, runs.LargestRunBytes, archive.MinRuns)
		}
		record.LZMA2 = &runs
	}
	if problem != "" {
		want.SHA256 = digest
		data, err := json.MarshalIndent(want, "", "  ")
		if err != nil {
			return record, err
		}
		if err := os.WriteFile(provenancePath(options.Dir, archive.Name), append(data, '\n'), 0o644); err != nil {
			return record, err
		}
	}
	return record, nil
}

// FileSHA256 hashes one file.
func FileSHA256(path string) (string, int64, error) {
	digest := sha256.New()
	file, err := os.Open(path)
	if err != nil {
		return "", 0, err
	}
	defer file.Close()
	size, err := io.Copy(digest, file)
	if err != nil {
		return "", 0, err
	}
	return hex.EncodeToString(digest.Sum(nil)), size, nil
}

func hashInto(w io.Writer, path string) error {
	file, err := os.Open(path)
	if err != nil {
		return err
	}
	defer file.Close()
	_, err = io.Copy(w, file)
	return err
}

func writeFile(path string, generate func(io.Writer) error) error {
	temporary := path + ".tmp"
	file, err := os.Create(temporary)
	if err != nil {
		return err
	}
	if err := generate(file); err != nil {
		file.Close()
		os.Remove(temporary)
		return err
	}
	if err := file.Close(); err != nil {
		os.Remove(temporary)
		return err
	}
	return os.Rename(temporary, path)
}

func fixMtimes(root string) error {
	return filepath.WalkDir(root, func(path string, _ fs.DirEntry, err error) error {
		if err != nil {
			return err
		}
		return os.Chtimes(path, fixedTime, fixedTime)
	})
}
