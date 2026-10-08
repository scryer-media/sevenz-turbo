// Package host describes the machine a run is on: OS, architecture, CPU
// model, the ISA extensions the codecs and ciphers dispatch on, core count,
// memory, and the load average before each row.
package host

import (
	"bufio"
	"bytes"
	"context"
	"os"
	"os/exec"
	"runtime"
	"sort"
	"strconv"
	"strings"

	"golang.org/x/sys/cpu"
)

// Machine is the host descriptor every report carries. The first fields keep
// rarpar-bench's JSON names.
type Machine struct {
	Label        string `json:"label"`
	OS           string `json:"os"`
	Kernel       string `json:"kernel"`
	Architecture string `json:"architecture"`
	CPU          string `json:"cpu"`
	CPUCount     int    `json:"cpu_count"`
	MemoryBytes  uint64 `json:"memory_bytes,omitempty"`
	// ISA lists the extensions present, by the names in WantedISA, sorted.
	ISA []string `json:"isa_flags"`
	// ISASources says where the flags were read: "x/sys/cpu" (CPUID or the
	// OS's hwcaps), "/proc/cpuinfo", "sysctl hw.optional".
	ISASources []string `json:"isa_sources"`
	// InstanceType is the operator-supplied instance type
	// (SEVENZ_BENCH_INSTANCE_TYPE), e.g. "c7i.4xlarge"; never probed.
	InstanceType string `json:"instance_type,omitempty"`
}

// WantedISA is every flag the descriptor reports, by architecture. They are
// the extensions lzma-turbo, crc-fast, AWS-LC and RustCrypto dispatch on.
var WantedISA = map[string][]string{
	"amd64": {"sse4.2", "pclmulqdq", "aes", "sha_ni", "bmi2", "avx2", "avx512f", "avx512bw", "avx512vl",
		"avx512vbmi", "avx512vbmi2", "gfni", "vaes", "vpclmulqdq"},
	"arm64": {"neon", "aes", "pmull", "sha2", "sha3", "sha512", "crc32", "sve", "sve2"},
}

// Collect describes this host.
func Collect(ctx context.Context, label string) Machine {
	machine := Machine{
		Label:        label,
		OS:           runtime.GOOS,
		Architecture: runtime.GOARCH,
		CPUCount:     runtime.NumCPU(),
		InstanceType: os.Getenv("SEVENZ_BENCH_INSTANCE_TYPE"),
	}
	machine.Kernel = kernel(ctx)
	machine.CPU = cpuModel(ctx)
	machine.MemoryBytes = memoryBytes(ctx)
	flags := map[string]bool{}
	for flag, present := range xsysFlags() {
		if present {
			flags[flag] = true
		}
	}
	machine.ISASources = append(machine.ISASources, "x/sys/cpu")
	if extra, source := osFlags(ctx); source != "" {
		for flag := range extra {
			flags[flag] = true
		}
		machine.ISASources = append(machine.ISASources, source)
	}
	wanted := map[string]bool{}
	for _, flag := range WantedISA[runtime.GOARCH] {
		wanted[flag] = true
	}
	for flag := range flags {
		if wanted[flag] {
			machine.ISA = append(machine.ISA, flag)
		}
	}
	sort.Strings(machine.ISA)
	if machine.ISA == nil {
		machine.ISA = []string{}
	}
	return machine
}

func xsysFlags() map[string]bool {
	switch runtime.GOARCH {
	case "amd64":
		x := cpu.X86
		return map[string]bool{
			"sse4.2": x.HasSSE42, "pclmulqdq": x.HasPCLMULQDQ, "aes": x.HasAES, "bmi2": x.HasBMI2,
			"avx2": x.HasAVX2, "avx512f": x.HasAVX512F, "avx512bw": x.HasAVX512BW, "avx512vl": x.HasAVX512VL,
			"avx512vbmi": x.HasAVX512VBMI, "avx512vbmi2": x.HasAVX512VBMI2, "gfni": x.HasAVX512GFNI,
			"vaes": x.HasAVX512VAES, "vpclmulqdq": x.HasAVX512VPCLMULQDQ,
		}
	case "arm64":
		a := cpu.ARM64
		return map[string]bool{
			"neon": a.HasASIMD, "aes": a.HasAES, "pmull": a.HasPMULL, "sha2": a.HasSHA2, "sha3": a.HasSHA3,
			"sha512": a.HasSHA512, "crc32": a.HasCRC32, "sve": a.HasSVE, "sve2": a.HasSVE2,
		}
	}
	return nil
}

// cpuinfoNames maps /proc/cpuinfo flag names to the descriptor's.
var cpuinfoNames = map[string]string{
	"sse4_2": "sse4.2", "pclmulqdq": "pclmulqdq", "aes": "aes", "sha_ni": "sha_ni", "bmi2": "bmi2",
	"avx2": "avx2", "avx512f": "avx512f", "avx512bw": "avx512bw", "avx512vl": "avx512vl",
	"avx512vbmi": "avx512vbmi", "avx512_vbmi2": "avx512vbmi2", "gfni": "gfni", "vaes": "vaes",
	"vpclmulqdq": "vpclmulqdq",
	"asimd":      "neon", "pmull": "pmull", "sha2": "sha2", "sha3": "sha3", "sha512": "sha512",
	"crc32": "crc32", "sve": "sve", "sve2": "sve2",
}

