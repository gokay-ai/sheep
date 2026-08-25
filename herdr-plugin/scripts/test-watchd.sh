#!/usr/bin/env bash
# Prove `watchd.sh` keeps exactly one recorder, and can stop every one it made.
#
#   test-watchd.sh
#
# The bug this exists for: herdr 0.8 dispatches hooks concurrently — one real
# session's plugin log holds three overlapping `workspace.created` entries about
# ten milliseconds apart — and both of Sheep's event hooks call `watchd.sh
# start`. When `start` was a plain check-then-act on a pidfile, six concurrent
# ones produced six live recorders behind one pid; `stop` reported success with
# five still running and `status` then said "not running".
#
# A race is not something a reader notices in a diff, so it is asserted here
# instead: eight starts at once, then a count.
#
# The recorder is a stand-in — a shell script that sits still under the argv
# `.../bin/sheep watch`, which is what `running_pid` and `recorder_pids` both
# match on. Nothing here needs a herdr session, a git repository or the real
# binary.

set -eu

here=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
watchd="$here/watchd.sh"

failures=0
ok() { printf 'ok    %s\n' "$1"; }
fail() {
  printf 'FAIL  %s\n' "$1" >&2
  failures=$((failures + 1))
}
check() {
  if [ "$2" = "$3" ]; then ok "$1 -> $3"; else
    fail "$1 -> $3 (expected $2)"
  fi
}

# `stop` sweeps every `sheep watch` this user is running, which is the point of
# it — and would take a real one with it. Refuse rather than interfere with the
# machine this is being run on.
if pgrep -u "$(id -u)" -f '(^|[ /])sheep +watch([ ]|$)' >/dev/null 2>&1; then
  echo "test-watchd.sh: a sheep recorder is already running for this user." >&2
  echo "  This test stops every one it can find, so it will not run beside a real" >&2
  echo "  session. Stop it first, or run this somewhere else." >&2
  exit 2
fi

sandbox=$(mktemp -d)
plugin_root="$sandbox/plugin"
state_dir="$sandbox/state"
mkdir -p "$plugin_root/bin" "$state_dir"

# The stand-in. It must not `exec` anything: the argv is what identifies a
# recorder, so replacing it would make the process invisible to the very
# functions under test.
cat >"$plugin_root/bin/sheep" <<'FAKE'
#!/bin/sh
# A stand-in for `sheep watch`: sits there, like the real one does. The sleep is
# short because a `trap` handler in sh only runs once the current foreground
# command returns, and this test wants to see a TERM land promptly.
trap 'exit 0' TERM INT
while true; do
  sleep 0.2
done
FAKE
chmod +x "$plugin_root/bin/sheep"

live() {
  pgrep -u "$(id -u)" -f '(^|[ /])sheep +watch([ ]|$)' 2>/dev/null | wc -l | tr -d ' '
}

# Wait for the recorder count to settle on an expected value, or give up.
# A TERM is asynchronous — polling for the answer is the honest way to assert
# on it, and a fixed sleep long enough to be safe would make this test slow.
settles_at() {
  want=$1
  waited=0
  while [ "$waited" -lt 40 ]; do
    [ "$(live)" = "$want" ] && return 0
    waited=$((waited + 1))
    sleep 0.1
  done
  return 1
}

check_settles() {
  if settles_at "$2"; then ok "$1 -> $2"; else
    fail "$1 -> $(live) (expected $2)"
  fi
}

# Called by `cleanup`, which the EXIT trap calls. Neither hop is one a static
# checker can follow, hence the directive.
# shellcheck disable=SC2329
sweep() {
  pgrep -u "$(id -u)" -f '(^|[ /])sheep +watch([ ]|$)' 2>/dev/null |
    while read -r stray; do kill "$stray" 2>/dev/null || true; done
  wait 2>/dev/null || true
}

# shellcheck disable=SC2329
cleanup() {
  sweep
  rm -rf "$sandbox"
}
trap cleanup EXIT

watchd() {
  env -u XDG_STATE_HOME \
    HERDR_PLUGIN_ROOT="$plugin_root" \
    HERDR_PLUGIN_STATE_DIR="$state_dir" \
    SHEEP_STATE_DIR="$state_dir" \
    bash "$watchd" "$@"
}

# --- 1. one start, one recorder ----------------------------------------------

echo "== a single start =="
watchd start >"$sandbox/first.log" 2>&1 || fail "start exited non-zero"
check_settles "one start" 1
if grep -q 'recorder started' "$sandbox/first.log"; then
  ok "start says it started one"
else
  fail "start did not report starting a recorder: $(cat "$sandbox/first.log")"
fi

