package payload

import (
	"bytes"
	"crypto/sha256"
	"encoding/hex"
	"testing"
)

// The first MiB of lzma-turbo's `cargo xtask fixtures` payload. A change here
// means the corpus no longer matches the one earlier numbers were taken on.
const firstMiBSHA256 = "615592a92fa0236c35e01ae0fd21727391fe5f0c139d6d3d8643b04b4d112fbf"

func TestWriteTextMatchesLzmaTurboPayload(t *testing.T) {
	var buffer bytes.Buffer
	if err := WriteText(&buffer, 1); err != nil {
		t.Fatal(err)
	}
	if buffer.Len() != MiB {
		t.Fatalf("wrote %d bytes, want %d", buffer.Len(), MiB)
	}
	sum := sha256.Sum256(buffer.Bytes())
	if got := hex.EncodeToString(sum[:]); got != firstMiBSHA256 {
		t.Fatalf("first MiB sha256 %s, want %s", got, firstMiBSHA256)
	}
}

func TestPayloadPrefixProperty(t *testing.T) {
	var one, two bytes.Buffer
	if err := WriteText(&one, 1); err != nil {
		t.Fatal(err)
	}
	if err := WriteText(&two, 2); err != nil {
		t.Fatal(err)
	}
	if !bytes.Equal(one.Bytes(), two.Bytes()[:MiB]) {
		t.Fatal("the 1 MiB payload is not the prefix of the 2 MiB one")
	}
}

func TestGeneratorsAreSizedAndDeterministic(t *testing.T) {
	cases := map[string]func(*bytes.Buffer) error{
		"x86":   func(b *bytes.Buffer) error { return WriteCodeX86(b, 100003) },
		"arm64": func(b *bytes.Buffer) error { return WriteCodeARM64(b, 100000) },
		"audio": func(b *bytes.Buffer) error { return WriteAudio(b, 100000) },
		"media": func(b *bytes.Buffer) error { return WriteMedia(b, 100000) },
		"tree":  func(b *bytes.Buffer) error { return WriteTreeFile(b, 3, 100000) },
	}
	for name, generate := range cases {
		var first, second bytes.Buffer
		if err := generate(&first); err != nil {
			t.Fatal(name, err)
		}
		if err := generate(&second); err != nil {
			t.Fatal(name, err)
		}
		if !bytes.Equal(first.Bytes(), second.Bytes()) {
			t.Fatalf("%s is not deterministic", name)
		}
		want := 100000
		if name == "x86" {
			want = 100003
		}
		if first.Len() != want {
			t.Fatalf("%s wrote %d bytes", name, first.Len())
		}
	}
}

func TestTreeLayout(t *testing.T) {
	files := Tree(64, 4096, 1)
	if len(files) != 64 {
		t.Fatalf("%d files", len(files))
	}
	seen := map[string]bool{}
	for _, file := range files {
		if seen[file.Path] {
			t.Fatalf("duplicate path %s", file.Path)
		}
		seen[file.Path] = true
		if file.Size < 1024 || file.Size > 4096*7/4 {
			t.Fatalf("%s size %d out of range", file.Path, file.Size)
		}
	}
}

// The media source is near-incompressible: a page holds its random bytes and
// then the zero tail, and no two pages are alike.
func TestMediaIsNearIncompressible(t *testing.T) {
	var buffer bytes.Buffer
	if err := WriteMedia(&buffer, 4*mediaPage); err != nil {
		t.Fatal(err)
	}
	data := buffer.Bytes()
	for page := range 4 {
		tail := data[page*mediaPage+mediaRandom : (page+1)*mediaPage]
		if !bytes.Equal(tail, make([]byte, len(tail))) {
			t.Fatalf("page %d: the tail is not zeros", page)
		}
	}
	if bytes.Equal(data[:mediaRandom], data[mediaPage:mediaPage+mediaRandom]) {
		t.Fatal("two pages are the same")
	}
}
