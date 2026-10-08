// Package fixtures defines and generates the archive corpus: synthetic
// sources written by package payload, packed by the official `7zz a`, with
// every size and SHA-256 recorded in fixtures.json. No fixture byte is ever
// committed; every host generates its own corpus from the same recipe.
package fixtures

import "fmt"

// Password is the passphrase of every encrypted fixture. It is a benchmark
// constant, not a secret.
const Password = "bench-passphrase"

// Source kinds.
const (
	KindText      = "text"
	KindCodeX86   = "code-x86"
	KindCodeARM64 = "code-arm64"
	KindAudio     = "audio"
	KindMedia     = "media"
	KindTree      = "tree"
)

// SourceSpec is one generated input: a directory under src/ holding either one
// generated file or a tree of small ones.
type SourceSpec struct {
	Name string `json:"name"`
	Kind string `json:"kind"`
	// Bytes is the size of a single-file source.
	Bytes int64 `json:"bytes,omitempty"`
	// Files and AverageBytes shape a tree source.
	Files        int    `json:"files,omitempty"`
	AverageBytes int64  `json:"average_bytes,omitempty"`
	Seed         uint64 `json:"seed,omitempty"`
	Note         string `json:"note"`
}

// FileName is the generated file of a single-file source.
func (s SourceSpec) FileName() string { return s.Name + ".bin" }

// ArchiveSpec is one fixture archive: a source packed with the given `7zz a`
// switches.
type ArchiveSpec struct {
	Name      string   `json:"name"`
	Source    string   `json:"source"`
	Args      []string `json:"args"`
	Encrypted bool     `json:"encrypted,omitempty"`
	Note      string   `json:"note"`
}

// Profile is a whole corpus recipe.
type Profile struct {
	Name     string        `json:"name"`
	Sources  []SourceSpec  `json:"sources"`
	Archives []ArchiveSpec `json:"archives"`
}

// Source looks a source up by name.
func (p Profile) Source(name string) (SourceSpec, bool) {
	for _, source := range p.Sources {
		if source.Name == name {
			return source, true
		}
	}
	return SourceSpec{}, false
}

// Archive looks an archive up by name.
func (p Profile) Archive(name string) (ArchiveSpec, bool) {
	for _, archive := range p.Archives {
		if archive.Name == name {
			return archive, true
		}
	}
	return ArchiveSpec{}, false
}

const mib = int64(1) << 20

// sizes is what differs between the full corpus and the quick one.
type sizes struct {
	payload, sub, code, audio, media int64
	treeFiles                        int
	treeAverage                      int64
	kdfFiles                         int
	// chunk is the explicit LZMA2 block size of the multi-threaded fixtures,
	// or "" for 7zz's own (four dictionaries, 64 MiB at -mx5). The quick
	// corpus is too small for 64 MiB blocks to cut it at all.
	chunk string
}

// Full is the fleet corpus: the 1 GiB payload docs/benchmarking.md was
// measured on, and inputs large enough that every row is seconds, not
// milliseconds.
func Full() Profile {
	return build("full", sizes{
		payload: 1024 * mib, sub: 256 * mib, code: 64 * mib, audio: 256 * mib, media: 1024 * mib,
		treeFiles: 8192, treeAverage: 32 << 10, kdfFiles: 2048,
	})
}

// Quick is the smoke corpus: the same recipe, small enough to generate and
// run in a few minutes, to prove a host and a build work end to end.
func Quick() Profile {
	return build("quick", sizes{
		payload: 32 * mib, sub: 16 * mib, code: 4 * mib, audio: 8 * mib, media: 16 * mib,
		treeFiles: 512, treeAverage: 16 << 10, kdfFiles: 128, chunk: "4m",
	})
}

// ByName returns the named profile.
func ByName(name string) (Profile, error) {
	switch name {
	case "full":
		return Full(), nil
	case "quick":
		return Quick(), nil
	}
	return Profile{}, fmt.Errorf("unknown profile %q (want full or quick)", name)
}

