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

use nix::sys::signal::{kill, Signal};
use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
use nix::unistd::{execvp, fork, ForkResult, Pid};
use std::ffi::CString;
use std::time::Instant;

/// Fork + exec `workload <ticks>`, immediately stopped (SIGSTOP) so it
/// consumes no CPU until the scheduler explicitly resumes it.
/// Returns the child's PID.
///
/// Note the inherent race: SIGSTOP is sent from the parent right after
/// fork, but the child may not have reached `execvp` yet. Think through
/// whether that matters for this assignment's correctness requirements,
/// and be ready to explain your reasoning either way.
pub fn spawn_paused(workload_path: &str, ticks: u64) -> Pid {
    match unsafe { fork() }.expect("fork failed") {
        ForkResult::Parent { child } => {
            kill(child, Signal::SIGSTOP).expect("failed to stop child");
            child
        }
        ForkResult::Child => {
            let prog = CString::new(workload_path).unwrap();
            let arg0 = prog.clone();
            let arg1 = CString::new(ticks.to_string()).unwrap();
            execvp(&prog, &[arg0, arg1]).expect("execvp failed");
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
