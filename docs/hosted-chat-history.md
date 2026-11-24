# Hosted chat history

Account-level ChatGPT and Claude conversations are distinct from local
coding-agent journals. An MCP connection alone does not capture hosted chat
history. Use the provider's account export, or an explicitly selected
conversation JSON file in the same format.

- [ChatGPT account data export](https://help.openai.com/en/articles/7260999-how-do-i-export-my-chatgpt-history-and-data)
- [Claude account data export](https://support.claude.com/en/articles/9450526-how-can-i-export-my-claude-data)

Extract the conversation JSON files locally, keep the original archive in
private storage, and do not pass unrelated account/profile/credential files.
Use a stable, non-secret account label on every machine. Select a destination
whose access controls are appropriate for personal history: a workspace name
by itself is not an access-control guarantee.

```bash
sessionmunch import chats --provider chatgpt --account personal \
  --workspace private-history --project hosted --dry-run --json \
  /path/to/conversations.json
```

Use `--provider claude` for Claude conversation JSON. Multiple file paths are
supported. Remove `--dry-run` only after checking counts, reported omissions
and the destination. Files must be extracted JSON (not ZIP), up to 256 MiB
each; larger exports must be split into valid conversation arrays locally.

The importer reads visible user/assistant text on the selected conversation
branch, including timestamps where present. It reports unsupported blocks,
unselected branches and malformed conversations; attachments, hidden reasoning,
system messages and tool internals are not imported. Keep the original exports
for later recovery of unsupported data. Replays deduplicate; edited visible
messages with the same source ID and changed sanitized content become additional
ledger revisions, rather than silently overwriting evidence. Importing the same
conversation into a different scope is rejected; use the explicit session
management path if relocation is intended.

Large visible text uses the existing verified artifact endpoint. The source
file must remain available to retry incomplete uploads. Hosted queues are
separated by destination, credential, account, provider and selected scope.
On credential rotation or destination changes, inspect any outstanding old
queues before retiring them. A successful fixture test does not establish
compatibility with every real export schema.

This is historical/periodic export ingestion, not automatic live capture.
Desktop-local Claude Code/Cowork and compressed Codex desktop journals require
separate native-store coverage. For live hosted capture, prefer an opt-in
provider-supported interface or local browser collector of visible messages
that emits the same identity and event format. Do not collect authentication
cookies, use hidden account APIs or silently upload unrelated conversations.
