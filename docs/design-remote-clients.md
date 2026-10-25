# Design: remote clients (public MCP connector and Android)

Status: proposed, 2026-10-25. Not implemented.

SessionMunch works well where it runs a CLI hook or imports a local transcript.
Two kinds of client are out of reach today:

1. **Hosted chat clients.** claude.ai projects, Claude Cowork and Desktop custom
   connectors, the Claude mobile apps, and ChatGPT reach MCP servers from the
   vendor's cloud. They need a public HTTPS URL and an OAuth sign-in. A
   SessionMunch server today is usually LAN- or tailnet-only, and authenticates
   with static bearer tokens or a trusted proxy header.
2. **Android (Termux).** Claude Code and Codex run in Termux, but SessionMunch
   ships no Android binary. The Linux aarch64 release is built against glibc,
   and Termux uses Android's bionic libc, so the hook command cannot run.

Both are product gaps, not deployment gaps: every operator hits them, so the
fix belongs in SessionMunch rather than in one site's proxy config.

## 1. Public MCP connector

### Goals

- Any operator can expose `/mcp` to hosted clients with one config block and
  any HTTPS reverse proxy (see [https-via-proxy.md](https-via-proxy.md)). No
  dependency on a specific tunnel or identity vendor.
- Hosted clients connect through the standard MCP authorization flow, so the
  same server works with claude.ai, Claude Desktop/Cowork custom connectors,
  the Claude mobile apps, ChatGPT, and any compliant client.
- Writes are allowed, but scoped. A hosted chat can read untrusted web pages
  and files, so a prompt injection must not be able to destroy memory.

### Authorization flow

SessionMunch acts as its own OAuth 2.1 authorization server for `/mcp`, per
the MCP authorization spec:

- `/.well-known/oauth-protected-resource` (RFC 9728) and
  `/.well-known/oauth-authorization-server` (RFC 8414) metadata.
- Authorization code with PKCE (S256 only). Dynamic client registration
  (RFC 7591) so hosted clients can register themselves, gated by an operator
  allowlist of redirect hosts (for example `claude.ai`, `chatgpt.com`).
- The consent page signs the operator in with an existing SessionMunch user
  credential, or a one-time code printed by `sessionmunch connector approve`,
  and shows the client name, redirect host, and requested scope.
- Short-lived access tokens with rotating refresh tokens, stored hashed. They
  can be revoked per client with `sessionmunch connector list|revoke`.
- Unauthenticated `/mcp` requests from a public listener get `401` with a
  `WWW-Authenticate` header pointing at the resource metadata. LAN listeners
  keep today's behavior.

An operator who already runs an identity provider can instead use the existing
trusted-proxy mode. OAuth is the default because it works with no extra
infrastructure.

### Access levels

Each grant carries one scope, chosen on the consent page:

| Scope | Allows | Typical client |
|---|---|---|
| `memory:read` | search, recent, read page, read events and observations, status | a shared or experimental chat |
| `memory:append` | read, plus write page (new version, never in-place), write handoff, feedback | your own claude.ai project or Cowork |
| `memory:admin` | everything, including delete, purge, forget sweep, reset, user admin | **never granted to a public connector** |

Rules:

- `memory:append` writes are additive. A page write creates a new version, so
  the old one stays recoverable through `checkpoints` / `restore-page`.
  Deletion is not reachable.
- Every write records the grant (client name, scope, token id) as the actor, so
  `audit-contamination` and page history show which connector wrote what. A
  bad client can be revoked and its writes reviewed as a set.
- Per-grant rate limits reuse the hook ingest limiter.
- `memory:admin` is available only on non-public listeners with a static
  root credential, as today.

This maps onto the existing `Capability` enum: `NormalRead` → `memory:read`,
`NormalWrite` → `memory:append`, and `Admin` / `UserManagement` stay
root-only.

### Configuration sketch

```toml
[connector]
enabled = true
public_url = "https://memory.example.com"   # what hosted clients see
allowed_redirect_hosts = ["claude.ai", "chatgpt.com"]
default_scope = "memory:read"
max_scope = "memory:append"
```

`sessionmunch serve` refuses to start with `enabled = true` unless
`public_url` is HTTPS and some auth mode is configured, so a misconfigured
server never exposes anonymous memory to the internet.

### What this does not solve

Hosted chats still have no lifecycle hooks, so the connector gives them search
and write access, not automatic capture of their own sessions. Capture there
needs a vendor export or hook surface. If Claude Desktop keeps local
transcripts, a Desktop importer is a separate, later feature.

## 2. Android (Termux) client

- Add an `aarch64-linux-android` release artifact, built with the Android NDK
  (`cargo-ndk`) in the release workflow, and run the unit test suite for that
  target in CI with an emulator, or at least `cargo check` on every PR.
- Teach the installer and `install-hooks` about Termux paths (`$PREFIX`, the
  `~/.claude` and `~/.codex` homes under the Termux home) and its lack of
  systemd. Import runs from `termux-job-scheduler` or a hook-triggered drain
  instead of a systemd timer.
- DNS: Termux has no `/etc/resolv.conf`. Use the Android resolver (the NDK
  build gets it from bionic) instead of shipping a static musl binary, which
  would fail name lookups.
- Hooks spool locally exactly as on desktop. A phone is often offline, so the
  7-day spool window and capture-time dating (`captured_ms`) matter most here.

The Claude and ChatGPT Android apps are not Termux programs. They reach
SessionMunch only through the public connector above.

## Rollout order

1. Connector with `memory:read` only, behind `[connector] enabled`.
2. `memory:append` with grant attribution and revocation.
3. Android release artifact and Termux installer support.
4. Desktop transcript importer, if Desktop stores transcripts locally.
