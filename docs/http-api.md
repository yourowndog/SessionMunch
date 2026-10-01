# HTTP & Frontend API Reference (`/api/v1`)

SessionMunch exposes a clean, read-only REST API under `/api/v1` for dashboard visualization, external tools, and developer inspection.

---

## Security & Authentication Model

- **Dual-Auth Support:**
  - **Machine Clients:** Send `Authorization: Bearer <token>`. The static `SESSIONMUNCH_AUTH_TOKEN` holds root machine authority; individual database user keys (`aim_...`) provide attributed user authority.
  - **Web Browsers:** Authenticate via `POST /auth/login`. Returns `HttpOnly`, `SameSite=Strict` session cookies plus a CSRF token.
- **Loopback Default:** By default, the server binds to `127.0.0.1:49374`. Non-loopback exposure without authentication fails closed unless explicitly permitted via `--allow-insecure-no-auth`.
- **DNS Rebinding Protection:** The HTTP server verifies the `Host` header against `allowed_hosts` (`localhost`, `127.0.0.1` by default).

---

## Read-Only Endpoint Catalog

All `/api/v1/*` endpoints are strictly read-only. Mutation operations are handled via authenticated `/admin/*` endpoints or MCP tools.

### Workspaces & Projects

#### `GET /api/v1/workspaces`
Lists all workspaces with project counts and page counts.
```json
{
  "workspaces": [
    {
      "workspace_name": "default",
      "project_count": 2,
      "page_count": 45
    }
  ]
}
```

#### `GET /api/v1/workspaces/{workspace}/projects`
Lists projects within a given workspace.

### Pages & Wiki

#### `GET /api/v1/workspaces/{workspace}/projects/{project}/pages`
Lists all markdown pages in the project wiki with metadata, frontmatter, and word counts.

#### `GET /api/v1/workspaces/{workspace}/projects/{project}/pages/{path}`
Fetches the raw markdown, parsed YAML frontmatter, backlinks, and forward links for an individual page.

### Search

#### `GET /api/v1/search?q={query}&workspace={workspace}&project={project}`
Executes an FTS5 full-text query across the project wiki.

#### `POST /api/v1/search`
Executes multi-project or global scoped searches with optional hybrid ranking.

### Sessions & Observations

#### `GET /api/v1/workspaces/{workspace}/projects/{project}/sessions`
Lists recorded agent sessions with start/end timestamps, originating agent harness, and observation counts.

#### `GET /api/v1/workspaces/{workspace}/projects/{project}/sessions/{session_id}`
Retrieves the sanitized observation stream for an individual session.

---

## Web UI Hosting

Starting SessionMunch with `--enable-web` serves the built-in browser UI at `/web`.

To host a custom frontend Single-Page Application (SPA):
```bash
sessionmunch serve --web-ui-dir /path/to/custom-spa/dist
```