# The message has to name the log the recorder actually writes turns to, not
# the file that only ever catches a refusal.
if grep -q "$state_dir/logs/watch.log" "$sandbox/first.log"; then
  ok "start points at <state>/logs/watch.log"
else
  fail "start advertised the wrong log: $(cat "$sandbox/first.log")"
fi

echo "== starting again is a no-op =="
watchd start >"$sandbox/second.log" 2>&1 || fail "a second start exited non-zero"
check "after a second start" 1 "$(live)"
if grep -q 'already running' "$sandbox/second.log"; then
  ok "the second start says so"
else
  fail "the second start did not recognise the first: $(cat "$sandbox/second.log")"
fi

echo "== status names both logs =="
watchd status >"$sandbox/status.log" 2>&1 || fail "status exited non-zero while running"
if grep -q "$state_dir/logs/watch.log" "$sandbox/status.log"; then
  ok "status names the turn log"
else
  fail "status did not name <state>/logs/watch.log: $(cat "$sandbox/status.log")"
fi

watchd stop >/dev/null 2>&1 || fail "stop exited non-zero"
check_settles "after stop" 0

# --- 2. the race ------------------------------------------------------------

echo "== eight concurrent starts =="
for i in $(seq 1 8); do
  watchd start >"$sandbox/race-$i.log" 2>&1 &
done
wait
check_settles "concurrent starts" 1

pid_file="$state_dir/recorder/watch.pid"
recorded=$(head -n 1 "$pid_file" 2>/dev/null | tr -dc '0-9')
if [ -n "$recorded" ] && kill -0 "$recorded" 2>/dev/null; then
  ok "the pidfile names the survivor ($recorded)"
else
  fail "the pidfile does not name a live recorder (got '${recorded:-}')"
fi

echo "== stop means stopped =="
watchd stop >"$sandbox/stop.log" 2>&1 || fail "stop exited non-zero"
check_settles "after stop" 0
if [ -f "$pid_file" ]; then
  fail "stop left a pidfile behind"
else
  ok "stop removed the pidfile"
fi

# --- 3. the sweep -----------------------------------------------------------

# An orphan is a recorder the pidfile does not know about — what the old race
# left six of. `stop` has to take those too, and `status` must not call the
# machine idle while they run.
echo "== orphans =="
watchd start >/dev/null 2>&1 || fail "start exited non-zero"
sleep 0.3
"$plugin_root/bin/sheep" watch >/dev/null 2>&1 &
orphan=$!
sleep 0.3
check "one recorder plus one orphan" 2 "$(live)"

watchd status >"$sandbox/orphan-status.log" 2>&1 || true
if grep -q 'recorder running' "$sandbox/orphan-status.log"; then
  ok "status still sees a running recorder"
else
  fail "status went quiet with two recorders alive: $(cat "$sandbox/orphan-status.log")"
fi

watchd stop >"$sandbox/orphan-stop.log" 2>&1 || fail "stop exited non-zero"
check_settles "after a sweeping stop" 0
if grep -q 'orphaned' "$sandbox/orphan-stop.log"; then
  ok "stop says it swept an orphan"
else
  fail "stop did not report the orphan: $(cat "$sandbox/orphan-stop.log")"
fi
kill "$orphan" 2>/dev/null || true

# A pidfile naming nothing, with a recorder alive, is the state `status` used to
# describe as "not running".
echo "== a stale pidfile beside a live recorder =="
"$plugin_root/bin/sheep" watch >/dev/null 2>&1 &
stray=$!
mkdir -p "$state_dir/recorder"
printf '999999\n' >"$pid_file"
sleep 0.3
watchd status >"$sandbox/stale-status.log" 2>&1 || true
if grep -q 'not the one' "$sandbox/stale-status.log"; then
  ok "status names the recorder the pidfile does not"
else
  fail "status hid a live recorder: $(cat "$sandbox/stale-status.log")"
fi
watchd stop >/dev/null 2>&1 || true
check_settles "after stop" 0
kill "$stray" 2>/dev/null || true

# --- 4. a lock nobody is holding ---------------------------------------------

# The lock is a directory, so a start killed between taking it and releasing it
# would block every later start forever. A holder that is demonstrably gone has
# to be cleared.
echo "== a stale lock =="
mkdir -p "$state_dir/recorder/start.lock"
printf '999999\n' >"$state_dir/recorder/start.lock/held-by"
watchd start >"$sandbox/stale-lock.log" 2>&1 || fail "start exited non-zero"
check_settles "start past a dead holder's lock" 1
watchd stop >/dev/null 2>&1 || true
sleep 0.3

echo
if [ "$failures" -eq 0 ]; then
  echo "all checks passed"
  exit 0
fi
echo "$failures check(s) failed" >&2
exit 1
