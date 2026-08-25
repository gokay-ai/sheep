#!/usr/bin/env bash
# Keep exactly one detached `sheep watch` alive for this herdr session.
#
#   watchd.sh start    start the recorder if it is not already running
#   watchd.sh stop     stop it
#   watchd.sh status   say whether it is running, and where its log is
#
# herdr's [[startup]] hook is a one-shot, not a supervisor: it spawns the
# command, captures its output and holds one of 32 in-flight command slots until
# it exits. `sheep watch` never exits, so running it there directly would sit in
# that slot for the whole session and pipe an endless log into herdr's capture
# buffer. `start` therefore forks the recorder off into its own session and
# returns immediately.
#
# `start` is also what the worktree.created / workspace.created hooks call, so
# it has to be a no-op when the recorder is already up — that is what the pidfile
# check below is for, and why it verifies the process is *ours* rather than
# trusting a recycled pid.

set -eu

script_dir=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
# shellcheck source=herdr-plugin/scripts/common.sh
. "$script_dir/common.sh"

state_dir=$(sheep_state_dir)
run_dir="$state_dir/recorder"
pid_file="$run_dir/watch.pid"
log_file="$run_dir/watch.log"

running_pid() {
  [ -f "$pid_file" ] || return 1
  pid=$(head -n 1 "$pid_file" 2>/dev/null | tr -dc '0-9')
  [ -n "$pid" ] || return 1
  kill -0 "$pid" 2>/dev/null || return 1
  # A pid can be recycled by anything. Only claim it if it still looks like the
  # recorder — stopping someone else's process would be a lot worse than
  # starting a second watcher.
  ps -p "$pid" -o args= 2>/dev/null | grep -q '[s]heep.*watch' || return 1
  printf '%s\n' "$pid"
}

start() {
  if pid=$(running_pid); then
    echo "sheep: recorder already running (pid $pid)"
    return 0
  fi

  bin=$(sheep_binary)
  mkdir -p "$run_dir"

  # setsid detaches from herdr's process group so the recorder survives the hook
  # that started it; nohup covers the platforms that have no setsid (macOS).
  if command -v setsid >/dev/null 2>&1; then
    setsid "$bin" watch >>"$log_file" 2>&1 &
  else
    nohup "$bin" watch >>"$log_file" 2>&1 &
  fi
  pid=$!
  printf '%s\n' "$pid" >"$pid_file"
  disown "$pid" 2>/dev/null || true

  echo "sheep: recorder started (pid $pid), logging to $log_file"
}

stop() {
  if pid=$(running_pid); then
    kill "$pid" 2>/dev/null || true
    rm -f "$pid_file"
    echo "sheep: recorder stopped (pid $pid)"
  else
    rm -f "$pid_file"
    echo "sheep: recorder is not running"
  fi
}

status() {
  if pid=$(running_pid); then
    echo "sheep: recorder running (pid $pid)"
    echo "sheep: log $log_file"
  else
    echo "sheep: recorder is not running"
    if [ -f "$log_file" ]; then
      echo "sheep: last log $log_file"
    fi
    return 1
  fi
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
