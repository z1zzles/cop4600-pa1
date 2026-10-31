// process_control.rs -- provided scaffold.
//
// Thin wrappers around the OS primitives this assignment requires you to
// use directly: fork/exec, SIGSTOP/SIGCONT, waitpid, and a monotonic
// clock. These functions perform the syscalls for you; the scheduling
// *policy* -- when to call them, and in what order -- is the part you're
// responsible for. Using these correctly still requires you to reason
// about races (e.g. a child can exit before you get around to reaping
// it, a stop signal can race with process startup) -- the wrappers don't
// hide that reasoning from you, only the raw libc calls.

use nix::errno::Errno;
use nix::sys::signal::{kill, Signal};
use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
use nix::unistd::{close, execvp, fork, pipe, read, write, ForkResult, Pid};
use std::ffi::CString;
use std::time::Instant;

/// Fork + exec `workload <ticks>`, stopped (SIGSTOP) before it runs any
/// workload code, so it consumes no CPU until the scheduler explicitly
/// resumes it. Returns the child's PID once the child is confirmed stopped.
///
/// Why the child waits on a pipe ("gate") before execvp:
///   The child is runnable the moment fork returns, so the parent's
///   kill(child, SIGSTOP) races with it. If the parent loses the CPU
///   before that signal lands, the child execs and burns part of its
///   burst before any scheduling decision selected it. Instead the child
///   blocks reading the gate, and the parent opens the gate only once
///   the stop has landed, so the child cannot reach the workload until
///   the scheduler's SIGCONT, however late the parent gets to run.
///
/// Why the parent waits for the stop (WUNTRACED) before opening the gate:
///   kill() returns once the signal is queued, not once the child is
///   stopped, so opening the gate right away could let a child that has
///   not stopped yet run straight on into exec. Waiting also guarantees
///   that a process selected in its arrival tick, microseconds after
///   fork, is already stopped when its SIGCONT is sent.
pub fn spawn_paused(workload_path: &str, ticks: u64) -> Pid {
    // Built before fork so the child has as little to do as possible.
    let prog = CString::new(workload_path).unwrap();
    let args = [prog.clone(), CString::new(ticks.to_string()).unwrap()];
    let (gate_r, gate_w) = pipe().expect("failed to create gate pipe");

    match unsafe { fork() }.expect("fork failed") {
        ForkResult::Parent { child } => {
            let _ = close(gate_r);
            kill(child, Signal::SIGSTOP).expect("failed to stop child");
            loop {
                match waitpid(child, Some(WaitPidFlag::WUNTRACED)) {
                    Ok(WaitStatus::Stopped(_, Signal::SIGSTOP)) => break,
                    Err(Errno::EINTR) => continue,
                    other => panic!("child {child} did not stop: {other:?}"),
                }
            }
            // The child is stopped, so this byte only lets it exec once
            // the scheduler resumes it.
            write(gate_w, &[1]).expect("failed to open gate");
            let _ = close(gate_w);
            child
        }
        ForkResult::Child => {
            let _ = close(gate_w);
            let mut byte = [0u8];
            loop {
                match read(gate_r, &mut byte) {
                    Ok(1) => break,
                    Err(Errno::EINTR) => continue,
                    // EOF: the parent died without opening the gate, so
                    // never run the workload unscheduled.
                    _ => unsafe { libc::_exit(1) },
                }
            }
            let _ = close(gate_r);
            execvp(&prog, &args).expect("execvp failed");
            unreachable!("execvp does not return on success");
        }
    }
}

/// Resume a stopped process -- your scheduler "selecting" it.
pub fn resume(pid: Pid) {
    kill(pid, Signal::SIGCONT).expect("failed to resume child");
}

/// Pause a running process -- your scheduler "preempting" it.
pub fn preempt(pid: Pid) {
    kill(pid, Signal::SIGSTOP).expect("failed to stop child");
}

/// Non-blocking check for whether a child has exited. Returns true if it
/// has (and reaps it in the process). You are responsible for calling
/// this, or an equivalent blocking wait, for every child you spawn -- an
/// exited child you never reap is a zombie.
pub fn try_reap(pid: Pid) -> bool {
    matches!(
        waitpid(pid, Some(WaitPidFlag::WNOHANG)),
        Ok(WaitStatus::Exited(_, _))
    )
}

/// Forcibly terminate a child (SIGKILL). Used during cleanup for any
/// process still alive when the run ends -- e.g. one that "did not
/// finish" within runfor. Never leave a real process running or stopped
/// after your scheduler exits.
pub fn terminate(pid: Pid) {
    let _ = kill(pid, Signal::SIGKILL);
}

/// Kill a child AND remove it from the process table. This is what
/// end-of-run cleanup must use: SIGKILL alone turns the child into a
/// zombie, and only a wait by its parent (us) removes that zombie.
///
/// Why the wait must be BLOCKING (no WNOHANG):
///   kill() returns as soon as the signal is queued, not when the child
///   is dead. The child still has to be scheduled and run its exit path
///   before it becomes waitable, so a WNOHANG wait issued right after
///   kill() usually sees "still alive" and reaps nothing.
///
/// Why the blocking wait is guaranteed to return:
///   - SIGKILL cannot be caught, blocked, or ignored, and it is delivered
///     even to a SIGSTOPped process, so the child will die.
///   - waitpid without WUNTRACED/WCONTINUED reports only termination, so
///     it cannot return early just because the child is stopped.
///   - If the child had already exited on its own (it is a zombie),
///     kill() on a zombie still succeeds and waitpid reaps it at once.
///
/// EINTR is retried: if a signal handler ever interrupts the wait, the
/// child has not been reaped yet, so giving up would leak it.
///
/// Returns how the child ended (normally Signaled(pid, SIGKILL, _), or
/// Exited if it finished on its own first), or the error if `pid` is not
/// a live-or-zombie child of ours.
pub fn terminate_and_reap(pid: Pid) -> nix::Result<WaitStatus> {
    kill(pid, Signal::SIGKILL)?;
    loop {
        match waitpid(pid, None) {
            Err(Errno::EINTR) => continue,
            other => return other,
        }
    }
}

/// Blocking reap -- waits for the given child to actually exit. Used
/// during cleanup, and to confirm a selected process's completion before
/// you log it as finished.
pub fn reap_blocking(pid: Pid) {
    loop {
        match waitpid(pid, None) {
            Ok(_) => break,
            Err(nix::errno::Errno::EINTR) => continue,
            Err(_) => break,
        }
    }
}

/// Milliseconds elapsed since a fixed reference point you establish at
/// program start. Use this -- not the input file's tick numbers -- to
/// compute every event time you print in your output.
pub fn now_ms(epoch: &Instant) -> u64 {
    epoch.elapsed().as_millis() as u64
}
