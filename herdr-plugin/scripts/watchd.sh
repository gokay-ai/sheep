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
# lock, re-checks under it, verifies the pidfile it wrote, and `stop` sweeps by
# pattern rather than trusting that one pid is the whole story.

set -eu

script_dir=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
# shellcheck source=herdr-plugin/scripts/common.sh
. "$script_dir/common.sh"

state_dir=$(sheep_state_dir)
run_dir="$state_dir/recorder"
pid_file="$run_dir/watch.pid"
lock_dir="$run_dir/start.lock"

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

# What a running recorder's argv looks like, as one extended regular expression
# both `pgrep -f` and `grep -E` read the same way.
RECORDER_ARGV='(^|[ /])sheep +watch([ ]|$)'

# Every `sheep watch` this user has running, whatever the pidfile believes.
#
# The pidfile can only ever name one process, and the race above produced
# several. This is what lets `stop` mean "stopped" and `status` stop lying.
# Scoped to this user's processes: a sweep that reached another account's would
# be a worse bug than the one it fixes.
recorder_pids() {
  if command -v pgrep >/dev/null 2>&1; then
    pgrep -u "$(id -u)" -f "$RECORDER_ARGV" 2>/dev/null || true
    return 0
  fi
  # No pgrep (a stripped container, mostly). shellcheck would rather this were
  # pgrep too — it is, above; this is the fallback for machines that have not
  # got it, and `ps -o pid=,args=` is what `running_pid` already reads.
  # shellcheck disable=SC2009
  ps -o pid=,args= -u "$(id -u)" 2>/dev/null |
    grep -E "$RECORDER_ARGV" |
    awk '{ print $1 }'
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

  # And the sweep. Anything still answering to `sheep watch` once the pidfile's
  # own process is gone is an orphan, and a `stop` that reports success while
  # five of them keep recording is worse than no `stop` at all.
  orphans=$(recorder_pids) || orphans=""
  for orphan in $orphans; do
    if [ "$orphan" != "$pid" ]; then
      kill "$orphan" 2>/dev/null || true
      stopped=$((stopped + 1))
    fi
  done

  case "$stopped" in
    0) echo "sheep: recorder is not running" ;;
    1) echo "sheep: recorder stopped${pid:+ (pid $pid)}" ;;
    *) echo "sheep: stopped $stopped recorders — $((stopped - 1)) of them orphaned" ;;
  esac
}

status() {
  alive=$(recorder_pids | tr '\n' ' ' | sed 's/ *$//') || alive=""
  code=0
  if pid=$(running_pid); then
    echo "sheep: recorder running (pid $pid)"
  elif [ -n "$alive" ]; then
    # Recorders are running but the pidfile does not name any of them. The old
    # `status` said "not running" here, which is exactly the sentence that hid
    # five orphans.
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
