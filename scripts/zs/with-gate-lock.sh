#!/usr/bin/env bash
# zs fork: run one heavy local gate at a time across worktrees.
#
# Several agent worktrees on one Mac each run `just desktop-test`,
# `just desktop-tauri-test` and the cargo test suites at the same moment,
# and the contention makes wall-clock and scheduler-sensitive tests flake.
# This wrapper takes an exclusive lock on one file per machine, waits for
# it, runs the command, and releases it. CI never calls it.
#
#   scripts/zs/with-gate-lock.sh just desktop-test
#   ZS_GATE_LOCK_WAIT=3600 scripts/zs/with-gate-lock.sh cargo test -p buzz-agent
set -euo pipefail
if [ "$#" -eq 0 ]; then
  echo "usage: $0 <command> [args...]" >&2
  exit 2
fi
lock_dir="${XDG_CACHE_HOME:-$HOME/.cache}/zs"
mkdir -p "$lock_dir"
export ZS_GATE_LOCK_FILE="${ZS_GATE_LOCK_FILE:-$lock_dir/buzz-gate.lock}"
export ZS_GATE_LOCK_WAIT="${ZS_GATE_LOCK_WAIT:-2700}"
exec python3 - "$@" <<'PY'
import fcntl, os, subprocess, sys, time
path = os.environ["ZS_GATE_LOCK_FILE"]
wait = int(os.environ["ZS_GATE_LOCK_WAIT"])
cmd = sys.argv[1:]
fd = os.open(path, os.O_RDWR | os.O_CREAT, 0o644)
start = time.monotonic()
announced = False
while True:
    try:
        fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        break
    except BlockingIOError:
        if not announced:
            print(f"gate-lock: waiting for {path} (held by another worktree)", file=sys.stderr, flush=True)
            announced = True
        if time.monotonic() - start > wait:
            print(f"gate-lock: gave up after {wait}s waiting for {path}", file=sys.stderr)
            sys.exit(75)
        time.sleep(2)
waited = int(time.monotonic() - start)
if waited:
    print(f"gate-lock: acquired after {waited}s", file=sys.stderr, flush=True)
os.ftruncate(fd, 0)
os.write(fd, f"{os.getpid()} {' '.join(cmd)}\n".encode())
rc = subprocess.call(cmd)
fcntl.flock(fd, fcntl.LOCK_UN)
sys.exit(rc)
PY
