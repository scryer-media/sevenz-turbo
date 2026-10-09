package suite

import (
	"encoding/json"
	"strings"
	"testing"
)

func TestRedactReplacesEveryPlaceLongestFirst(t *testing.T) {
	raw := &Raw{Runs: []RunRecord{
		{Tool: "/u/someone/bin/7zz", Command: "/u/someone/bin/7zz t /u/someone/corpus/full/mt.7z"},
		{Tool: `C:\Users\someone\bin\7za.exe`, Command: `dist\decode-bench.exe op decode --archive C:\Users\someone\corpus\full\mt.7z`},
	}}
	places := []Place{
		{Path: "/u/someone", Label: "~"},
		{Path: "/u/someone/corpus/full/", Label: "<fixtures>"},
		{Path: `C:\Users\someone`, Label: "~"},
		{Path: `C:\Users\someone\corpus\full`, Label: "<fixtures>"},
		{Path: "/", Label: "<root>"},
	}
	if err := Redact(raw, places); err != nil {
		t.Fatal(err)
	}
	want := []RunRecord{
		{Tool: "~/bin/7zz", Command: "~/bin/7zz t <fixtures>/mt.7z"},
		{Tool: `~\bin\7za.exe`, Command: `dist\decode-bench.exe op decode --archive <fixtures>\mt.7z`},
	}
	for i, run := range raw.Runs {
		if run.Tool != want[i].Tool || run.Command != want[i].Command {
			t.Errorf("run %d = %q %q, want %q %q", i, run.Tool, run.Command, want[i].Tool, want[i].Command)
		}
	}
	data, err := json.Marshal(raw)
	if err != nil {
		t.Fatal(err)
	}
	if strings.Contains(string(data), "someone") {
		t.Errorf("the account survived redaction: %s", data)
	}
}
