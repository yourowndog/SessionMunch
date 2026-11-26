# Visible-turn collector (prototype)

**Status:** prototype, tested only against a synthetic fixture (MUNCH-022).
It ships with **no** ChatGPT or Claude profile and has **not** been pointed at
any real or authenticated page.

The collector reads finalized visible chat turns from a page the owner has
enrolled and emits `sessionmunch.observed-visible-turn.v1` NDJSON. That output
goes into `sessionmunch import chats --format observed`, which uses the same
session and event identity as account exports, so live capture, restarts and
later exports deduplicate.

## Pieces

| File | Role |
|---|---|
| `collector.js` | In-page collector. A surface profile selects message elements. It reads rendered visible text (`innerText`), which skips hidden DOM. Records stay in memory until the runner drains them. The collector sends nothing itself. |
| `profiles/fixture.json` | Profile for the synthetic fixture only. |
| `run.mjs` | Local runner. Drives headless Chromium over `--remote-debugging-pipe` (no TCP listener). Each run gets a fresh temporary profile that is deleted afterwards, a refusing proxy and a NOTFOUND resolver, and no display variables are passed through. Uses Node built-ins only. |
| `test/` | Synthetic fixture page and `node --test` suite. |

## Fail-closed rules (same as the importer)

A message is emitted only when all of these hold:

- The page URL contains a native conversation UUID.
- The element carries a native message UUID.
- The role is `user` or `assistant`.
- The message is not streaming.
- The visible text changed since the last emission for that message ID.

Everything else is counted, never guessed. The collector never derives an
identity from position, title, URL shape, time or text.

| Page event | What the collector emits | What the importer does with it |
|---|---|---|
| Edit (same ID, new text) | A new record | A new revision |
| Regeneration (new native ID) | A new message | A new message |
| Same text under two different IDs | Two messages | Two messages |
| Partial or streaming turn | Nothing | Nothing (never reaches it) |
| Tool block or hidden reasoning | Nothing | Nothing (never reaches it) |

## Running the tests

```bash
CHROME_HEADLESS_SHELL=/path/to/chrome-headless-shell \
SESSIONMUNCH_BIN=/path/to/sessionmunch \
node --test test/
```

`SESSIONMUNCH_BIN` is optional. When set, the emitted NDJSON is checked with
`import chats --format observed --dry-run` under an isolated `HOME` and data
directory.

## Requirements a real surface must meet

These are checks, not assumptions. Each surface (ChatGPT web, Claude web, and
any desktop app) needs an owner decision and an owner-selected harmless
thread before a profile for it is written.

1. **Native conversation ID** must be in the page URL as a UUID that matches
   the account export's conversation ID for the same thread.
2. **Native message ID** must be present on each rendered message, and it
   must equal the export's message ID for the same message. If it is absent,
   or it differs from the export (for example a DOM-local ID), the surface
   fails closed: nothing is captured for it.
3. **A finality signal** must reliably mark streaming, interrupted and
   stopped turns. Without one, only post-completion rescans are safe; that
   needs evidence before use.
4. **Visible text boundary:** the profile's text selector must cover
   rendered message text only. Tool calls, artifacts, citations, attachments
   and hidden reasoning stay out unless separately approved.
5. **Edit and regeneration behaviour** must be observed on a harmless
   thread. Does an edit keep the message ID? Does regeneration create a new
   one? Is the old branch still rendered?
6. **Account label:** use the same stable non-secret label as exports. A
   collector must never read account identity from cookies or storage.
7. **Supported access only.** No private APIs, response interception,
   cookie or storage reads, credential handling, or automated sending.
   Enrollment is per thread or per tab with explicit owner action, and stops
   on logout or revocation.
8. **Desktop apps** (Electron) offer no supported injection point.
   Desktop-originated threads are covered only when viewed on the web or
   through an account export. The collector makes no desktop claim.
9. **Transport:** records leave the page only through the local runner (or a
   future reviewed browser extension) into the local import. There is no
   direct network send from the page.
10. **Acceptance evidence** for each surface:
    - exact app or site version;
    - an ID equality check against an export of the same thread;
    - replay, edit, regeneration and restart checks;
    - a privacy check for hidden reasoning and tool content;
    - the coverage limits written down.

Until a surface passes all of these, use account export (historical only) for
it.