// ParseCPUInfoFlags reads the flag names out of /proc/cpuinfo text (x86
// "flags", arm64 "Features").
func ParseCPUInfoFlags(text string) map[string]bool {
	flags := map[string]bool{}
	scanner := bufio.NewScanner(strings.NewReader(text))
	scanner.Buffer(make([]byte, 1<<16), 1<<20)
	for scanner.Scan() {
		key, value, found := strings.Cut(scanner.Text(), ":")
		if !found {
			continue
		}
		key = strings.TrimSpace(key)
		if key != "flags" && key != "Features" {
			continue
		}
		for _, flag := range strings.Fields(value) {
			if name, ok := cpuinfoNames[flag]; ok {
				flags[name] = true
			}
		}
	}
	return flags
}

// darwinFeatures maps sysctl keys to descriptor flags.
var darwinFeatures = map[string]string{
	"hw.optional.AdvSIMD":         "neon",
	"hw.optional.arm.FEAT_AES":    "aes",
	"hw.optional.arm.FEAT_PMULL":  "pmull",
	"hw.optional.arm.FEAT_SHA256": "sha2",
	"hw.optional.arm.FEAT_SHA3":   "sha3",
	"hw.optional.arm.FEAT_SHA512": "sha512",
	"hw.optional.armv8_crc32":     "crc32",
	"hw.optional.arm.FEAT_SVE":    "sve",
	"hw.optional.arm.FEAT_SVE2":   "sve2",
	"hw.optional.avx2_0":          "avx2",
	"hw.optional.avx512f":         "avx512f",
	"hw.optional.aes":             "aes",
}

func osFlags(ctx context.Context) (map[string]bool, string) {
	switch runtime.GOOS {
	case "linux":
		data, err := os.ReadFile("/proc/cpuinfo")
		if err != nil {
			return nil, ""
		}
		return ParseCPUInfoFlags(string(data)), "/proc/cpuinfo"
	case "darwin":
		flags := map[string]bool{}
		for key, flag := range darwinFeatures {
			if commandLine(ctx, "sysctl", "-n", key) == "1" {
				flags[flag] = true
			}
		}
		return flags, "sysctl hw.optional"
	}
	return nil, ""
}

func kernel(ctx context.Context) string {
	if runtime.GOOS == "windows" {
		if value := commandLine(ctx, "cmd", "/c", "ver"); value != "" {
			return value
		}
		return "not-collected"
	}
	if value := commandLine(ctx, "uname", "-sr"); value != "" {
		return value
	}
	return "not-collected"
}

func cpuModel(ctx context.Context) string {
	switch runtime.GOOS {
	case "darwin":
		if value := commandLine(ctx, "sysctl", "-n", "machdep.cpu.brand_string"); value != "" {
			return value
		}
	case "linux":
		if output, err := exec.CommandContext(ctx, "lscpu").Output(); err == nil {
			if value := field(string(output), "Model name"); value != "" {
				return value
			}
		}
		if data, err := os.ReadFile("/proc/cpuinfo"); err == nil {
			if value := field(string(data), "model name"); value != "" {
				return value
			}
			if part := field(string(data), "CPU part"); part != "" {
				return "arm64 CPU part " + part
			}
		}
	case "windows":
		output, err := exec.CommandContext(ctx, "reg", "query", `HKLM\HARDWARE\DESCRIPTION\System\CentralProcessor\0`, "/v", "ProcessorNameString").Output()
		if err == nil {
			for _, line := range strings.Split(string(output), "\n") {
				if _, value, found := strings.Cut(line, "REG_SZ"); found {
					return strings.TrimSpace(value)
				}
			}
		}
	}
	return "not-collected"
}

// field returns the first "key: value" line's value.
func field(text, key string) string {
	for _, line := range strings.Split(text, "\n") {
		name, value, found := strings.Cut(line, ":")
		if found && strings.TrimSpace(name) == key {
			return strings.TrimSpace(value)
		}
	}
	return ""
}

func memoryBytes(ctx context.Context) uint64 {
	switch runtime.GOOS {
	case "darwin":
		if value, err := strconv.ParseUint(commandLine(ctx, "sysctl", "-n", "hw.memsize"), 10, 64); err == nil {
			return value
		}
	case "linux":
		if data, err := os.ReadFile("/proc/meminfo"); err == nil {
			fields := strings.Fields(field(string(data), "MemTotal"))
			if len(fields) >= 1 {
				if value, err := strconv.ParseUint(fields[0], 10, 64); err == nil {
					return value * 1024
				}
			}
		}
	case "windows":
		return platformMemoryBytes()
	}
	return 0
}

// LoadAverage is the one-minute load average (Linux /proc/loadavg, macOS
// sysctl vm.loadavg), or -1 where the OS has none (Windows).
func LoadAverage() float64 {
	switch runtime.GOOS {
	case "linux":
		data, err := os.ReadFile("/proc/loadavg")
		if err != nil {
			return -1
		}
		return parseFirstFloat(string(data))
	case "darwin":
		output, err := exec.Command("sysctl", "-n", "vm.loadavg").Output()
		if err != nil {
			return -1
		}
		return parseFirstFloat(strings.Trim(strings.TrimSpace(string(output)), "{}"))
	}
	return -1
}

func parseFirstFloat(text string) float64 {
	fields := strings.Fields(text)
	if len(fields) == 0 {
		return -1
	}
	value, err := strconv.ParseFloat(fields[0], 64)
	if err != nil {
		return -1
	}
	return value
}

func commandLine(ctx context.Context, program string, args ...string) string {
	output, err := exec.CommandContext(ctx, program, args...).Output()
	if err != nil {
		return ""
	}
	return strings.TrimSpace(string(bytes.SplitN(output, []byte("\n"), 2)[0]))
}
