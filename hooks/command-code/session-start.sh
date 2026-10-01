#!/bin/sh
# Command Code SessionStart hook: capture the boundary, then inject a pending
# cross-agent handoff through hookSpecificOutput.additionalContext.
_lib_dir="$(dirname "$0")"
[ -f "$_lib_dir/_lib.sh" ] || _lib_dir="$_lib_dir/.."
. "$_lib_dir/_lib.sh"

SERVER="${SESSIONMUNCH_HOOK_URL:-http://127.0.0.1:49374}"
PAYLOAD=$(cat)
CWD=$(sessionmunch_extract_cwd "$PAYLOAD")
QS=$(sessionmunch_marker_qs "$CWD")
SESSION_ID=$(sessionmunch_extract_session_id "$PAYLOAD")
SESSION_QS=""
[ -n "$SESSION_ID" ] && SESSION_QS="&session_id=$(sessionmunch_url_encode "$SESSION_ID")"

printf '%s' "$PAYLOAD" \
    | sessionmunch_post_hook "$SERVER/hook?event=session-start&agent=command-code${QS}" >/dev/null 2>&1 || true

HANDOFF=$(sessionmunch_get_handoff "$SERVER/handoff?agent=command-code${QS}${SESSION_QS}" 2>/dev/null || true)
if [ -n "$HANDOFF" ]; then
    printf '{"hookSpecificOutput":{"hookEventName":"SessionStart","additionalContext":%s}}\n' \
        "$(printf '%s' "$HANDOFF" | sessionmunch_json_string)"
else
    printf '{}\n'
fi
exit 0
