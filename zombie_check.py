#!/usr/bin/env python3
"""
zombie_check.py -- detect processes the scheduler leaves behind.

Why this exists: when the scheduler exits, any child it never reaped is
re-parented to the nearest "subreaper" (normally PID 1). If PID 1 reaps
orphans (systemd, tini, docker --init, bash), the zombie vanishes almost
instantly and `ps aux | grep defunct` shows nothing -- even though the bug
is real. If PID 1 does NOT reap (e.g. `sleep infinity` as the container
command), zombies pile up forever.

This script makes itself a subreaper (prctl PR_SET_CHILD_SUBREAPER), so
anything the scheduler leaves behind is re-parented HERE and stays
visible, regardless of what PID 1 is. It then lists every leftover
process, reaps them so the next run starts clean, and exits non-zero if
anything was left.

Usage:
    python3 zombie_check.py <scheduler-binary> <input-file> [runs]
"""
import ctypes
import os
import subprocess
import sys
import time

PR_SET_CHILD_SUBREAPER = 36


def become_subreaper():
    libc = ctypes.CDLL(None, use_errno=True)
    if libc.prctl(PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) != 0:
        err = ctypes.get_errno()
        sys.exit(f"prctl(PR_SET_CHILD_SUBREAPER) failed: {os.strerror(err)}")


def my_children():
    """Return [(pid, state, cmdline)] for every process whose parent is us."""
    me = os.getpid()
    found = []
    for entry in os.listdir("/proc"):
        if not entry.isdigit():
            continue
        try:
            with open(f"/proc/{entry}/stat") as f:
                stat = f.read()
        except OSError:
            continue  # process disappeared while scanning
        # Format: pid (comm) state ppid ...  -- comm may contain spaces,
        # so split on the LAST ')'.
        rparen = stat.rfind(")")
        comm = stat[stat.find("(") + 1 : rparen]
        fields = stat[rparen + 2 :].split()
        state, ppid = fields[0], int(fields[1])
        if ppid == me:
            found.append((int(entry), state, comm))
    return found


def reap_leftovers():
    """Collect any zombies we adopted, so each run is counted independently."""
    while True:
        try:
            pid, _ = os.waitpid(-1, os.WNOHANG)
        except ChildProcessError:
            return
        if pid == 0:
            return


def kill_and_reap(pids):
    for pid in pids:
        try:
            os.kill(pid, 9)
        except ProcessLookupError:
            pass
    for pid in pids:
        try:
            os.waitpid(pid, 0)
        except ChildProcessError:
            pass


def one_run(binary, infile):
    proc = subprocess.run([binary, infile], timeout=120)
    # Give a just-SIGKILLed child time to finish dying, so a racy fix that
    # "almost" works still gets caught as a leftover.
    time.sleep(0.3)
    left = my_children()
    if left:
        kill_and_reap([pid for pid, state, _ in left if state != "Z"])
    reap_leftovers()
    return proc.returncode, left


def main():
    if len(sys.argv) < 3:
        sys.exit(__doc__)
    binary, infile = sys.argv[1], sys.argv[2]
    runs = int(sys.argv[3]) if len(sys.argv) > 3 else 1

    become_subreaper()

    bad_runs = 0
    for i in range(1, runs + 1):
        rc, left = one_run(binary, infile)
        if left:
            bad_runs += 1
            desc = ", ".join(f"pid {p} state {s} ({c})" for p, s, c in left)
            print(f"run {i}: exit={rc}  LEFT BEHIND: {desc}")
        else:
            print(f"run {i}: exit={rc}  clean")

    print(f"\n{bad_runs}/{runs} runs left processes behind")
    sys.exit(1 if bad_runs else 0)


if __name__ == "__main__":
    main()
