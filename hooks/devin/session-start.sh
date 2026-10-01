#!/bin/sh
# Devin CLI SessionStart hook.
# 1. Forwards the event JSON to sessionmunch.
# 2. Synchronously fetches the pending handoff and injects it through
#    hookSpecificOutput.additionalContext, which Devin consumes as
#    additional session context.
_lib_dir="$(dirname "$0")"
[ -f "$_lib_dir/_lib.sh" ] || _lib_dir="$_lib_dir/.."
. "$_lib_dir/_lib.sh"

SERVER="${SESSIONMUNCH_HOOK_URL:-http://127.0.0.1:49374}"
PAYLOAD=$(cat)
CWD=$(sessionmunch_resolve_cwd "$PAYLOAD")
QS=$(sessionmunch_marker_qs "$CWD")
SID_QS=$(sessionmunch_session_id_qs devin session-start)

printf '%s' "$PAYLOAD" \
    | sessionmunch_post_hook "$SERVER/hook?event=session-start&agent=devin${QS}${SID_QS}" >/dev/null 2>&1 || true

HANDOFF=$(sessionmunch_get_handoff "$SERVER/handoff?agent=devin${QS}${SID_QS}" 2>/dev/null || true)
if [ -n "$HANDOFF" ]; then
    printf '{"hookSpecificOutput":{"hookEventName":"SessionStart","additionalContext":%s}}\n' \
        "$(printf '%s' "$HANDOFF" | sessionmunch_json_string)"
else
    printf '{}\n'
fi
exit 0
