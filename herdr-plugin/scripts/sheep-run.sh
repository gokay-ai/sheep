#!/usr/bin/env bash
# Run a Sheep subcommand against the worktree the action was invoked on.
#
#   sheep-run.sh snap      record the focused agent's worktree as a turn
#   sheep-run.sh doctor    report whether that worktree is safe to record
#
# Output goes to herdr's plugin log rather than a terminal:
#
#   herdr plugin log list --plugin sheep
#
# Nothing here writes to the working tree: `snap` only reads it, and `doctor`
# reads nothing but git metadata. The restoring half of Sheep is deliberately not
# an action — it belongs behind the rewind overlay's plan-then-confirm.

set -eu

script_dir=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
# shellcheck source=herdr-plugin/scripts/common.sh
. "$script_dir/common.sh"

verb=${1:-doctor}
bin=$(sheep_binary)
cwd=$(sheep_target_cwd)
line=$(sheep_target_line)
pane=${HERDR_PANE_ID:-}

case "$verb" in
  snap)
    if [ -n "$pane" ]; then
      exec "$bin" --repo "$cwd" --line "$line" snap --pane "$pane" --note "manual snapshot"
    fi
    exec "$bin" --repo "$cwd" --line "$line" snap --note "manual snapshot"
    ;;
  doctor)
    exec "$bin" --repo "$cwd" --line "$line" doctor
    ;;
  *)
    echo "sheep: unknown verb '$verb' (expected snap or doctor)" >&2
    exit 2
    ;;
esac
