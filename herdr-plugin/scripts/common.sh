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

# The timeline to record against: one per agent pane, `default` when there is no
# pane to name it after. HERDR_PANE_ID is set for action and event commands
# whenever a pane is focused.
#
# The pane id goes through verbatim on purpose, even though `sheep snap` cannot
# take it yet: herdr pane ids look like `w31:pW`, and `shadow::ref_name` builds
# `refs/sheep/<line>` from the raw string, which git rejects for the colon
# ("refusing to update ref with bad name"). `store::sanitize` already maps
# non-alphanumerics to `-` for the turn log's filename; the shadow ref needs the
# same treatment. Sanitizing here instead would only move the bug — the recorder
# passes the same ids and would then disagree with this script about which
# timeline a pane owns.
sheep_target_line() {
  line=${HERDR_PANE_ID:-}
  [ -n "$line" ] || line=$(sheep_context_field focused_pane_id)
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
