//go:build linux

package procmeasure

import (
	"errors"
	"fmt"
	"os"
	"os/exec"
	"runtime"
	"syscall"
	"unsafe"

	"golang.org/x/sys/unix"
)

// On Linux the child's rusage ru_maxrss is floored at the harness's own
// peak. Go starts a child with vfork (CLONE_VM): until it execs, the child
// runs on the parent's memory map, and execve records the outgoing map's
// high-water mark into the child's signal->maxrss, which ru_maxrss then
// reports as a lower bound. Every row whose true peak is under the harness's
// own (about 16 MiB late in a run) read as the harness. A plain fork would
// not help: the copied map counts the parent's resident pages too.
//
// So the peak is read from the child itself: VmHWM in /proc/<pid>/status,
// the high-water mark of the map the tool runs on, which the final exec
// created empty. A zombie has already released its map and its status has no
// VmHWM, so it cannot be read after exit; the child is traced instead, with
// only PTRACE_O_TRACEEXIT, and read at its exit stop, when it has finished
// but still holds its map. It is then let go and reaped by exec.Cmd.Wait as
// usual, which still collects the CPU times and the exit status.
//
// Only the thread group leader is traced (no PTRACE_O_TRACECLONE), so the
// tool's other threads never stop, and the leader stops only at its exec and
// its exit, and for a signal sent to it. The tools measured here exit from
// the leader (main returns or calls exit), so its exit stop is the process's.
// Their own children are not covered: none of them spawns any.

const nativeRSSSource = RSSSourceVmHWMExitStop

// NativeRSSSource is where Run and Track read the peak resident set here.
const NativeRSSSource = nativeRSSSource

const (
	cldExited  = 1
	cldKilled  = 2
	cldDumped  = 3
	cldTrapped = 4
	cldStopped = 5
)

// exitTrace is the tracing of one child. ptrace requests must come from the
// thread that traces, so the goroutine holds its OS thread from Start until
// the exit stop is passed.
type exitTrace struct {
	locked bool
}

// beginExitTrace has the command start traced and locks the calling
// goroutine to its thread. Call finish (via await, or abandon when Start
// failed) on the same goroutine.
func beginExitTrace(cmd *exec.Cmd) *exitTrace {
	runtime.LockOSThread()
	if cmd.SysProcAttr == nil {
		cmd.SysProcAttr = &syscall.SysProcAttr{}
	}
	cmd.SysProcAttr.Ptrace = true
	return &exitTrace{locked: true}
}

func (t *exitTrace) abandon() {
	if t.locked {
		t.locked = false
		runtime.UnlockOSThread()
	}
}

// await follows the started child to its exit stop, reads its peak there and
// lets it exit. It never reaps the child: exec.Cmd.Wait does. An error means
// no peak was read, and the run must not report one.
func (t *exitTrace) await(pid int) (int64, error) {
	defer t.abandon()
	peak, gone, err := follow(pid)
	if err != nil && !gone {
		// A child left traced with nobody following it would hang at its
		// next stop: end it, and the run fails on the error.
		_ = syscall.Kill(pid, syscall.SIGKILL)
	}
	return peak, err
}

// follow runs the trace. gone reports that the child has exited or been
// let go from its exit stop, so nothing more can stop it.
func follow(pid int) (peak int64, gone bool, err error) {
	optioned := false
	for {
		code, status, err := peekChild(pid)
		if err != nil {
			return 0, false, err
		}
		switch code {
		case cldExited, cldKilled, cldDumped:
			// Killed without an exit stop (SIGKILL on a timeout may skip it),
			// or an exit the trace never saw: there is no peak to report.
			return 0, true, errors.New("the process exited without an exit stop, so its VmHWM was never read")
		case cldTrapped, cldStopped:
		default:
			return 0, false, fmt.Errorf("waitid reported si_code %d for the traced child", code)
		}
		if _, _, err := consumeStop(pid); err != nil {
			return 0, false, err
		}
		switch {
		case status == int(syscall.SIGTRAP)|unix.PTRACE_EVENT_EXIT<<8:
			peak, readErr := readVmHWM(pid)
			// Let it go whatever the read gave: the run is not to hang on a
			// harness fault.
			if err := unix.PtraceCont(pid, 0); err != nil && readErr == nil {
				readErr = fmt.Errorf("continue from the exit stop: %w", err)
			}
			return peak, true, readErr
		case status == int(syscall.SIGTRAP)|unix.PTRACE_EVENT_EXEC<<8:
			// taskset execing the tool in place.
			if err := unix.PtraceCont(pid, 0); err != nil {
				return 0, false, fmt.Errorf("continue after exec: %w", err)
			}
		case status == int(syscall.SIGTRAP) && !optioned:
			// The stop PTRACE_TRACEME leaves after the first exec.
			if err := unix.PtraceSetOptions(pid, unix.PTRACE_O_TRACEEXIT|unix.PTRACE_O_TRACEEXEC|unix.PTRACE_O_EXITKILL); err != nil {
				return 0, false, fmt.Errorf("PTRACE_SETOPTIONS: %w", err)
			}
			optioned = true
			if err := unix.PtraceCont(pid, 0); err != nil {
				return 0, false, fmt.Errorf("continue after the first exec: %w", err)
			}
		default:
			// A signal for the tool: deliver it as if nothing were tracing.
			if err := unix.PtraceCont(pid, status&0x7f); err != nil {
				return 0, false, fmt.Errorf("deliver signal %d: %w", status&0x7f, err)
			}
		}
	}
}

// sigchldStatus is the offset of si_status in a SIGCHLD siginfo: after
// si_signo, si_errno and si_code, the union is pointer-aligned, and holds
// si_pid, si_uid, then si_status.
var sigchldStatus = (12+unsafe.Sizeof(uintptr(0))-1)&^(unsafe.Sizeof(uintptr(0))-1) + 8

func waitChild(pid, options int) (code, status int, err error) {
	for {
		var info unix.Siginfo
		err = unix.Waitid(unix.P_PID, pid, &info, options, nil)
		if err == unix.EINTR {
			continue
		}
		if err != nil {
			return 0, 0, fmt.Errorf("waitid: %w", err)
		}
		status := *(*int32)(unsafe.Add(unsafe.Pointer(&info), sigchldStatus))
		return int(info.Code), int(status), nil
	}
}

// peekChild reports the child's next stop or its exit without consuming an
// exit, which stays for exec.Cmd.Wait to reap.
func peekChild(pid int) (int, int, error) {
	return waitChild(pid, unix.WSTOPPED|unix.WEXITED|unix.WNOWAIT)
}

// consumeStop takes the stop peekChild saw.
func consumeStop(pid int) (int, int, error) {
	return waitChild(pid, unix.WSTOPPED)
}

func readVmHWM(pid int) (int64, error) {
	data, err := os.ReadFile(fmt.Sprintf("/proc/%d/status", pid))
	if err != nil {
		return 0, err
	}
	peak, err := ParseVmHWM(data)
	if err != nil {
		return 0, fmt.Errorf("/proc/%d/status at the exit stop: %w", pid, err)
	}
	return peak, nil
}

// selfPeak is the harness's own high-water mark, read the same way.
func selfPeak() (int64, error) {
	data, err := os.ReadFile("/proc/self/status")
	if err != nil {
		return 0, err
	}
	return ParseVmHWM(data)
}
