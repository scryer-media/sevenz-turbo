package fixtures

import (
	"encoding/json"
	"io"
	"os"
	"path/filepath"
	"testing"
)

func smallTree() SourceSpec {
	return SourceSpec{Name: "tree-small", Kind: KindTree, Files: 6, AverageBytes: 512, Seed: 7}
}

func TestATreeIsReusedOnlyWhenItIsExactlyTheRecipe(t *testing.T) {
	dir := t.TempDir()
	source := smallTree()
	first, err := ensureSource(dir, source, func(string, ...any) {})
	if err != nil {
		t.Fatal(err)
	}
	root := SourceDir(dir, source.Name)
	if problem := sourceProblem(root, source); problem != "" {
		t.Fatalf("a freshly written tree is refused: %s", problem)
	}

	extra := filepath.Join(root, "stray.bin")
	if err := os.WriteFile(extra, []byte("x"), 0o644); err != nil {
		t.Fatal(err)
	}
	if problem := sourceProblem(root, source); problem == "" {
		t.Fatal("a tree with an extra member was accepted")
	}
	again, err := ensureSource(dir, source, func(string, ...any) {})
	if err != nil {
		t.Fatal(err)
	}
	if _, err := os.Stat(extra); !os.IsNotExist(err) {
		t.Fatal("regeneration kept the extra member")
	}
	if again.SHA256 != first.SHA256 || again.TotalBytes != first.TotalBytes {
		t.Fatal("regeneration wrote different content")
	}

	changed := source
	changed.Seed++
	if problem := sourceProblem(root, changed); problem == "" {
		t.Fatal("a tree written for another seed was accepted")
	}
}

func TestASingleFileSourceIsBoundToItsRecipe(t *testing.T) {
	dir := t.TempDir()
	source := SourceSpec{Name: "audio-small", Kind: KindAudio, Bytes: 64 << 10}
	if _, err := ensureSource(dir, source, func(string, ...any) {}); err != nil {
		t.Fatal(err)
	}
	root := SourceDir(dir, source.Name)
	if err := os.WriteFile(filepath.Join(root, "extra.wav"), nil, 0o644); err != nil {
		t.Fatal(err)
	}
	if problem := sourceProblem(root, source); problem == "" {
		t.Fatal("a single-file source with an extra member was accepted")
	}
}

func TestVerifyRejectsAChangedCorpus(t *testing.T) {
	dir := t.TempDir()
	source := smallTree()
	record, err := ensureSource(dir, source, func(string, ...any) {})
	if err != nil {
		t.Fatal(err)
	}
	archivePath := filepath.Join(dir, "a.7z")
	if err := os.WriteFile(archivePath, []byte("archive bytes"), 0o644); err != nil {
		t.Fatal(err)
	}
	digest, size, err := FileSHA256(archivePath)
	if err != nil {
		t.Fatal(err)
	}
	manifest := &Manifest{Sources: []SourceRecord{record},
		Archives: []ArchiveRecord{{ArchiveSpec: ArchiveSpec{Name: "a.7z", Source: source.Name}, Bytes: size, SHA256: digest}}}
	if err := manifest.Verify(dir); err != nil {
		t.Fatalf("an untouched corpus fails verification: %v", err)
	}

	if err := os.WriteFile(archivePath, []byte("archive byteZ"), 0o644); err != nil {
		t.Fatal(err)
	}
	if err := manifest.Verify(dir); err == nil {
		t.Fatal("a changed archive passed verification")
	}
	if err := os.WriteFile(archivePath, []byte("archive bytes"), 0o644); err != nil {
		t.Fatal(err)
	}

	member := filepath.Join(SourceDir(dir, source.Name), filepath.FromSlash(firstMember(t, source)))
	file, err := os.OpenFile(member, os.O_WRONLY, 0)
	if err != nil {
		t.Fatal(err)
	}
	if _, err := file.WriteAt([]byte{0xff}, 0); err != nil {
		t.Fatal(err)
	}
	if err := file.Close(); err != nil {
		t.Fatal(err)
	}
	if err := manifest.Verify(dir); err == nil {
		t.Fatal("a changed source member passed verification")
	}
}

func firstMember(t *testing.T, source SourceSpec) string {
	t.Helper()
	for path := range expectedMembers(source) {
		return path
	}
	t.Fatal("empty source")
	return ""
}

func TestACachedArchiveIsReusedOnlyUnderItsProvenance(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "a.7z")
	if err := os.WriteFile(path, []byte("archive"), 0o644); err != nil {
		t.Fatal(err)
	}
	want := provenance{Oracle: Oracle{SHA256: "oracle-1", Banner: "7-Zip 26.00"}, Source: "text", SourceSHA: "src-1", Args: []string{"-mx=5"}}
	if problem := archiveProblem(dir, path, want); problem == "" {
		t.Fatal("an archive without provenance was reused")
	}
	recorded := want
	digest, _, err := FileSHA256(path)
	if err != nil {
		t.Fatal(err)
	}
	recorded.SHA256 = digest
	writeProvenance(t, dir, recorded)
	if problem := archiveProblem(dir, path, want); problem != "" {
		t.Fatalf("a matching archive was refused: %s", problem)
	}
	for name, change := range map[string]func(*provenance){
		"oracle":  func(p *provenance) { p.Oracle.SHA256 = "oracle-2" },
		"source":  func(p *provenance) { p.SourceSHA = "src-2" },
		"args":    func(p *provenance) { p.Args = []string{"-mx=9"} },
		"encrypt": func(p *provenance) { p.Encrypted = true },
	} {
		other := want
		other.Args = append([]string(nil), want.Args...)
		change(&other)
		if problem := archiveProblem(dir, path, other); problem == "" {
			t.Errorf("%s changed but the archive was reused", name)
		}
	}
	if err := os.WriteFile(path, []byte("archivf"), 0o644); err != nil {
		t.Fatal(err)
	}
	if problem := archiveProblem(dir, path, want); problem == "" {
		t.Fatal("an archive changed after it was written was reused")
	}
}

func writeProvenance(t *testing.T, dir string, p provenance) {
	t.Helper()
	file, err := os.Create(provenancePath(dir, "a.7z"))
	if err != nil {
		t.Fatal(err)
	}
	defer file.Close()
	if _, err := io.WriteString(file, mustJSON(t, p)); err != nil {
		t.Fatal(err)
	}
}

func mustJSON(t *testing.T, value any) string {
	t.Helper()
	data, err := json.Marshal(value)
	if err != nil {
		t.Fatal(err)
	}
	return string(data)
}
