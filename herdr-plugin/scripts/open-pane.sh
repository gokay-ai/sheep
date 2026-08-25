#!/usr/bin/env bash
# Open one of Sheep's declared panes for the agent the action was invoked on.
#
#   open-pane.sh dock      the persistent timeline, split in beside the agent
#   open-pane.sh rewind    the rewind overlay
#
# The manifest cannot express "start in the focused agent's worktree" — a pane
# command is a fixed argv and herdr defaults the pane's cwd to the plugin root —
# so the action goes through `herdr plugin pane open`, which takes --cwd. That is
# also where the timeline name is threaded through: one timeline per agent pane.

set -eu

script_dir=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
# shellcheck source=herdr-plugin/scripts/common.sh
. "$script_dir/common.sh"

entrypoint=${1:-dock}
case "$entrypoint" in
  dock | rewind) ;;
  *)
    echo "sheep: unknown pane '$entrypoint' (expected dock or rewind)" >&2
    exit 2
    ;;
esac

herdr_bin=${HERDR_BIN_PATH:-herdr}
plugin_id=${HERDR_PLUGIN_ID:-sheep}
cwd=$(sheep_target_cwd)
line=$(sheep_target_line)

# The dock is a split and has to say which pane to split off, or herdr falls
# back to whatever happens to be focused — which is not necessarily the pane the
# action was invoked on. The overlay needs no target.
target=()
if [ "$entrypoint" = dock ] && [ -n "${HERDR_PANE_ID:-}" ]; then
  target=(--target-pane "$HERDR_PANE_ID" --direction right)
fi

# SHEEP_LINE is the seam the TUI reads to know which timeline the pane belongs
# to: `sheep ui` resolves its own repository and would otherwise fall back to
# the `default` line, which is the wrong timeline in a multi-agent workspace.
exec "$herdr_bin" plugin pane open \
  --plugin "$plugin_id" \
  --entrypoint "$entrypoint" \
  "${target[@]+"${target[@]}"}" \
  --cwd "$cwd" \
  --env "SHEEP_LINE=$line" \
  --focus
