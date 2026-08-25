#!/usr/bin/env bash
# Keep exactly one detached `sheep watch` alive for this herdr session.
#
#   watchd.sh start    start the recorder if it is not already running
#   watchd.sh stop     stop it, and sweep up anything a past race left behind
#   watchd.sh status   say whether it is running, and where its logs are
#
# herdr's [[startup]] hook is a one-shot, not a supervisor: it spawns the
# command, captures its output and holds one of 32 in-flight command slots until
# it exits. `sheep watch` never exits, so running it there directly would sit in
# that slot for the whole session and pipe an endless log into herdr's capture
# buffer. `start` therefore forks the recorder off into its own session and
# returns immediately.
#
# `start` is also what the worktree.created / workspace.created hooks call, so
# it has to be a no-op when the recorder is already up.
#
# "Already up" is where this used to be wrong. The pidfile check was a plain
# check-then-act, and herdr 0.8 dispatches hooks CONCURRENTLY — one real
# session's plugin log holds three overlapping `workspace.created` entries about
# ten milliseconds apart. Six of these racing past the same check produced six
# live recorders behind one pidfile; `stop` then reported success with five
# orphans still recording, and `status` said "not running". So `start` takes a
# lock, re-checks under it, verifies the pidfile it wrote, and keeps a ledger of
# every recorder it starts — so `stop` can find the ones the pidfile has stopped
# naming, and only those. A `sheep watch` this script did not start is somebody
# else's and is left alone.

set -eu

script_dir=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
# shellcheck source=herdr-plugin/scripts/common.sh
. "$script_dir/common.sh"

state_dir=$(sheep_state_dir)
run_dir="$state_dir/recorder"
pid_file="$run_dir/watch.pid"
lock_dir="$run_dir/start.lock"
# The ledger of recorders this watchd has started. See `our_recorders`.
pids_file="$run_dir/started.pids"

# Two different logs, and confusing them is how `status` came to advertise a
# file that holds nothing interesting. `sheep watch` opens its own log — the
# turn-by-turn one, the file worth tailing — at <state>/logs/watch.log. The one
# below only catches what the process writes before that logger exists, or
# instead of it: a refusal to start, a panic.
watch_log="$state_dir/logs/watch.log"
spawn_log="$run_dir/spawn.log"

# The pid in the pidfile, if it is alive and still looks like ours.
#
# A pid can be recycled by anything. Only claim it if it still looks like the
# recorder — stopping someone else's process would be a lot worse than starting
# a second watcher.
running_pid() {
  [ -f "$pid_file" ] || return 1
  pid=$(head -n 1 "$pid_file" 2>/dev/null | tr -dc '0-9')
  [ -n "$pid" ] || return 1
  kill -0 "$pid" 2>/dev/null || return 1
  ps -p "$pid" -o args= 2>/dev/null | grep -q '[s]heep.*watch' || return 1
  printf '%s\n' "$pid"
}

# Every recorder THIS watchd started that is still running.
#
# The pidfile can only ever name one process, and the race above produced
# several, so `stop` needs a second source of truth. It must be a narrow one.
# Sweeping the process table for `sheep watch` — which is what this did first —
# reaches across every herdr session on the machine and across a hand-run
# `sheep watch --dry-run` in another terminal, and a `stop` that kills a
# recorder it did not start throws away every turn taken until someone notices.
# That is the loss Sheep exists to prevent; doing it while cleaning up is not a
# trade worth making.
#
# So: a ledger. It cannot be read off the process table instead, because
# `sheep watch` takes its state directory from the environment and never puts
# it in argv, and an `env VAR=… sheep watch` wrapper vanishes from argv the
# moment `env` execs. `start` appends `<pid> <binary>` while holding the lock,
# so concurrent starts cannot interleave a line between them.
#
# A pid on its own would be a trap, because pids are recycled — so an entry
# counts only while the process it names is still running the binary that entry
# was written for. `case` rather than `grep`: a path is not a regular
# expression, and some of them have spaces in.
our_recorders() {
  [ -f "$pids_file" ] || return 0
  while read -r recorded binary; do
    case "$recorded" in '' | *[!0-9]*) continue ;; esac
    [ -n "$binary" ] || continue
    kill -0 "$recorded" 2>/dev/null || continue
    args=$(ps -p "$recorded" -o args= 2>/dev/null) || continue
    case "$args" in
      *"$binary watch"*) printf '%s\n' "$recorded" ;;
    esac
  done <"$pids_file"
}

# Drop ledger entries whose process is gone, so a state directory that lives for
# months does not accumulate a line per start. Anything still running — including
# something that ignored a TERM — is kept, so the next `stop` can still find it.
forget_dead_recorders() {
  [ -f "$pids_file" ] || return 0
  # One line per pid becomes one space-separated word list, so the membership
  # test below can be a plain glob.
  live=$(our_recorders | tr '\n' ' ') || live=""
  kept=""
  while read -r recorded binary; do
    case " $live " in
      *" $recorded "*) kept="$kept$recorded $binary
" ;;
    esac
  done <"$pids_file"
  printf '%s' "$kept" >"$pids_file"
}

