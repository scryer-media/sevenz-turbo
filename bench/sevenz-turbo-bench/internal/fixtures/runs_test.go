package fixtures

import (
	"bytes"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

// stream builds an archive's first 32 bytes and an LZMA2 stream after them
// from chunk descriptions: 'R' an uncompressed chunk that resets the
// dictionary, 'u' an uncompressed chunk, 'L' a compressed chunk that resets
// it, 'l' a compressed chunk with new properties, 'c' a plain compressed
// chunk. Every chunk unpacks to unpacked bytes; a compressed one packs to 7.
func stream(chunks string, unpacked int, end bool) []byte {
	out := append([]byte{}, signature...)
	out = append(out, make([]byte, packStart-len(signature))...)
	for _, kind := range chunks {
		size := unpacked - 1
		switch kind {
		case 'R', 'u':
			control := byte(0x02)
			if kind == 'R' {
				control = 0x01
			}
			out = append(out, control, byte(size>>8), byte(size))
			out = append(out, make([]byte, unpacked)...)
		default:
			control := map[rune]byte{'L': 0xE0, 'l': 0xC0, 'c': 0x80}[kind] | byte(size>>16)
			out = append(out, control, byte(size>>8), byte(size), 0, 6)
			if control >= 0xC0 {
				out = append(out, 0x5D)
			}
			out = append(out, make([]byte, 7)...)
		}
	}
	if end {
		out = append(out, 0x00)
	}
	// What follows the stream in an archive (its header) is not walked.
	return append(out, 0xFF, 0xFF, 0xFF)
}

func TestRunsAreCountedFromTheStream(t *testing.T) {
	data := stream("RuuLcclcRL", 1000, true)
	runs, err := CountLZMA2Runs(bytes.NewReader(data), int64(len(data)))
	if err != nil {
		t.Fatal(err)
	}
	// R u u | L c c l c | R | L, then the end marker.
	want := LZMA2Runs{Runs: 4, UnpackedBytes: 10000, LargestRunBytes: 5000, SmallestRunBytes: 1000,
		UncompressedChunks: 4, CompressedChunks: 6,
		// Four uncompressed chunks of 3+1000, three compressed with a
		// properties byte (6+7), three without (5+7), and the end marker.
		PackedBytes: 4*1003 + 3*13 + 3*12 + 1, LargestRunPacked: 3009}
	if runs != want {
		t.Fatalf("got %+v\nwant %+v", runs, want)
	}
}

func TestACompressedChunkCarriesItsHighSizeBits(t *testing.T) {
	data := stream("Lc", 0x12345, true)
	runs, err := CountLZMA2Runs(bytes.NewReader(data), int64(len(data)))
	if err != nil {
		t.Fatal(err)
	}
	if runs.Runs != 1 || runs.UnpackedBytes != 2*0x12345 {
		t.Fatalf("%+v", runs)
	}
}

func TestAStreamThatIsNotLZMA2IsRefused(t *testing.T) {
	cases := map[string]struct {
		data []byte
		want string
	}{
		"no signature":     {append([]byte("PK\x03\x04"), make([]byte, 64)...), "not a 7z archive"},
		"no end marker":    {stream("Ru", 100, false)[:packStart+2*103], "no end marker"},
		"reserved control": {append(stream("R", 100, false)[:packStart+103], 0x03, 0, 0, 0), "not an LZMA2 chunk header"},
		"no first reset":   {stream("uR", 100, true), "does not begin with a dictionary reset"},
		"cut header":       {stream("R", 100, false)[:packStart+103+2], "cut short"},
	}
	for name, c := range cases {
		if name == "cut header" {
			c.data[len(c.data)-2] = 0x80
		}
		_, err := CountLZMA2Runs(bytes.NewReader(c.data), int64(len(c.data)))
		if err == nil || !strings.Contains(err.Error(), c.want) {
			t.Errorf("%s: error %v, want one naming %q", name, err, c.want)
		}
	}
}

// A count is only an archive's if the stream decodes to the archive's bytes:
// a second folder, or a coder that is not LZMA2, shows as a different total.
func TestACountIsHeldToTheSourceSize(t *testing.T) {
	path := filepath.Join(t.TempDir(), "a.7z")
	if err := os.WriteFile(path, stream("RL", 1000, true), 0o644); err != nil {
		t.Fatal(err)
	}
	if runs, err := countRuns(path, 2000); err != nil || runs.Runs != 2 {
		t.Fatalf("runs %+v, error %v", runs, err)
	}
	if _, err := countRuns(path, 3000); err == nil || !strings.Contains(err.Error(), "decodes to 2000 bytes") {
		t.Fatalf("error %v", err)
	}
}

// The run fixtures are sized to have more runs than threads, and say so.
func TestTheRunFixturesAreFullOnly(t *testing.T) {
	full, quick := Full(), Quick()
	widest, ok := full.Archive("media_mx5_3g.7z")
	if !ok || widest.MinRuns <= WidestHostThreads {
		t.Fatalf("media_mx5_3g.7z: in full %t, MinRuns %d, want above %d", ok, widest.MinRuns, WidestHostThreads)
	}
	for _, name := range FullOnlyArchives {
		archive, ok := full.Archive(name)
		if !ok {
			t.Fatalf("%s is not in the full profile", name)
		}
		if _, ok := quick.Archive(name); ok {
			t.Errorf("%s is in the quick profile", name)
		}
		if _, ok := quick.Source(archive.Source); ok {
			t.Errorf("%s: its source %s is in the quick profile", name, archive.Source)
		}
		source, _ := full.Source(archive.Source)
		if source.Kind != KindMedia || archive.MinRuns < 9 {
			t.Errorf("%s: source kind %s, MinRuns %d", name, source.Kind, archive.MinRuns)
		}
	}
}
