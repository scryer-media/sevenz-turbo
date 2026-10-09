package fixtures

import (
	"bytes"
	"errors"
	"fmt"
	"io"
	"os"
)

// LZMA2Runs is the run layout of an archive's LZMA2 stream, counted from the
// stream: a run begins at every chunk that resets the dictionary, and is what
// a parallel decoder can hand to one thread. Sizes are in bytes; a run's
// packed size counts its chunk headers.
type LZMA2Runs struct {
	Runs             int   `json:"runs"`
	UnpackedBytes    int64 `json:"unpacked_bytes"`
	PackedBytes      int64 `json:"packed_bytes"`
	LargestRunBytes  int64 `json:"largest_run_bytes"`
	SmallestRunBytes int64 `json:"smallest_run_bytes"`
	LargestRunPacked int64 `json:"largest_run_packed_bytes"`
	// UncompressedChunks and CompressedChunks say how the encoder stored the
	// data: a near-incompressible stream is mostly the first kind.
	UncompressedChunks int64 `json:"uncompressed_chunks"`
	CompressedChunks   int64 `json:"compressed_chunks"`
}

// signature opens every 7z archive.
var signature = []byte{'7', 'z', 0xBC, 0xAF, 0x27, 0x1C}

// packStart is where a 7zz-written archive's first packed stream begins: the
// signature header is 32 bytes and 7zz puts nothing between it and the data.
const packStart = 32

// CountLZMA2Runs walks the chunk headers of the LZMA2 stream that is the first
// packed stream of the archive in r, to the stream's end marker. It reads the
// headers only. It is for an archive of one folder whose only coder is LZMA2,
// not encrypted, as 7zz writes one file with `-m0=lzma2`; anything else is
// either reported as not being LZMA2 chunks or gives totals the caller can see
// are not the archive's (UnpackedBytes is every byte the stream decodes to).
func CountLZMA2Runs(r io.ReaderAt, size int64) (LZMA2Runs, error) {
	var runs LZMA2Runs
	head := make([]byte, 6)
	if _, err := r.ReadAt(head, 0); err != nil || !bytes.Equal(head, signature) {
		return runs, errors.New("not a 7z archive")
	}
	at := int64(packStart)
	var runUnpacked, runPacked int64
	closeRun := func() {
		if runs.Runs == 0 {
			return
		}
		runs.LargestRunBytes = max(runs.LargestRunBytes, runUnpacked)
		runs.LargestRunPacked = max(runs.LargestRunPacked, runPacked)
		if runs.SmallestRunBytes == 0 || runUnpacked < runs.SmallestRunBytes {
			runs.SmallestRunBytes = runUnpacked
		}
	}
	for {
		if at >= size {
			return runs, fmt.Errorf("the LZMA2 stream has no end marker before offset %d", size)
		}
		n, err := r.ReadAt(head, at)
		if n == 0 {
			return runs, fmt.Errorf("chunk header at offset %d: %w", at, err)
		}
		control := head[0]
		if control == 0x00 {
			closeRun()
			runs.PackedBytes = at + 1 - packStart
			return runs, nil
		}
		var header, packed, unpacked int64
		var reset bool
		switch {
		case control == 0x01 || control == 0x02:
			if n < 3 {
				return runs, fmt.Errorf("chunk header at offset %d is cut short", at)
			}
			header = 3
			unpacked = int64(head[1])<<8 | int64(head[2]) + 1
			packed = unpacked
			reset = control == 0x01
			runs.UncompressedChunks++
		case control >= 0x80:
			header = 5
			if control >= 0xC0 {
				header = 6
			}
			if int64(n) < header {
				return runs, fmt.Errorf("chunk header at offset %d is cut short", at)
			}
			unpacked = int64(control&0x1F)<<16 | int64(head[1])<<8 | int64(head[2]) + 1
			packed = int64(head[3])<<8 | int64(head[4]) + 1
			reset = control >= 0xE0
			runs.CompressedChunks++
		default:
			return runs, fmt.Errorf("offset %d is not an LZMA2 chunk header (control byte %#02x)", at, control)
		}
		if reset {
			closeRun()
			runs.Runs++
			runUnpacked, runPacked = 0, 0
		} else if runs.Runs == 0 {
			return runs, fmt.Errorf("the stream at offset %d does not begin with a dictionary reset: not an LZMA2 stream", at)
		}
		runUnpacked += unpacked
		runPacked += header + packed
		runs.UnpackedBytes += unpacked
		at += header + packed
	}
}

// countRuns is CountLZMA2Runs over the file at path, held to the bytes the
// archive is recorded as unpacking to.
func countRuns(path string, unpackedBytes int64) (LZMA2Runs, error) {
	file, err := os.Open(path)
	if err != nil {
		return LZMA2Runs{}, err
	}
	defer file.Close()
	info, err := file.Stat()
	if err != nil {
		return LZMA2Runs{}, err
	}
	runs, err := CountLZMA2Runs(file, info.Size())
	if err != nil {
		return runs, err
	}
	if runs.UnpackedBytes != unpackedBytes {
		return runs, fmt.Errorf("its LZMA2 stream decodes to %d bytes, the source is %d: not one LZMA2 folder", runs.UnpackedBytes, unpackedBytes)
	}
	return runs, nil
}
