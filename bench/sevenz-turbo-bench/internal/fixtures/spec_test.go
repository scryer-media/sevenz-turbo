package fixtures

import (
	"os"
	"slices"
	"strings"
	"testing"
)

func TestProfilesShareOneRecipe(t *testing.T) {
	full, quick := Full(), Quick()
	only := len(FullOnlyArchives)
	if len(full.Archives) != len(quick.Archives)+only || len(full.Sources) != len(quick.Sources)+only {
		t.Fatal("full and quick profiles list different fixtures beyond the full-only ones")
	}
	for _, archive := range full.Archives {
		if _, ok := full.Source(archive.Source); !ok {
			t.Errorf("%s: unknown source %s", archive.Name, archive.Source)
		}
		if _, ok := quick.Archive(archive.Name); !ok && !slices.Contains(FullOnlyArchives, archive.Name) {
			t.Errorf("%s missing from quick", archive.Name)
		}
		hasPassword := false
		for _, arg := range archive.Args {
			hasPassword = hasPassword || strings.HasPrefix(arg, "-p")
		}
		if hasPassword != archive.Encrypted {
			t.Errorf("%s: Encrypted=%t but password switch present=%t", archive.Name, archive.Encrypted, hasPassword)
		}
	}
}

// readmePending are archives whose README row awaits the operator's approval
// of the Markdown edit. Remove a name once bench/fixtures/README.md lists it.
var readmePending = map[string]bool{}

// The fixtures README lists every archive the recipe writes.
func TestReadmeListsEveryArchive(t *testing.T) {
	readme, err := os.ReadFile("../../../fixtures/README.md")
	if err != nil {
		t.Fatal(err)
	}
	for _, archive := range Full().Archives {
		listed := strings.Contains(string(readme), "`"+archive.Name+"`")
		if listed && readmePending[archive.Name] {
			t.Errorf("bench/fixtures/README.md lists %s: drop it from readmePending", archive.Name)
		}
		if !listed && !readmePending[archive.Name] {
			t.Errorf("bench/fixtures/README.md does not list %s", archive.Name)
		}
	}
}
