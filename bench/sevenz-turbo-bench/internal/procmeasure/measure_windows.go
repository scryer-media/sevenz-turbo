//go:build windows

package procmeasure

import (
	"errors"
	"fmt"
	"os"
	"os/exec"
	"syscall"
	"unsafe"
)

var (
	kernel32                    = syscall.NewLazyDLL("kernel32.dll")
	procGetProcessIoCounters    = kernel32.NewProc("GetProcessIoCounters")
	procK32GetProcessMemoryInfo = kernel32.NewProc("K32GetProcessMemoryInfo")
	procSetProcessAffinityMask  = kernel32.NewProc("SetProcessAffinityMask")
)

const (
	processQueryInformation = 0x0400
	processSetInformation   = 0x0200
	processVMRead           = 0x0010
	// ERROR_VIRUS_INFECTED and ERROR_VIRUS_DELETED: Defender (or another AV
	// filter) refused to start the image.
	errorVirusInfected = syscall.Errno(225)
	errorVirusDeleted  = syscall.Errno(226)
)

type ioCounters struct {
	ReadOperationCount  uint64
	WriteOperationCount uint64
	OtherOperationCount uint64
	ReadTransferCount   uint64
	WriteTransferCount  uint64
	OtherTransferCount  uint64
}

type processMemoryCounters struct {
	CB                         uint32
	PageFaultCount             uint32
	PeakWorkingSetSize         uintptr
	WorkingSetSize             uintptr
	QuotaPeakPagedPoolUsage    uintptr
	QuotaPagedPoolUsage        uintptr
	QuotaPeakNonPagedPoolUsage uintptr
	QuotaNonPagedPoolUsage     uintptr
	PagefileUsage              uintptr
	PeakPagefileUsage          uintptr
}

func pinnedCommand(command Command) (string, []string, string) {
	return command.Path, command.Args, ""
}

// PinSupported reports whether --pin-cpus can take effect on this host.
func PinSupported() bool { return true }

// probe holds a process handle taken right after start. The process object
// stays queryable after exit for as long as the handle is open, which is how
// the I/O and peak-memory counters of an exited child are read.
type probe struct {
	handle syscall.Handle
	pinned string
	// pinErr is why a requested pin could not be applied; the child has
	// been killed, so nothing unconfined is measured as if it were pinned.
	pinErr error
}

func attachProbe(cmd *exec.Cmd, pin string) probe {
	if cmd.Process == nil {
		return probe{}
	}
	// K32GetProcessMemoryInfo needs QUERY_INFORMATION|VM_READ; the affinity
	// call needs SET_INFORMATION, requested only when pinning.
	access := uint32(processQueryInformation | processVMRead)
	pinning := pin != ""
	var mask uintptr
	if pinning {
		var err error
		if mask, err = affinityMask(pin); err != nil {
			return refusePin(cmd, probe{}, err)
		}
		access |= processSetInformation
	}
	handle, err := syscall.OpenProcess(access, false, uint32(cmd.Process.Pid))
	if err != nil {
		if pinning {
			return refusePin(cmd, probe{}, fmt.Errorf("open the process to pin it: %w", err))
		}
		return probe{}
	}
	p := probe{handle: handle}
	if pinning {
		// The child is already running when the mask lands: process start-up
		// takes a few milliseconds before any benchmark work, and Go exposes no
		// suspended-start handle to close that window entirely.
		r, _, callErr := procSetProcessAffinityMask.Call(uintptr(handle), mask)
		if r == 0 {
			return refusePin(cmd, p, fmt.Errorf("SetProcessAffinityMask(%s): %w", pin, callErr))
		}
		p.pinned = pin
	}
	return p
}

func (p probe) finish(measurement *Measurement) {
	if p.handle == 0 {
		return
	}
	defer syscall.CloseHandle(p.handle)
	var counters ioCounters
	if r, _, _ := procGetProcessIoCounters.Call(uintptr(p.handle), uintptr(unsafe.Pointer(&counters))); r != 0 {
		measurement.ReadOps = int64(counters.ReadOperationCount)
		measurement.WriteOps = int64(counters.WriteOperationCount)
		measurement.OtherOps = int64(counters.OtherOperationCount)
		measurement.ReadBytes = int64(counters.ReadTransferCount)
		measurement.WriteBytes = int64(counters.WriteTransferCount)
	}
	var memory processMemoryCounters
	memory.CB = uint32(unsafe.Sizeof(memory))
	if r, _, _ := procK32GetProcessMemoryInfo.Call(uintptr(p.handle), uintptr(unsafe.Pointer(&memory)), uintptr(memory.CB)); r != 0 {
		measurement.MaxRSSBytes = int64(memory.PeakWorkingSetSize)
	}
}

func fillRusage(*Measurement, *os.ProcessState) {}

// configureKill leaves the default kill of the direct child; WaitDelay (set by
// Run) closes the pipes so a surviving grandchild cannot hold Wait open. The
// tools measured here do not spawn children on Windows.
func configureKill(*exec.Cmd) {}

// refusePin kills a child whose requested pin could not be applied, so the
// run fails rather than measuring an unconfined process.
func refusePin(cmd *exec.Cmd, p probe, err error) probe {
	_ = cmd.Process.Kill()
	p.pinErr = err
	return p
}

// affinityMask turns "0-7" (or "3") into a processor mask.
func affinityMask(pin string) (uintptr, error) {
	first, last, err := ParsePinRange(pin)
	if err != nil {
		return 0, err
	}
	if last >= int(8*unsafe.Sizeof(uintptr(0))) {
		return 0, fmt.Errorf("CPU %d is beyond one processor group's affinity mask", last)
	}
	var mask uintptr
	for cpu := first; cpu <= last; cpu++ {
		mask |= 1 << uint(cpu)
	}
	return mask, nil
}

func isQuarantineError(err error) bool {
	var errno syscall.Errno
	if errors.As(err, &errno) {
		return errno == errorVirusInfected || errno == errorVirusDeleted
	}
	return false
}

// NativeRSSSource is where Run and Track read the peak resident set here.
const NativeRSSSource = RSSSourcePeakWorkingSet

const nativeRSSSource = NativeRSSSource
