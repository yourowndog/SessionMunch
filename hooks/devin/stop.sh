#!/bin/sh
# Devin CLI stop hook.
_lib_dir="$(dirname "$0")"
[ -f "$_lib_dir/_lib.sh" ] || _lib_dir="$_lib_dir/.."
. "$_lib_dir/_lib.sh"

SERVER="${SESSIONMUNCH_HOOK_URL:-http://127.0.0.1:49374}"
PAYLOAD=$(cat)
CWD=$(sessionmunch_resolve_cwd "$PAYLOAD")
QS=$(sessionmunch_marker_qs "$CWD")
SID_QS=$(sessionmunch_session_id_qs devin stop)

printf '%s' "$PAYLOAD" \
    | sessionmunch_post_hook "$SERVER/hook?event=stop&agent=devin${QS}${SID_QS}" >/dev/null 2>&1 || true
printf '{}\n'
exit 0
