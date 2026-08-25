#!/usr/bin/env bash
# Shared plumbing for Sheep's herdr hooks. Sourced, never executed.
#
# herdr hands a hook its context two ways: a few HERDR_* variables, and the full
# PluginInvocationContext as JSON in HERDR_PLUGIN_CONTEXT_JSON. The variables are
# the reliable half — they are plain strings — so everything that can come from
# one does, and the JSON is only read for the fields that have no variable
# (chiefly the focused pane's working directory).

# The plugin root: one level up from this file, whatever the caller's cwd is.
sheep_plugin_root() {
  if [ -n "${HERDR_PLUGIN_ROOT:-}" ]; then
    printf '%s\n' "$HERDR_PLUGIN_ROOT"
    return 0
  fi
  CDPATH='' cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd
}

# The binary install.sh put in place. Falls back to a local `cargo build
# --release`, which is what a linked development checkout has instead.
sheep_binary() {
  root=$(sheep_plugin_root)
  for candidate in "$root/bin/sheep" "$root/../target/release/sheep" "$root/../target/debug/sheep"; do
    if [ -x "$candidate" ]; then
      printf '%s\n' "$candidate"
      return 0
    fi
  done
  # Last resort: whatever is on PATH, so a `cargo install`ed sheep still works.
  if command -v sheep >/dev/null 2>&1; then
    command -v sheep
    return 0
  fi
  echo "sheep: no sheep binary found under $root/bin — run herdr-plugin/install.sh" >&2
  return 1
}

# One flat string field out of HERDR_PLUGIN_CONTEXT_JSON. jq when it is there,
# a narrow sed otherwise: every field read here is a top-level string with a
# unique key, so the naive match is safe for them and for nothing else.
sheep_context_field() {
  key=$1
  json=${HERDR_PLUGIN_CONTEXT_JSON:-}
  [ -n "$json" ] || return 0
  if command -v jq >/dev/null 2>&1; then
    printf '%s' "$json" | jq -r --arg k "$key" '.[$k] // empty' 2>/dev/null
    return 0
  fi
  printf '%s' "$json" | sed -n 's/.*"'"$key"'":"\([^"]*\)".*/\1/p' | head -n 1
}

# The worktree the action is about. The focused pane's cwd is the truth — under
# herdr each agent gets its own worktree — with the workspace's cwd as the
# fallback for a workspace-context invocation that has no focused pane.
sheep_target_cwd() {
  cwd=$(sheep_context_field focused_pane_cwd)
  [ -n "$cwd" ] || cwd=$(sheep_context_field workspace_cwd)
  [ -n "$cwd" ] || cwd=$PWD
  printf '%s\n' "$cwd"
}

# The timeline this action or pane is about.
#
# `sheep watch` is the only thing that records agent turns, and it names a
# timeline after the agent herdr attributes to the pane — `claude`, `codex` —
# because that is `--line-by`'s default and nothing here overrides it. Every
# pane and action below has to arrive at the SAME string or the dock reads a
# timeline nothing writes, and "nothing recorded yet" becomes indistinguishable
# from "you are looking in the wrong place". So this asks herdr the same
# question the recorder asks: `focused_pane_agent`, straight out of the
# invocation context.
#
# Not the pane id. `w31:pW` is reassigned on every herdr restart, so a per-pane
# timeline would start empty again after each one — and the recorder does not
# use it. That is also why the fallback is `default` rather than the pane id:
# `default` is the name a standalone `sheep snap` uses, so a pane herdr
# attributes no agent to lands on a timeline that exists in the model instead of
# on one nothing else will ever name.
#
# `tests/plugin_timeline.rs` sources this file and asserts the string it prints
# is the string `WatchArgs`' default produces for the same pane.
sheep_target_line() {
  line=$(sheep_context_field focused_pane_agent)
  [ -n "$line" ] || line=default
  printf '%s\n' "$line"
}

# Sheep's own state lives wherever herdr told the plugin to keep state; the
# recorder's pidfile and log go in a subdirectory of it so they cannot collide
# with the turn logs (`turns/`) or the shadow repositories (`shadow/`, `tmp/`).
sheep_state_dir() {
  if [ -n "${HERDR_PLUGIN_STATE_DIR:-}" ]; then
    printf '%s\n' "$HERDR_PLUGIN_STATE_DIR"
  elif [ -n "${SHEEP_STATE_DIR:-}" ]; then
    printf '%s\n' "$SHEEP_STATE_DIR"
  elif [ -n "${XDG_STATE_HOME:-}" ]; then
    printf '%s\n' "$XDG_STATE_HOME/sheep"
  else
    printf '%s\n' "$HOME/.local/state/sheep"
  fi
}
