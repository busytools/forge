#!/usr/bin/env python3
"""Machine-wide gate lock, held by the kernel for the whole run.

Two `just check` runs on one machine boot CEF hosts into each other, because
nextest only serialises the live suite within its own run
(`.config/nextest.toml`). The lock is a `flock` on a file beside forge's other
machine-local state: the kernel releases it when the holder process dies, so
there is no stale lock to reclaim and no owner to verify. The file carries the
holder's pid so a waiting gate can name it.

A signal the wrapper can handle (HUP, INT, QUIT, TERM) stops the gate's whole
tree before the wrapper exits. SIGKILL cannot be forwarded: the kernel frees
the lock, and that gate's tree is orphaned.

Usage:
  check_lock.py run [--label NAME] -- <command> [args...]

Env:
  FORGE_CHECK_LOCK_DIR      directory holding check.lock (tests)
  FORGE_CHECK_LOCK_TIMEOUT  seconds to wait for a holder (default 3600)
"""

import fcntl
import os
import signal
import subprocess
import sys

DEFAULT_LABEL = "check"
DEFAULT_TIMEOUT = 3600
TEARDOWN_GRACE = 10

SIGNAL_CODES = {
    signal.SIGHUP: 129,
    signal.SIGINT: 130,
    signal.SIGQUIT: 131,
    signal.SIGTERM: 143,
}


def lock_root(label):
    override = os.environ.get("FORGE_CHECK_LOCK_DIR")
    if override:
        return override
    home = os.environ.get("HOME")
    if not home:
        print(f"[ERROR] {label}: no HOME to place the gate lock under", file=sys.stderr)
        sys.exit(2)
    if sys.platform == "darwin":
        return os.path.join(home, "Library", "Application Support", "forge")
    base = os.environ.get("XDG_DATA_HOME") or os.path.join(home, ".local", "share")
    return os.path.join(base, "forge")


def usage(label):
    print(
        f"[ERROR] {label}: usage: check_lock.py run [--label NAME] -- <command> [args...]",
        file=sys.stderr,
    )
    sys.exit(2)


def config_failure(label, detail):
    print(detail, file=sys.stderr)
    print(f"[ERROR] {label}: the gate lock was not acquired", flush=True)
    sys.exit(2)


def timeout_failure(label, timeout, holder):
    print(
        f"[ERROR] {label}: gave up after {timeout}s waiting for the gate lock held by pid"
        f" {holder}; the kernel releases it when that process exits",
        file=sys.stderr,
    )
    print(
        f"[ERROR] {label}: the gate lock was not acquired (held by pid {holder})",
        flush=True,
    )
    sys.exit(1)


def read_pid(fd):
    try:
        os.lseek(fd, 0, os.SEEK_SET)
        raw = os.read(fd, 64)
    except OSError:
        return ""
    text = raw.decode("ascii", "replace").strip()
    return text if text.isdigit() else ""


def write_pid(fd):
    try:
        os.lseek(fd, 0, os.SEEK_SET)
        os.ftruncate(fd, 0)
        os.write(fd, f"{os.getpid()}\n".encode("ascii"))
    except OSError:
        pass


class LockTimeout(Exception):
    pass


def on_alarm(signum, frame):
    raise LockTimeout


state = {"phase": "waiting", "proc": None, "pending": None, "signal": None}


def hard_kill(signum, frame):
    proc = state["proc"]
    if proc is None:
        return
    try:
        os.killpg(proc.pid, signal.SIGKILL)
    except ProcessLookupError:
        pass


def stop(signum, frame):
    code = SIGNAL_CODES.get(signum, 143)
    if state["phase"] == "launching":
        # The signal beat the pid assignment; the child it starts is stopped
        # after the assignment rather than left holding the terminal's pipes.
        state["pending"] = signum
        return
    proc = state["proc"]
    if proc is None:
        # Blocked on the lock: the flock syscall is interrupted and this
        # propagates, so nothing is held to release.
        raise SystemExit(code)
    # Do not wait here: the main thread may be inside Popen.wait(), which
    # holds a non-reentrant lock. The wait returns once the tree is gone, and
    # a tree that ignores TERM is SIGKILLed after the grace rather than
    # holding the machine-wide lock until FORGE_CHECK_LOCK_TIMEOUT.
    state["signal"] = signum
    signal.signal(signal.SIGALRM, hard_kill)
    signal.alarm(TEARDOWN_GRACE)
    try:
        os.killpg(proc.pid, signal.SIGTERM)
    except ProcessLookupError:
        pass


def run(command, label):
    state["phase"] = "waiting"
    state["proc"] = None
    state["pending"] = None
    state["signal"] = None
    for signum in (signal.SIGHUP, signal.SIGINT, signal.SIGQUIT, signal.SIGTERM):
        signal.signal(signum, stop)

    raw_timeout = os.environ.get("FORGE_CHECK_LOCK_TIMEOUT", str(DEFAULT_TIMEOUT))
    if not raw_timeout.isdigit():
        config_failure(
            label,
            f"[ERROR] {label}: FORGE_CHECK_LOCK_TIMEOUT must be a whole number of seconds",
        )
    timeout = int(raw_timeout)

    root = lock_root(label)
    try:
        os.makedirs(root, exist_ok=True)
    except OSError:
        config_failure(label, f"[ERROR] {label}: cannot create {root}")

    path = os.path.join(root, "check.lock")
    try:
        fd = os.open(path, os.O_RDWR | os.O_CREAT, 0o644)
    except OSError:
        config_failure(label, f"[ERROR] {label}: cannot open {path}")

    holder = read_pid(fd)
    try:
        fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except BlockingIOError:
        print(
            f"[..] {label}: waiting for another gate run"
            f" (held by pid {holder or 'unknown'})",
            flush=True,
        )
        if timeout <= 0:
            timeout_failure(label, timeout, read_pid(fd) or "unknown")
        signal.signal(signal.SIGALRM, on_alarm)
        signal.alarm(timeout)
        try:
            fcntl.flock(fd, fcntl.LOCK_EX)
        except (LockTimeout, InterruptedError):
            timeout_failure(label, timeout, read_pid(fd) or "unknown")
        finally:
            signal.alarm(0)

    write_pid(fd)

    state["phase"] = "launching"
    proc = subprocess.Popen(command, start_new_session=True)
    state["proc"] = proc
    state["phase"] = "running"
    if state["pending"] is not None:
        stop(state["pending"], None)

    status = 0
    try:
        status = proc.wait()
    finally:
        signal.alarm(0)
    if state["signal"] is not None:
        sys.exit(SIGNAL_CODES.get(state["signal"], 143))
    sys.exit(status if status >= 0 else 128 - status)


def main():
    args = sys.argv[1:]
    label = DEFAULT_LABEL
    if not args or args[0] != "run":
        usage(label)
    args = args[1:]
    if args[:1] == ["--label"]:
        if len(args) < 2:
            usage(label)
        label = args[1]
        args = args[2:]
    if args[:1] == ["--"]:
        args = args[1:]
    if not args:
        usage(label)
    run(args, label)


if __name__ == "__main__":
    main()
