#!/bin/sh
# codex SessionStart hook.
# 1. Forwards the event JSON to the sessionmunch server (fire-and-forget).
# 2. Synchronously fetches any pending cross-agent handoff and prints
#    it to stdout — agent CLIs prepend session-start hook stdout to
#    the next session, so the resuming agent sees prior context with
#    no human in the loop.
#
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
    | sessionmunch_post_hook "$SERVER/hook?event=session-start&agent=codex${QS}" >/dev/null 2>&1 || true
sessionmunch_get_handoff "$SERVER/handoff?agent=codex${QS}${SESSION_QS}" 2>/dev/null || true
exit 0