# Take the start lock, or give up on starting.
#
# `mkdir` is the lock: one atomic syscall on every filesystem this runs on,
# unlike `[ -e ] && touch`, which is the very shape of bug this exists to stop.
take_lock() {
  attempts=0
  while [ "$attempts" -lt 100 ]; do
    if mkdir "$lock_dir" 2>/dev/null; then
      printf '%s\n' "$$" >"$lock_dir/held-by" 2>/dev/null || true
      return 0
    fi
    attempts=$((attempts + 1))
    # A holder killed between the mkdir and the release would block every later
    # start for the life of the machine, so a lock whose owner is demonstrably
    # gone is cleared once and the loop retried. An empty `held-by` means the
    # holder is mid-write, which is a reason to wait, not to steal.
    holder=$(head -n 1 "$lock_dir/held-by" 2>/dev/null | tr -dc '0-9') || holder=""
    if [ -n "$holder" ] && ! kill -0 "$holder" 2>/dev/null; then
      rm -rf "$lock_dir"
      continue
    fi
    sleep 0.1
  done
  return 1
}

# Release only a lock we still hold. Releasing unconditionally would let a
# late trap delete the lock a *different* start had since taken, which is the
# original bug wearing a hat.
release_lock() {
  trap - EXIT INT TERM HUP
  holder=$(head -n 1 "$lock_dir/held-by" 2>/dev/null | tr -dc '0-9') || holder=""
  if [ "$holder" = "$$" ]; then
    rm -rf "$lock_dir"
  fi
}

start() {
  mkdir -p "$run_dir"

  # Cheap first look, outside the lock. It decides nothing — the check under
  # the lock is the one that means anything — it just keeps the common case
  # (the hooks firing at a healthy recorder) from serialising on a mkdir.
  if pid=$(running_pid); then
    echo "sheep: recorder already running (pid $pid)"
    return 0
  fi

  if ! take_lock; then
    # Somebody else is starting one and is taking their time about it. Two
    # recorders is the failure being avoided here, so losing this race quietly
    # is the correct outcome: the hook that holds the lock will finish.
    echo "sheep: another start is already in flight; leaving it to that one"
    return 0
  fi
  trap release_lock EXIT INT TERM HUP

  if pid=$(running_pid); then
    echo "sheep: recorder already running (pid $pid)"
    release_lock
    return 0
  fi

  bin=$(sheep_binary)

  # setsid detaches from herdr's process group so the recorder survives the hook
  # that started it; nohup covers the platforms that have no setsid (macOS).
  if command -v setsid >/dev/null 2>&1; then
    setsid "$bin" watch >>"$spawn_log" 2>&1 &
  else
    nohup "$bin" watch >>"$spawn_log" 2>&1 &
  fi
  pid=$!
  printf '%s\n' "$pid" >"$pid_file"
  disown "$pid" 2>/dev/null || true

  # Verify after writing rather than trusting the write. A pidfile that does not
  # name the process just started is a pidfile `stop` cannot act on — and a
  # recorder nobody can stop, started by a hook that reported success, is how
  # orphans accumulate in the first place.
  written=$(head -n 1 "$pid_file" 2>/dev/null | tr -dc '0-9') || written=""
  if [ "$written" != "$pid" ] || ! kill -0 "$pid" 2>/dev/null; then
    kill "$pid" 2>/dev/null || true
    release_lock
    echo "sheep: the recorder did not come up cleanly; see $spawn_log" >&2
    return 1
  fi

  # And into the ledger, so `stop` can find this one even if the pidfile stops
  # naming it. Written under the lock, so two starts cannot interleave a line.
  printf '%s %s\n' "$pid" "$bin" >>"$pids_file"
  forget_dead_recorders

  release_lock
  echo "sheep: recorder started (pid $pid), logging to $watch_log"
}

stop() {
  stopped=0
  pid=""
  if pid=$(running_pid); then
    kill "$pid" 2>/dev/null || true
    stopped=$((stopped + 1))
  fi
  rm -f "$pid_file"

  # And the sweep. A recorder this watchd started that the pidfile has stopped
  # naming is an orphan, and a `stop` that reports success while five of them
  # keep recording is worse than no `stop` at all. Only ours: see
  # `our_recorders`.
  orphans=$(our_recorders) || orphans=""
  for orphan in $orphans; do
    if [ "$orphan" != "$pid" ]; then
      kill "$orphan" 2>/dev/null || true
      stopped=$((stopped + 1))
    fi
  done
  forget_dead_recorders

  case "$stopped" in
    0) echo "sheep: recorder is not running" ;;
    1) echo "sheep: recorder stopped${pid:+ (pid $pid)}" ;;
    *) echo "sheep: stopped $stopped recorders — $((stopped - 1)) of them orphaned" ;;
  esac
}

status() {
  alive=$(our_recorders | tr '\n' ' ' | sed 's/ *$//') || alive=""
  code=0
  if pid=$(running_pid); then
    echo "sheep: recorder running (pid $pid)"
  elif [ -n "$alive" ]; then
    # Recorders this watchd started are running but the pidfile does not name
    # any of them. The old `status` said "not running" here, which is exactly
    # the sentence that hid five orphans.
    echo "sheep: recorder running, but not the one $pid_file names (pid(s) $alive)"
    echo "sheep: \`watchd.sh stop\` sweeps them up"
  else
    echo "sheep: recorder is not running"
    code=1
  fi
  echo "sheep: turns   $watch_log"
  echo "sheep: startup $spawn_log"
  return "$code"
}

case "${1:-start}" in
  start) start ;;
  stop) stop ;;
  restart)
    stop
    start
    ;;
  status) status ;;
  *)
    echo "usage: watchd.sh [start|stop|restart|status]" >&2
    exit 2
    ;;
esac
