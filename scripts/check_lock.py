#!/usr/bin/env python3
"""Machine-wide gate lock, held by the kernel for the whole run.

Two `just check` runs on one machine boot CEF hosts into each other, because
nextest only serialises the live suite within its own run
(`.config/nextest.toml`). The lock is a `flock` on a file beside forge's other
machine-local state: the kernel releases it when the holder dies, including
SIGKILL, so there is no stale lock to reclaim and no owner to verify. The file
carries the holder's pid so a waiting gate can name it.

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


def lock_root():
    override = os.environ.get("FORGE_CHECK_LOCK_DIR")
    if override:
        return override
    home = os.environ.get("HOME")
    if not home:
        print("[ERROR] check: no HOME to place the gate lock under", file=sys.stderr)
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


def timeout_failure(label, timeout, holder, path):
    print(
        f"[ERROR] {label}: gave up after {timeout}s waiting for the gate lock held by pid"
        f" {holder}; remove {path} if that pid is not a gate run",
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


def stop(signum, frame):
    code = 130 if signum == signal.SIGINT else 143
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
    # holds a non-reentrant lock. The wait returns once the tree is gone.
    state["signal"] = signum
    try:
        os.killpg(proc.pid, signal.SIGTERM)
    except ProcessLookupError:
        pass


def run(command, label):
    state["phase"] = "waiting"
    state["proc"] = None
    state["pending"] = None
    state["signal"] = None
    signal.signal(signal.SIGINT, stop)
    signal.signal(signal.SIGTERM, stop)

    raw_timeout = os.environ.get("FORGE_CHECK_LOCK_TIMEOUT", str(DEFAULT_TIMEOUT))
    if not raw_timeout.isdigit():
        config_failure(
            label,
            f"[ERROR] {label}: FORGE_CHECK_LOCK_TIMEOUT must be a whole number of seconds",
        )
    timeout = int(raw_timeout)

    root = lock_root()
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
            timeout_failure(label, timeout, holder or "unknown", path)
        signal.signal(signal.SIGALRM, on_alarm)
        signal.alarm(timeout)
        try:
            fcntl.flock(fd, fcntl.LOCK_EX)
        except (LockTimeout, InterruptedError):
            timeout_failure(label, timeout, holder or "unknown", path)
        finally:
            signal.alarm(0)

    write_pid(fd)

    state["phase"] = "launching"
    proc = subprocess.Popen(command, start_new_session=True)
    state["proc"] = proc
    state["phase"] = "running"
    if state["pending"] is not None:
        stop(state["pending"], None)

    status = proc.wait()
    if state["signal"] is not None:
        sys.exit(130 if state["signal"] == signal.SIGINT else 143)
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
