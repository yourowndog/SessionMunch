#!/bin/sh
# antigravity-cli PreInvocation hook. Forwards the event JSON to the
# sessionmunch server, then injects any pending handoff as an ephemeral
# model-visible message using Antigravity's JSON stdout contract.
_lib_dir="$(dirname "$0")"
[ -f "$_lib_dir/_lib.sh" ] || _lib_dir="$_lib_dir/.."
. "$_lib_dir/_lib.sh"

SERVER="${SESSIONMUNCH_HOOK_URL:-http://127.0.0.1:49374}"
PAYLOAD=$(cat)
if ! sessionmunch_antigravity_is_initial_invocation "$PAYLOAD"; then
    printf '{}\n'
    exit 0
fi
CWD=$(sessionmunch_extract_cwd "$PAYLOAD")
QS=$(sessionmunch_marker_qs "$CWD")
SESSION_ID=$(sessionmunch_extract_session_id "$PAYLOAD")
SESSION_QS=""
if [ -n "$SESSION_ID" ]; then
    SESSION_QS="&session_id=$(sessionmunch_url_encode "$SESSION_ID")"
fi

printf '%s' "$PAYLOAD" \
    | sessionmunch_post_hook "$SERVER/hook?event=session-start&agent=antigravity-cli${QS}" >/dev/null 2>&1 || true
HANDOFF=$(sessionmunch_get_handoff "$SERVER/handoff?agent=antigravity-cli${QS}${SESSION_QS}" 2>/dev/null || true)
if [ -n "$HANDOFF" ]; then
    printf '{"injectSteps":[{"ephemeralMessage":'
    printf '%s' "$HANDOFF" | sessionmunch_json_string
    printf '}]}\n'
else
    printf '{}\n'
fi
exit 0
