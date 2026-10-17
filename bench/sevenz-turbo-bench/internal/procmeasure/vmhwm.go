package procmeasure

import (
	"bufio"
	"bytes"
	"errors"
	"fmt"
	"strconv"
	"strings"
)

// ParseVmHWM reads the VmHWM line of a Linux /proc/<pid>/status, the peak
// resident set of the process's memory map, in bytes. A status without the
// line (a zombie, or a kernel thread, has no map) or with one it cannot read
// is an error: the caller then has no peak, never a guessed one.
func ParseVmHWM(status []byte) (int64, error) {
	scanner := bufio.NewScanner(bytes.NewReader(status))
	for scanner.Scan() {
		value, found := strings.CutPrefix(scanner.Text(), "VmHWM:")
		if !found {
			continue
		}
		fields := strings.Fields(value)
		if len(fields) != 2 || fields[1] != "kB" {
			return 0, fmt.Errorf("VmHWM line %q: want a count of kB", strings.TrimSpace(value))
		}
		kib, err := strconv.ParseInt(fields[0], 10, 64)
		if err != nil || kib <= 0 {
			return 0, fmt.Errorf("VmHWM line %q: want a positive count of kB", strings.TrimSpace(value))
		}
		return kib << 10, nil
	}
	if err := scanner.Err(); err != nil {
		return 0, err
	}
	return 0, errors.New("no VmHWM line: the process has no memory map (it has already exited)")
}
