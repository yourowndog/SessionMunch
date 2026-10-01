#!/bin/sh
# Grok Build CLI user-prompt hook.
_lib_dir="$(dirname "$0")"
[ -f "$_lib_dir/_lib.sh" ] || _lib_dir="$_lib_dir/.."
. "$_lib_dir/_lib.sh"

SERVER="${SESSIONMUNCH_HOOK_URL:-http://127.0.0.1:49374}"
PAYLOAD=$(cat)
CWD=$(sessionmunch_extract_cwd "$PAYLOAD")
QS=$(sessionmunch_marker_qs "$CWD")

printf '%s' "$PAYLOAD" \
    | sessionmunch_post_hook "$SERVER/hook?event=user-prompt&agent=grok${QS}" >/dev/null 2>&1 || true
printf '{}\n'
exit 0