func build(name string, s sizes) Profile {
	lzma2MT := "-m0=lzma2"
	if s.chunk != "" {
		lzma2MT = "-m0=lzma2:c=" + s.chunk
	}
	password := "-p" + Password
	return Profile{
		Name: name,
		Sources: []SourceSpec{
			{Name: "payload", Kind: KindText, Bytes: s.payload, Note: "lzma-turbo's xtask payload (SplitMix64 seed 7): hex words and random runs"},
			{Name: "payload-sub", Kind: KindText, Bytes: s.sub, Note: "the first bytes of the same payload"},
			{Name: "code-x86", Kind: KindCodeX86, Bytes: s.code, Note: "synthetic x86-64-shaped code with dense E8/E9 rel32 branches"},
			{Name: "code-arm64", Kind: KindCodeARM64, Bytes: s.code, Note: "synthetic AArch64-shaped code with dense BL branches"},
			{Name: "audio", Kind: KindAudio, Bytes: s.audio, Note: "synthetic 16-bit stereo PCM, two tones plus noise"},
			{Name: "media", Kind: KindMedia, Bytes: s.media, Note: "near-incompressible pages (4016 random bytes, 80 zeros): the already-compressed video and audio of a usenet download"},
			{Name: "tree", Kind: KindTree, Files: s.treeFiles, AverageBytes: s.treeAverage, Seed: 0x7EE, Note: "many small text members under invented directories"},
			{Name: "kdf-tree", Kind: KindTree, Files: s.kdfFiles, AverageBytes: 2 << 10, Seed: 0xCDF, Note: "many tiny members, for the per-folder key-derivation cost"},
		},
		Archives: []ArchiveSpec{
			{Name: "st.7z", Source: "payload", Args: []string{"-mx=5", "-m0=lzma2", "-mmt=1"}, Note: "one LZMA2 stream with no dictionary resets: cannot be decoded in parallel by anyone"},
			{Name: "mt.7z", Source: "payload", Args: []string{"-mx=5", lzma2MT, "-mmt=8"}, Note: "LZMA2 written block-parallel (dictionary reset per block): the parallel decode fixture; -mmt=8, not -mmt=on, so the layout is the same on every host"},
			{Name: "lzma.7z", Source: "payload-sub", Args: []string{"-mx=5", "-m0=lzma", "-mmt=1"}, Note: "LZMA (not LZMA2), one stream"},
			{Name: "tree_solid.7z", Source: "tree", Args: []string{"-mx=5", lzma2MT, "-mmt=8", "-ms=on"}, Note: "many small members in one solid folder"},
			{Name: "tree_nonsolid.7z", Source: "tree", Args: []string{"-mx=5", "-m0=lzma2", "-mmt=8", "-ms=off"}, Note: "the same members, one folder each"},
			{Name: "aes_store.7z", Source: "payload", Args: []string{"-mx=0", password, "-mhe=on"}, Encrypted: true, Note: "stored and AES-256 encrypted: decrypt + CRC is nearly all of the work"},
			{Name: "aes_mx1.7z", Source: "payload", Args: []string{"-mx=1", lzma2MT, "-mmt=8", password, "-mhe=on"}, Encrypted: true, Note: "LZMA2 -mx1 under AES-256"},
			{Name: "aes_kdf.7z", Source: "kdf-tree", Args: []string{"-mx=5", "-m0=lzma2", "-ms=off", password, "-mhe=on"}, Encrypted: true, Note: "tiny members, one encrypted folder each, encrypted header: SHA-256 key derivation dominated"},
			{Name: "bcj_x86.7z", Source: "code-x86", Args: []string{"-mx=5", "-mmt=1", "-mf=BCJ"}, Note: "BCJ x86 + LZMA2"},
			{Name: "bcj_arm64.7z", Source: "code-arm64", Args: []string{"-mx=5", "-mmt=1", "-mf=ARM64"}, Note: "BCJ ARM64 + LZMA2"},
			{Name: "bcj2.7z", Source: "code-x86", Args: []string{"-mx=5", "-mmt=1", "-mf=BCJ2"}, Note: "BCJ2 (four streams) + LZMA2/LZMA"},
			{Name: "delta.7z", Source: "audio", Args: []string{"-mx=5", "-mmt=1", "-mf=Delta:4"}, Note: "delta distance 4 + LZMA2"},
			{Name: "media_mx1.7z", Source: "media", Args: []string{"-mx=1", lzma2MT, "-mmt=8"}, Note: "near-incompressible LZMA2 -mx1 in parallel blocks: mostly uncompressed chunks, the download-shaped decode"},
			{Name: "media_mx5.7z", Source: "media", Args: []string{"-mx=5", lzma2MT, "-mmt=8"}, Note: "the same at -mx5: fewer, larger blocks, so the parallel decoder holds fewer runs at once"},
			{Name: "ppmd.7z", Source: "payload-sub", Args: []string{"-mx=5", "-m0=PPMd", "-mmt=1"}, Note: "PPMd (decoded through an external PPMd crate, named in the toolchain record): secondary row"},
		},
	}
}
