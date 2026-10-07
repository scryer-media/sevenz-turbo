package toolchain

import "testing"

func TestParseBanner(t *testing.T) {
	cases := []struct{ text, version string }{
		{"\n7-Zip (z) 26.01 (arm64) : Copyright (c) 1999-2026 Igor Pavlov : 2026-04-27\n 64-bit", "26.01"},
		{"7-Zip 24.09 (x64) : Copyright (c) 1999-2024 Igor Pavlov : 2024-11-29", "24.09"},
		{"7-Zip [64] 16.02 : Copyright (c) 1999-2016 Igor Pavlov : 2016-05-21\np7zip Version 16.02", "16.02"},
	}
	for _, c := range cases {
		banner, version := ParseBanner(c.text)
		if banner == "" || version != c.version {
			t.Errorf("ParseBanner(%q) = %q, %q; want version %q", c.text, banner, version, c.version)
		}
	}
	if banner, _ := ParseBanner("usage: something else"); banner != "" {
		t.Errorf("non-7-Zip text gave banner %q", banner)
	}
}

func TestLockedVersion(t *testing.T) {
	lock := "[[package]]\nname = \"crc-fast\"\nversion = \"1.10.0\"\n\n[[package]]\nname = \"lzma-turbo\"\nversion = \"0.6.0\"\nsource = \"registry\"\n"
	if got := LockedVersion(lock, "lzma-turbo"); got != "0.6.0" {
		t.Fatalf("got %q", got)
	}
	if got := LockedVersion(lock, "absent"); got != "" {
		t.Fatalf("got %q", got)
	}
}

func TestLastJSON(t *testing.T) {
	object, err := LastJSON([]byte("noise\n{\"ok\":true,\"lzma_turbo\":\"0.6.0\"}\n"))
	if err != nil || object["lzma_turbo"] != "0.6.0" {
		t.Fatalf("got %v, %v", object, err)
	}
	if _, err := LastJSON([]byte("not json\n")); err == nil {
		t.Fatal("want an error")
	}
}
