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
use nix::unistd::{execvp, fork, ForkResult, Pid};
use std::ffi::CString;
use std::time::Instant;

/// Fork + exec `workload <ticks>`, stopped (SIGSTOP) before it runs any
/// workload code, so it consumes no CPU until the scheduler explicitly
/// resumes it. Returns the child's PID once the child is confirmed stopped.
///
/// Why the child stops itself, before execvp:
///   Stopping it from the parent (kill(child, SIGSTOP) right after fork)
///   races with the child, which is runnable the moment fork returns. If
///   the parent loses the CPU before that signal lands, the child execs
///   and burns part of its burst before any scheduling decision selected
///   it. A child that stops itself cannot reach the workload until the
///   scheduler's SIGCONT, however late the parent gets to run.
///
/// Why the parent must wait for that stop (WUNTRACED):
///   A process can be selected in its arrival tick, microseconds after
///   fork. If that SIGCONT arrived before the child had stopped itself it
///   would do nothing, the child would then stop with nobody left to
///   resume it, and reap_blocking would hang. For the same reason the
///   parent no longer sends its own SIGSTOP: parent-stop, then SIGCONT,
///   would let the child run on into its own stop and stay there.
pub fn spawn_paused(workload_path: &str, ticks: u64) -> Pid {
    // Built before fork so the child has as little to do as possible
    // before stopping itself.
    let prog = CString::new(workload_path).unwrap();
    let args = [prog.clone(), CString::new(ticks.to_string()).unwrap()];

    match unsafe { fork() }.expect("fork failed") {
        ForkResult::Parent { child } => loop {
            match waitpid(child, Some(WaitPidFlag::WUNTRACED)) {
                Ok(WaitStatus::Stopped(_, Signal::SIGSTOP)) => return child,
                Err(Errno::EINTR) => continue,
                other => panic!("child {child} did not stop before exec: {other:?}"),
            }
        },
        ForkResult::Child => {
            // Returns only once the scheduler selects us with SIGCONT.
            kill(Pid::this(), Signal::SIGSTOP).expect("failed to stop self");
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
