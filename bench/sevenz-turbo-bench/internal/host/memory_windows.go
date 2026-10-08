//go:build windows

package host

import (
	"syscall"
	"unsafe"
)

var procGlobalMemoryStatusEx = syscall.NewLazyDLL("kernel32.dll").NewProc("GlobalMemoryStatusEx")

// memoryStatusEx is MEMORYSTATUSEX.
type memoryStatusEx struct {
	Length               uint32
	MemoryLoad           uint32
	TotalPhys            uint64
	AvailPhys            uint64
	TotalPageFile        uint64
	AvailPageFile        uint64
	TotalVirtual         uint64
	AvailVirtual         uint64
	AvailExtendedVirtual uint64
}

// platformMemoryBytes is the installed physical memory GlobalMemoryStatusEx
// reports, or 0 if the call fails.
func platformMemoryBytes() uint64 {
	status := memoryStatusEx{}
	status.Length = uint32(unsafe.Sizeof(status))
	if r, _, _ := procGlobalMemoryStatusEx.Call(uintptr(unsafe.Pointer(&status))); r == 0 {
		return 0
	}
	return status.TotalPhys
}
