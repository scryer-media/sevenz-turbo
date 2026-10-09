package suite

import (
	"encoding/json"
	"sort"
	"strings"
)

// Place is a directory whose absolute path a run's records name and that a
// published report replaces with Label: the corpus, the run's scratch and
// output directories, the checkout, the home directory.
type Place struct {
	Path  string
	Label string
}

// Redact replaces every Place's absolute path in raw's text with its label,
// so raw.json and the reports built from it say where a file sat relative to
// the corpus or the checkout without naming the machine's directories or the
// account that ran them. The longest path is replaced first, so the corpus
// inside the home directory reads as <fixtures>, not ~/.../fixtures. A path
// is replaced in the form JSON writes it, so a Windows path's backslashes
// match.
func Redact(raw *Raw, places []Place) error {
	data, err := json.Marshal(raw)
	if err != nil {
		return err
	}
	kept := make([]Place, 0, len(places))
	for _, place := range places {
		// A root is not a prefix worth replacing: it would rewrite every path.
		if trimmed := strings.TrimRight(place.Path, `/\`); len(trimmed) > 3 {
			kept = append(kept, Place{Path: trimmed, Label: place.Label})
		}
	}
	sort.SliceStable(kept, func(i, j int) bool { return len(kept[i].Path) > len(kept[j].Path) })
	text := string(data)
	for _, place := range kept {
		path, err := json.Marshal(place.Path)
		if err != nil {
			return err
		}
		text = strings.ReplaceAll(text, strings.Trim(string(path), `"`), place.Label)
	}
	redacted := &Raw{}
	if err := json.Unmarshal([]byte(text), redacted); err != nil {
		return err
	}
	*raw = *redacted
	return nil
}
