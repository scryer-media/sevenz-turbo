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
// already on disk, then writes the manifest with every size and digest.
// Files already present are kept and re-hashed, never rewritten.
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

func ensureSource(dir string, source SourceSpec, logf func(string, ...any)) (SourceRecord, error) {
	root := SourceDir(dir, source.Name)
	record := SourceRecord{SourceSpec: source, Path: filepath.Join("src", source.Name)}
	if source.Kind == KindTree {
		files := payload.Tree(source.Files, source.AverageBytes, source.Seed)
		marker := filepath.Join(root, ".complete")
		if _, err := os.Stat(marker); err != nil {
			logf("fixtures: generating %s (%d members)", source.Name, len(files))
			if err := os.RemoveAll(root); err != nil {
				return record, err
			}
			for index, file := range files {
				path := filepath.Join(root, filepath.FromSlash(file.Path))
				if err := os.MkdirAll(filepath.Dir(path), 0o755); err != nil {
					return record, err
				}
				if err := writeFile(path, func(w io.Writer) error { return payload.WriteTreeFile(w, index, file.Size) }); err != nil {
					return record, err
				}
			}
			if err := fixMtimes(root); err != nil {
				return record, err
			}
			if err := os.WriteFile(marker, nil, 0o644); err != nil {
				return record, err
			}
		}
		digest := sha256.New()
		for _, file := range files {
			path := filepath.Join(root, filepath.FromSlash(file.Path))
			fmt.Fprintf(digest, "%s\x00%d\x00", file.Path, file.Size)
			if err := hashInto(digest, path); err != nil {
				return record, err
			}
			record.TotalBytes += file.Size
		}
		record.FileCount = len(files)
		record.SHA256 = hex.EncodeToString(digest.Sum(nil))
		return record, nil
	}

	path := filepath.Join(root, source.FileName())
	if info, err := os.Stat(path); err != nil || info.Size() != source.Bytes {
		logf("fixtures: generating %s (%d MiB)", source.Name, source.Bytes>>20)
		if err := os.MkdirAll(root, 0o755); err != nil {
			return record, err
		}
		generate := map[string]func(io.Writer) error{
			KindText:      func(w io.Writer) error { return payload.WriteText(w, int(source.Bytes/payload.MiB)) },
			KindCodeX86:   func(w io.Writer) error { return payload.WriteCodeX86(w, source.Bytes) },
			KindCodeARM64: func(w io.Writer) error { return payload.WriteCodeARM64(w, source.Bytes) },
			KindAudio:     func(w io.Writer) error { return payload.WriteAudio(w, source.Bytes) },
		}[source.Kind]
		if generate == nil {
			return record, fmt.Errorf("unknown source kind %q", source.Kind)
		}
		if err := writeFile(path, generate); err != nil {
			return record, err
		}
		if err := os.Chtimes(path, fixedTime, fixedTime); err != nil {
			return record, err
		}
	}
	digest, size, err := FileSHA256(path)
	if err != nil {
		return record, err
	}
	record.TotalBytes, record.FileCount, record.SHA256 = size, 1, digest
	return record, nil
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

func ensureArchive(ctx context.Context, options Options, archive ArchiveSpec, source SourceRecord, logf func(string, ...any)) (ArchiveRecord, error) {
	record := ArchiveRecord{ArchiveSpec: archive, Path: archive.Name, UnpackedBytes: source.TotalBytes, Entries: source.FileCount}
	path := filepath.Join(options.Dir, archive.Name)
	if _, err := os.Stat(path); err != nil {
		if options.SevenZip.Path == "" {
			return record, errors.New("no 7zz to write it with")
		}
		logf("fixtures: 7zz a %s %s", strings.Join(archive.Args, " "), archive.Name)
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
