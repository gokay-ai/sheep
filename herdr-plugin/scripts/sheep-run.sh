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
agent=$(sheep_context_field focused_pane_agent)

case "$verb" in
  snap)
    # A manual snapshot lands on the same timeline the recorder is writing —
    # `sheep_target_line` is what makes that true — so it is worth carrying the
    # same attribution as a recorded turn. Both flags are omitted rather than
    # passed empty when herdr did not say.
    args=()
    if [ -n "$pane" ]; then args+=(--pane "$pane"); fi
    if [ -n "$agent" ]; then args+=(--agent "$agent"); fi
    exec "$bin" --repo "$cwd" --line "$line" snap \
      "${args[@]+"${args[@]}"}" --note "manual snapshot"
    ;;
  doctor)
    exec "$bin" --repo "$cwd" --line "$line" doctor
    ;;
  *)
    echo "sheep: unknown verb '$verb' (expected snap or doctor)" >&2
    exit 2
    ;;
esac
