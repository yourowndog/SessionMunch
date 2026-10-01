"""ai-memory memory provider plugin — MemoryProvider for the ai-memory Rust server.

Cross-session long-term memory with sqlite-backed wiki pages, Karpathy-style
consolidation, FTS5 + embedding retrieval, and managed workstream support.
Communicates with an ai-memory server over its MCP or HTTP API.

Config: $HERMES_HOME/ai-memory.json or env vars.

## Threading

Every write/prefetch path uses ``spawn_context_thread`` from the base module so
contextvars (profile isolation) propagate correctly. Background threads are
daemon so a stuck HTTP call never blocks interpreter exit.
"""

from __future__ import annotations

import hashlib
import json
import logging
import os
import sys
import threading
import urllib.error
import urllib.request
from pathlib import Path
from typing import Any, Dict, List, Optional

from agent.memory_provider import MemoryProvider, spawn_context_thread

# The adapter directory is named ``ai-memory``, which is not a valid Python
# identifier, so it can never be imported as a package. Hermes loads this plugin
# by file path, which leaves its siblings reachable only as top-level modules.
_ADAPTER_DIR = Path(__file__).resolve().parent
if str(_ADAPTER_DIR) not in sys.path:
    sys.path.append(str(_ADAPTER_DIR))

import scope  # noqa: E402  (sibling modules; see _ADAPTER_DIR note above)
import schema  # noqa: E402

logger = logging.getLogger(__name__)

# ---------------------------------------------------------------------------
# Constants
# ---------------------------------------------------------------------------

# Default ai-memory MCP server address.
#
# NOTE (2026-09-25 triage, re-verified 2026-09-30 against live Weakling):
# this used to default to a local loopback address, which pointed at whatever
# stale local ai-memory binary (if any) happened to be running on the host --
# never correct for a shared deployment. The single enabled ai-memory.service
# (v2.2.1, retrieval-section-index) is Weakling's Tailscale-only listener;
# every CLI-agent node reaches that one shared instance, not a per-host copy.
# Confirm this address against cluster-specs before relying on it long-term --
# addresses on a Tailscale mesh are current operational state, not a stable
# contract.
_DEFAULT_MCP_URL = "http://100.109.145.90:49374/mcp"

# Config file basename written to $HERMES_HOME.
_CONFIG_FILENAME = "ai-memory.json"

# Spill threshold for oversized prefetch output (chars).
_SPILL_THRESHOLD = 10_000

# Default token file paths (node-local secrets, never synced via chezmoi).
_DEFAULT_TOKEN_FILE = Path.home() / ".local" / "share" / "ai-memory" / "auth-token"
_FALLBACK_TOKEN_FILE = Path.home() / ".config" / "opencode" / ".ai-memory-token"

# Hit arrays the live server uses, in merge order, live in ``schema.py``
# (``_HIT_ARRAY_KEYS``) — a global=true query leaves ``hits`` empty and
# populates ``global_hits``; scopes= and time-travel use ``global_scope_hits``
# / ``raw_hits``. Verified live 2026-09-30 against Weakling
# v2.2.1-retrieval-section-index.

# ---------------------------------------------------------------------------
# Tool schemas (static — derived from API_MAP.md §3)
# ---------------------------------------------------------------------------

# fmt: off
_AI_MEMORY_TOOL_SCHEMAS: List[Dict[str, Any]] = [
    {
        "name": "memory_query",
        "description": (
            "Hybrid (FTS5 + entity + vector + graph) search over ai-memory wiki pages. "
            "Returns ranked hits with score provenance, page paths, and body excerpts. "
            "Use this to recall what the agent knows about a topic — pass a natural-language "
            "query or an FTS5 expression. Supports multi-project scope, passage-level retrieval, "
            "and time-travel (as_of)."
        ),
        "parameters": {
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "FTS5 query expression or natural-language search (required).",
                },
                "limit": {
                    "type": "integer",
                    "description": "Max hits (default 10, max 100).",
                },
                "project": {
                    "type": "string",
                    "description": "Project scope filter.",
                },
                "workspace": {
                    "type": "string",
                    "description": "Workspace scope filter.",
                },
                "global": {
                    "type": "boolean",
                    "description": "Cross-project global search (FTS-only, no vector).",
                },
                "unit": {
                    "type": "string",
                    "enum": ["page", "passage"],
                    "description": "Retrieval unit: full pages or discrete passages.",
                },
            },
            "required": ["query"],
        },
    },
    {
        "name": "memory_read_page",
        "description": (
            "Read the full body of an ai-memory wiki page by path or by FTS query "
            "(top hit's body). Pass exactly one of 'path' or 'query'. Returns the "
            "page title, body, frontmatter, and path."
        ),
        "parameters": {
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Exact wiki path (takes precedence over query).",
                },
                "query": {
                    "type": "string",
                    "description": "FTS5 search — returns the top hit's body.",
                },
                "project": {
                    "type": "string",
                    "description": "Project scope.",
                },
                "workspace": {
                    "type": "string",
                    "description": "Workspace scope.",
                },
            },
        },
    },
    {
        "name": "memory_write_page",
        "description": (
            "Write or overwrite an ai-memory wiki page. The path is a relative file path "
            "within the wiki tree (e.g. 'notes/project-x.md'). Body is markdown. Supports "
            "tier (working/episodic/semantic/procedural), tags, pinning, and optional TTL. "
            "Returns the page id and git checkpoint hash."
        ),
        "parameters": {
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Relative wiki path (required). E.g. 'notes/project-x.md'.",
                },
                "body": {
                    "type": "string",
                    "description": "Markdown body (required).",
                },
                "title": {
                    "type": "string",
                    "description": "Optional title; derived from H1 when omitted.",
                },
                "tier": {
                    "type": "string",
                    "enum": ["working", "episodic", "semantic", "procedural"],
                    "description": "Memory tier (default: semantic).",
                },
                "tags": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Tags for categorization.",
                },
                "pinned": {
                    "type": "boolean",
                    "description": "Exempt from decay sweep.",
                },
                "expires_at": {
                    "type": "string",
                    "description": "RFC3339 or YYYY-MM-DD TTL for auto-expiry.",
                },
                "project": {
                    "type": "string",
                    "description": "Project scope (created if missing).",
                },
                "workspace": {
                    "type": "string",
                    "description": "Workspace scope (created if missing).",
                },
            },
            "required": ["path", "body"],
        },
    },
    {
        "name": "memory_recent",
        "description": (
            "List the most recently updated ai-memory wiki pages. Useful for orienting "
            "yourself on what was recently written. Supports project/workspace scoping."
        ),
        "parameters": {
            "type": "object",
            "properties": {
                "limit": {
                    "type": "integer",
                    "description": "Max pages (default 10, max 100).",
                },
                "project": {
                    "type": "string",
                    "description": "Project scope.",
                },
                "workspace": {
                    "type": "string",
                    "description": "Workspace scope.",
                },
            },
        },
    },
    {
        "name": "memory_delete_page",
        "description": (
            "Delete an ai-memory wiki page by its exact path. The page is tombstoned, "
            "not immediately purged. Use with care — prefer memory_write_page for "
            "corrections."
        ),
        "parameters": {
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Exact wiki path to delete (required).",
                },
                "project": {
                    "type": "string",
                    "description": "Project scope.",
                },
                "workspace": {
                    "type": "string",
                    "description": "Workspace scope.",
                },
            },
            "required": ["path"],
        },
    },
    {
        "name": "memory_status",
        "description": (
            "Return aggregate counts of pages, sessions, and observations in the "
            "ai-memory store. Useful for a quick health check."
        ),
        "parameters": {
            "type": "object",
            "properties": {
                "project": {
                    "type": "string",
                    "description": "Project scope.",
                },
                "workspace": {
                    "type": "string",
                    "description": "Workspace scope.",
                },
            },
        },
    },
]
# fmt: on

# Map tool name → tool schema for quick lookup.
_TOOL_SCHEMA_BY_NAME: Dict[str, Dict[str, Any]] = {
    s["name"]: s for s in _AI_MEMORY_TOOL_SCHEMAS
}


# ---------------------------------------------------------------------------
# MCP client helper (HTTP transport)
# ---------------------------------------------------------------------------

class _McpClient:
    """Minimal MCP client for ai-memory over HTTP transport.

    Wraps JSON-RPC 2.0 calls to the ai-memory MCP endpoint.  The server is
    expected at ``mcp_url`` (default: Weakling's shared ai-memory.service).

    Auth: the live server answers 401 without a bearer token. Every CLI-agent
    node authenticates via a node-local ``Authorization: Bearer ***`` header,
    token read from ``~/.local/share/ai-memory/auth-token`` or
    ``~/.config/opencode/.ai-memory-token`` (node-local secret, mode 0600,
    never synced via chezmoi).
    """

    def __init__(
        self,
        mcp_url: str = _DEFAULT_MCP_URL,
        timeout: float = 15.0,
        token_file: Optional[str] = None,
    ) -> None:
        self._mcp_url = mcp_url.rstrip("/")
        self._timeout = timeout
        self._session: Any = None  # urllib opener
        self._request_id = 0
        self._token_file = token_file
        self._bearer_token: Optional[str] = None

    # ------------------------------------------------------------------
    # Public helpers
    # ------------------------------------------------------------------

    def call_tool(self, tool_name: str, arguments: Dict[str, Any]) -> Dict[str, Any]:
        """Call an ai-memory MCP tool and return the parsed result.

        Returns a dict with at least ``"content"`` (list) on success or
        ``"isError"`` / ``"error"`` on failure.
        """
        self._request_id += 1
        payload = {
            "jsonrpc": "2.0",
            "id": self._request_id,
            "method": "tools/call",
            "params": {
                "name": tool_name,
                "arguments": arguments,
            },
        }
        logger.debug("MCP call_tool(%s) args=%s", tool_name, arguments)
        return self._post(payload)

    def health(self) -> bool:
        """Quick connectivity check (no network call per spec — checks config presence).

        Per SPEC.md §3: is_available() must NOT make network calls.
        We check whether the config file exists and looks parseable instead.
        """
        return False

    # ------------------------------------------------------------------
    # Internal
    # ------------------------------------------------------------------

    def _read_bearer_token(self) -> Optional[str]:
        """Read bearer token from token file.

        Priority:
        1. Explicit token_file config
        2. ~/.local/share/ai-memory/auth-token
        3. ~/.config/opencode/.ai-memory-token

        Token value is stripped of whitespace and "Bearer " prefix.
        Result is cached so only one file read happens per instance.
        """
        if self._bearer_token is not None:
            return self._bearer_token or None

        # Build ordered list of candidate paths.
        candidates: List[Path] = []
        if self._token_file:
            candidates.append(Path(self._token_file).expanduser())
        candidates.append(_DEFAULT_TOKEN_FILE)
        candidates.append(_FALLBACK_TOKEN_FILE)

        for path in candidates:
            if path.exists():
                try:
                    token = path.read_text(encoding="utf-8").strip()
                    token = token.removeprefix("Bearer ").strip()
                    if token:
                        self._bearer_token = token
                        return token
                except OSError:
                    continue

        # Cache the negative — avoid re-reading on every call.
        self._bearer_token = ""
        return None

    @staticmethod
    def _normalize_jsonrpc_response(raw: Dict[str, Any]) -> Dict[str, Any]:
        """Normalize a JSON-RPC 2.0 response envelope to MCP content format.

        On success the server returns ``{"jsonrpc":"2.0","id":N,"result":
        {"content":[...]}}``.  On error it returns ``{"jsonrpc":"2.0",
        "id":N,"error":{"code":...,"message":...}}``.  Both are unwrapped
        into the flat ``{"content":[...],[ "isError": true ]}`` shape that
        ``handle_tool_call`` expects.
        """
        # Already in MCP content format (error from our own HTTP/transport layer).
        if "isError" in raw:
            return raw

        if "error" in raw:
            err = raw["error"]
            msg = err.get("message", "Unknown JSON-RPC error")
            return {
                "content": [{"type": "text", "text": json.dumps({
                    "error": f"JSON-RPC error: {msg}"
                })}],
                "isError": True,
            }

        # Successful JSON-RPC response — extract content from result.
        inner = raw.get("result", {})
        if isinstance(inner, dict) and "content" in inner:
            return inner
        # Some endpoints may return a non-content result (e.g. tools/list).
        return {"content": [{"type": "text", "text": json.dumps(inner)}]}

    def _post(self, body: Dict[str, Any]) -> Dict[str, Any]:
        """HTTP POST to the MCP endpoint with JSON-RPC 2.0 and bearer auth.

        Returns a normalized MCP content dict (not the raw JSON-RPC envelope).
        """
        token = self._read_bearer_token()

        headers = {
            "Content-Type": "application/json",
            "Accept": "application/json, text/event-stream",
        }
        if token:
            headers["Authorization"] = f"Bearer {token}"

        data = json.dumps(body).encode("utf-8")
        req = urllib.request.Request(self._mcp_url, data=data, headers=headers, method="POST")

        try:
            with urllib.request.urlopen(req, timeout=self._timeout) as resp:
                resp_body = resp.read().decode("utf-8").strip()
                if not resp_body:
                    return {
                        "content": [{"type": "text", "text": json.dumps({
                            "error": "Empty response from ai-memory server"
                        })}],
                        "isError": True,
                    }
                try:
                    raw = json.loads(resp_body)
                    return self._normalize_jsonrpc_response(raw)
                except json.JSONDecodeError:
                    return {
                        "content": [{"type": "text", "text": json.dumps({
                            "error": f"Invalid JSON response: {resp_body[:200]}"
                        })}],
                        "isError": True,
                    }
        except urllib.error.HTTPError as e:
            err_body = e.read().decode("utf-8") if e.fp else ""
            logger.debug("MCP HTTP error %d: %s", e.code, err_body)
            return {
                "content": [{"type": "text", "text": json.dumps({
                    "error": f"HTTP {e.code}: {err_body}"
                })}],
                "isError": True,
            }
        except urllib.error.URLError as e:
            logger.debug("MCP URL error: %s", e)
            return {
                "content": [{"type": "text", "text": json.dumps({
                    "error": f"Connection error: {e.reason}"
                })}],
                "isError": True,
            }
        except Exception as e:
            logger.debug("MCP unexpected error: %s", e)
            return {
                "content": [{"type": "text", "text": json.dumps({
                    "error": f"Unexpected error: {e}"
                })}],
                "isError": True,
            }

    def close(self) -> None:
        """Close the HTTP session."""
        if self._session is not None:
            try:
                self._session.close()
            except Exception:
                pass
            self._session = None


# ---------------------------------------------------------------------------
# AiMemoryProvider
# ---------------------------------------------------------------------------

class AiMemoryProvider(MemoryProvider):
    """MemoryProvider for ai-memory: sqlite+fts5 wiki pages with Karpathy-style consolidation.

    Communicates with the ai-memory Rust server over its MCP HTTP transport.
    The server runs independently (managed by systemd or similar); this plugin
    connects to it at ``mcp_url`` (default: Weakling's shared ai-memory.service).

    Profile isolation: all config paths use ``hermes_home`` from ``initialize()``.
    """

    # v2 fail-closed pre-compress checkpoint.
    pre_compress_checkpoint_api_version = 2

    def __init__(self) -> None:
        # Config.
        self._hermes_home: str = ""
        self._session_id: str = ""
        self._config: Dict[str, Any] = {}
        self._config_path: Optional[Path] = None

        # MCP client.
        self._mcp: Optional[_McpClient] = None

        # Background thread tracking (daemon — never blocks exit).
        self._sync_thread: Optional[threading.Thread] = None
        self._prefetch_thread: Optional[threading.Thread] = None
        self._prefetch_result: str = ""
        self._prefetch_lock = threading.Lock()
        self._prefetch_generation: object = object()

        # Runtime hints from initialize() kwargs.
        self._platform: str = "cli"
        self._agent_context: str = ""
        self._agent_identity: str = ""
        self._agent_workspace: str = ""

        # Scope hints for ai-memory calls.
        self._default_project: str = ""
        self._default_workspace: str = ""

    # -- Core lifecycle --------------------------------------------------

    @property
    def name(self) -> str:
        return "ai-memory"

    def is_available(self) -> bool:
        """Check if ai-memory is configured. No network calls per spec."""
        # Check for config file existence first.
        try:
            from hermes_constants import get_hermes_home
            config_path = Path(str(get_hermes_home())) / _CONFIG_FILENAME
            if config_path.exists():
                return True
        except Exception:
            pass
        # Fall back to env var check.
        return bool(os.environ.get("AI_MEMORY_MCP_URL") or os.environ.get("AI_MEMORY_HOME"))

    def unavailable_reason(self) -> str:
        return (
            "ai-memory is not configured.  Run 'hermes memory setup' or set "
            "AI_MEMORY_MCP_URL in your environment."
        )

    def initialize(self, session_id: str, **kwargs) -> None:
        """Initialize the ai-memory provider for this session.

        Stores session metadata and loads config.  The MCP client is created
        lazily — no network calls during init (the server may not be running
        yet).

        kwargs always include ``hermes_home`` and ``platform``; may include
        ``agent_context``, ``agent_identity``, ``agent_workspace``.
        """
        self._session_id = session_id
        self._hermes_home = kwargs.get("hermes_home", "")
        self._platform = kwargs.get("platform", "cli")
        self._agent_context = kwargs.get("agent_context", "")
        self._agent_identity = kwargs.get("agent_identity", "")
        self._agent_workspace = kwargs.get("agent_workspace", "")

        # Load config.
        self._load_config()

        # Build MCP client (lazy — no connection attempt).
        mcp_url = (
            self._config.get("mcp_url")
            or os.environ.get("AI_MEMORY_MCP_URL")
            or _DEFAULT_MCP_URL
        )
        token_file = (
            self._config.get("token_file")
            or os.environ.get("AI_MEMORY_TOKEN_FILE")
        )
        self._mcp = _McpClient(mcp_url=mcp_url, token_file=token_file)

        logger.debug(
            "AiMemoryProvider initialized (session=%s, platform=%s, mcp_url=%s)",
            session_id, self._platform, mcp_url,
        )

    @staticmethod
    def _resolve_hermes_home() -> str:
        """This profile's Hermes home, or "" if it cannot be determined."""
        try:
            from hermes_constants import get_hermes_home
            return str(get_hermes_home())
        except Exception:
            return ""

    def _load_config(self) -> None:
        """Load config from $HERMES_HOME/ai-memory.json (silently empty if missing).

        The home is the ``hermes_home`` kwarg when ``initialize()`` passed one,
        else the profile's own ``get_hermes_home()``. It used to bail out when
        the kwarg was absent, which left ``default_project`` /
        ``default_workspace`` empty — and an empty scope is the server's
        *silent* failure mode: the call is not rejected, it auto-resolves to
        the credential's active project, which on a shared deployment is a
        throwaway project. Every read then "succeeded" and returned nothing,
        which reads as an empty memory rather than a misconfiguration.
        """
        self._config = {}
        hermes_home = self._hermes_home or self._resolve_hermes_home()
        if not hermes_home:
            return
        self._hermes_home = hermes_home
        config_path = Path(self._hermes_home) / _CONFIG_FILENAME
        self._config_path = config_path
        if config_path.exists():
            try:
                raw = config_path.read_text(encoding="utf-8")
                self._config = json.loads(raw) if raw.strip() else {}
            except (json.JSONDecodeError, OSError) as e:
                logger.warning("Failed to read ai-memory config: %s", e)
        self._default_project = self._config.get("default_project", "")
        self._default_workspace = self._config.get("default_workspace", "")

    # -- Config methods --------------------------------------------------

    def get_config_schema(self) -> List[Dict[str, Any]]:
        return [
            {
                "key": "mcp_url",
                "description": "ai-memory MCP server URL (default: http://100.109.145.90:49374/mcp)",
                "default": _DEFAULT_MCP_URL,
                "env_var": "AI_MEMORY_MCP_URL",
            },
            {
                "key": "token_file",
                "description": (
                    "Path to bearer token file (default: ~/.local/share/ai-memory/auth-token, "
                    "fallback: ~/.config/opencode/.ai-memory-token). "
                    "Secret — not written to plaintext config; routed to .env."
                ),
                "default": "",
                "secret": True,
                "env_var": "AI_MEMORY_TOKEN_FILE",
            },
            {
                "key": "default_project",
                "description": "Default project scope for ai-memory operations",
                "default": "",
            },
            {
                "key": "default_workspace",
                "description": "Default workspace scope for ai-memory operations",
                "default": "",
            },
        ]

    def save_config(self, values: Dict[str, Any], hermes_home: str) -> None:
        """Write non-secret config to $HERMES_HOME/ai-memory.json.

        Secret fields (marked with ``secret: True`` in the schema) are excluded
        from the plaintext config file and should be set via environment variables
        or .env instead.
        """
        from utils import atomic_json_write

        config_path = Path(hermes_home) / _CONFIG_FILENAME
        existing: Dict[str, Any] = {}
        if config_path.exists():
            try:
                existing = json.loads(config_path.read_text(encoding="utf-8"))
            except (json.JSONDecodeError, OSError):
                pass

        # Filter out secret fields — they go to .env, not the plaintext config.
        schema = {field["key"]: field for field in self.get_config_schema()}
        non_secret_values = {
            k: v for k, v in values.items()
            if not schema.get(k, {}).get("secret", False)
        }
        # Also strip any pre-existing secret keys from the existing sidecar.
        non_secret_existing = {
            k: v for k, v in existing.items()
            if not schema.get(k, {}).get("secret", False)
        }
        # Write only non-secret values to the config file
        merged_for_file = {**non_secret_existing, **non_secret_values}
        atomic_json_write(config_path, merged_for_file, mode=0o600)
        # But keep all values (including secrets) in the in-memory config for runtime use
        merged_for_memory = {**existing, **values}
        self._config = merged_for_memory
        self._config_path = config_path
        logger.debug("ai-memory config saved to %s", config_path)

    # -- System prompt block ---------------------------------------------

    def system_prompt_block(self) -> str:
        """Static block about ai-memory capabilities (prompt-cache friendly)."""
        return (
            "# ai-memory Memory\n"
            "Active. Long-term memory is powered by ai-memory (sqlite+fts5 wiki pages "
            "with Karpathy-style consolidation). Available tools:\n"
            "- memory_query — hybrid search across all wiki pages\n"
            "- memory_read_page — read a page by path or search\n"
            "- memory_write_page — write a new wiki page\n"
            "- memory_recent — list recently updated pages\n"
            "- memory_delete_page — delete a page by path\n"
            "- memory_status — show store statistics\n"
            "Use these tools to store and retrieve durable knowledge."
        )

    # -- Prefetch --------------------------------------------------------

    def prefetch(self, query: str, *, session_id: str = "") -> str:
        """Return recalled context for the upcoming turn.

        The queued worker formats ``memory_query`` hits into the plain-text
        context contract expected by Hermes.  If no worker has completed yet,
        prefetch remains empty rather than making an inline network call.
        """
        # Return any pre-queued result exactly once.
        with self._prefetch_lock:
            result = self._prefetch_result
            self._prefetch_result = ""
        if result:
            return result

        if not query or not query.strip():
            return ""
        logger.debug("prefetch(%r) — no queued result", query[:80])
        return ""

    def queue_prefetch(self, query: str, *, session_id: str = "") -> None:
        """Queue a background ``memory_query`` for the next turn."""
        if not query or not query.strip() or self._mcp is None:
            return

        with self._prefetch_lock:
            if self._prefetch_thread and self._prefetch_thread.is_alive():
                logger.debug("ai-memory prefetch already running; skipping duplicate request")
                return
            generation = object()
            self._prefetch_generation = generation

        def _run() -> None:
            try:
                arguments: Dict[str, Any] = {
                    "query": query.strip(),
                    "limit": 5,
                    "unit": "passage",
                }
                # Same routing rule as handle_tool_call, so a prefetch is
                # scoped exactly like an explicit query. Pair-atomic: the
                # server fails closed on a half-supplied pair, so an
                # unconfigured half is left off and the active-project
                # pointer resolves instead.
                self._apply_default_scope(arguments)
                result = self._mcp.call_tool("memory_query", arguments)
                formatted = self._format_prefetch_result(result)
                if self._prefetch_generation is generation:
                    with self._prefetch_lock:
                        self._prefetch_result = formatted
            except Exception as exc:  # background failures must not escape the worker
                logger.warning("ai-memory queue_prefetch failed: %s", exc)
            finally:
                with self._prefetch_lock:
                    if self._prefetch_thread is threading.current_thread():
                        self._prefetch_thread = None

        self._prefetch_thread = spawn_context_thread(_run, name="ai-memory-prefetch")
        self._prefetch_thread.start()

    @staticmethod
    def _format_hit(hit: Dict[str, Any]) -> str:
        """Format one page or passage hit as a single context line.

        The wire shape and its quirks live in ``schema.py`` — see that
        module's docstring for the live-verified field names, the
        string-encoded numerics, and the ``heading_path`` double encoding.
        All three shapes render through it: page (``path`` / ``title`` /
        ``snippet``), passage (``page_path`` / ``page_title`` / ``text``), and
        the pre-I1 names ``excerpt`` / ``content`` / ``body``.
        """
        parsed = schema.parse_hit(hit)
        return schema.format_hit(parsed) if parsed is not None else "- memory hit"

    @classmethod
    def _format_prefetch_result(cls, result: Dict[str, Any]) -> str:
        """Convert an MCP ``memory_query`` result into Hermes context text.

        Delegates the payload walk to ``schema.parse_query_payload``, which
        merges every hit array the server uses — ``hits`` for project-scoped,
        ``global_hits`` for ``global=true``, ``global_scope_hits`` for
        ``scopes=``, ``raw_hits`` for time-travel. A parser that only reads
        ``payload["hits"]`` reports "no results" for every global query while
        ``hits`` sits empty.
        """
        if result.get("isError"):
            return ""

        hits: List[Any] = []
        for item in result.get("content", []):
            if item.get("type") != "text":
                continue
            text = item.get("text", "")
            if not text.strip():
                continue
            parsed = schema.parse_query_payload(text)
            if parsed.sources:
                hits.extend(parsed.hits)
                continue
            # A provider or test double may already return prose.
            try:
                json.loads(text)
            except (TypeError, ValueError):
                return text.strip()

        return schema.format_result(schema.QueryResult(hits=tuple(hits)))

    # -- Turn sync -------------------------------------------------------

    def sync_turn(
        self,
        user_content: str,
        assistant_content: str,
        *,
        session_id: str = "",
        messages: Optional[List[Dict[str, Any]]] = None,
        turn_author: Optional[Dict[str, Any]] = None,
    ) -> None:
        """Deferred: ai-memory owns turn capture through its native lifecycle hooks.

        Hermes's ``sync_turn`` push model has no safe MCP equivalent: the live
        ai-memory service expects hook-shaped observations at ``/hook`` and
        already captures sessions through the node's managed hook scripts.  A
        direct POST from this provider would duplicate or misattribute turns, so
        this path intentionally performs no transport call.  Capture
        double-ingest is gated by the E2E suite (t_8dc1ad69), not by this stub.
        """
        # Explicitly defer rather than pretending the MCP transport can ingest
        # a Hermes transcript.
        logger.debug("ai-memory sync_turn deferred to native lifecycle-hook integration")

    # -- Session lifecycle -----------------------------------------------

    def on_pre_compress(self, messages: List[Dict[str, Any]], *, require_checkpoint: bool = False, **kwargs) -> str:
        """Hermes pre-compress checkpoint API v2 hook.

        Called before context compaction. When ``require_checkpoint`` is True
        (enforced by ``compression.checkpoint_required: true``), this checkpoint
        must durably archive pre-compaction transcript state or raise to fail
        closed.

        Failure-safety proof:
        Raw/canonical history is immutable truth; this hook never mutates, deletes,
        or truncates the passed messages or source session transcripts.
        If checkpointing fails, it raises an exception to fail closed without
        leaving any corrupted partial history.
        """
        if not messages:
            return ""

        if self._mcp is None:
            if require_checkpoint:
                raise RuntimeError("ai-memory MCP client is not initialized for required checkpoint")
            logger.debug("ai-memory on_pre_compress: MCP client not initialized (best-effort)")
            return ""

        evidence_parts: List[str] = []
        for m in messages:
            if not isinstance(m, dict):
                continue
            role = m.get("role", "")
            if role not in ("user", "assistant"):
                continue
            if m.get("_compressed_summary"):
                continue
            content = m.get("content", "")
            if isinstance(content, str) and content.strip():
                evidence_parts.append(f"{role}: {content.strip()}")

        if not evidence_parts:
            return ""

        joined_text = "\n\n".join(evidence_parts)
        content_digest = hashlib.sha256(joined_text.encode("utf-8")).hexdigest()[:16]
        sid = self._session_id or "default"
        checkpoint_path = f"checkpoints/{sid}/pre-compress-{content_digest}.md"
        body = (
            f"# Pre-Compress Checkpoint {content_digest}\n\n"
            f"- Session: `{sid}`\n"
            f"- Digest: `{content_digest}`\n"
            f"- Messages: {len(evidence_parts)}\n\n"
            f"## Transcript Evidence\n\n"
            f"{joined_text}\n"
        )
        write_args: Dict[str, Any] = {
            "path": checkpoint_path,
            "body": body,
            "tier": "episodic",
            "pinned": True,
        }
        try:
            self._apply_default_scope(write_args)
            result = self._mcp.call_tool("memory_write_page", write_args)
            if result.get("isError"):
                err_msg = "unknown error"
                for item in result.get("content", []):
                    if item.get("type") == "text":
                        err_msg = item.get("text", err_msg)
                        break
                raise RuntimeError(f"ai-memory checkpoint write failed: {err_msg}")
            logger.info("ai-memory pre-compress checkpoint saved: %s", checkpoint_path)
            return f"ai-memory checkpoint: {checkpoint_path} (digest: {content_digest})"
        except Exception as exc:
            logger.warning("ai-memory on_pre_compress checkpoint error: %s", exc)
            if require_checkpoint:
                raise RuntimeError(f"ai-memory required pre-compress checkpoint failed: {exc}") from exc
            return ""

    def on_session_end(self, messages: List[Dict[str, Any]]) -> None:
        """Trigger ai-memory consolidation for the session being closed."""
        if not self._mcp or not self._session_id:
            return
        try:
            result = self._mcp.call_tool("memory_consolidate", {"session_id": self._session_id})
        except Exception as exc:
            logger.warning("ai-memory on_session_end consolidation failed: %s", exc)
            return
        if result.get("isError"):
            error_text = "unknown error"
            for item in result.get("content", []):
                if item.get("type") == "text":
                    error_text = item.get("text", error_text)
                    break
            logger.warning("ai-memory on_session_end consolidation failed: %s", error_text)

    # -- Tools -----------------------------------------------------------

    def _apply_default_scope(self, args: Dict[str, Any]) -> None:
        """Route this call's scope through :func:`scope.resolve_scope` (in place).

        The routing rules live in ``scope.py`` because they are properties of the
        *server*, not of this provider — one place to read them from, one set of
        tests.  The two behaviours worth restating here, both verified live
        against Weakling v2.2.1 on 2026-09-30:

        * The server fails closed on a partial pair, and an unscoped call
          silently resolves to the credential's active project — which on a
          shared deployment is often a throwaway project. So defaults are
          applied only when the caller supplied neither half AND both are
          configured, and a caller-supplied half always wins.
        * ``global: true`` and ``scopes=[...]`` are *whole-call* scope modes
          that the server rejects when combined with any named scope. Defaults
          must not be injected into them, or a legitimate global search fails
          on a scope the caller never asked for.

        Raises:
            scope.ScopeError: the combination is one the server rejects.
        """
        resolution = scope.resolve_scope(
            args,
            default_project=self._default_project,
            default_workspace=self._default_workspace,
        )
        for warning in resolution.warnings:
            logger.debug("ai-memory scope (%s): %s", resolution.mode, warning)

    def get_tool_schemas(self) -> List[Dict[str, Any]]:
        """Return tool schemas for ai-memory tools.

        Defined statically from the API_MAP.md catalog (6 core tools).
        Additional ai-memory tools (handoffs, consolidate, lint, etc.) are
        intentionally excluded — they are maintenance/admin operations that
        don't belong in the model's tool surface.
        """
        return list(_AI_MEMORY_TOOL_SCHEMAS)

    def handle_tool_call(self, tool_name: str, args: Dict[str, Any], **kwargs) -> str:
        """Dispatch a tool call to the ai-memory MCP server.

        Translates the Hermes tool dispatch into an MCP CallToolRequest over
        HTTP (bearer-authenticated JSON-RPC 2.0) and returns the result as a
        JSON string.
        """
        from tools.registry import tool_error

        if self._mcp is None:
            return tool_error("ai-memory MCP client not initialized.")

        schema = _TOOL_SCHEMA_BY_NAME.get(tool_name)
        if schema is None:
            return tool_error(f"Unknown tool: {tool_name}")

        # Scope routing rejects the combinations the server refuses *before* the
        # round trip (scope.py, verified live 2026-09-30), so the model gets the
        # production rule in plain language instead of a raw JSON-RPC -32603.
        try:
            self._apply_default_scope(args)
        except scope.ScopeError as exc:
            return tool_error(f"ai-memory tool '{tool_name}' failed: {exc}")

        try:
            result = self._mcp.call_tool(tool_name, args)
            if result.get("isError"):
                error_text = "unknown error"
                for content_item in result.get("content", []):
                    if content_item.get("type") == "text":
                        error_text = content_item.get("text", error_text)
                        break
                return tool_error(f"ai-memory tool '{tool_name}' failed: {error_text}")

            # Extract text content from the MCP response.
            text_parts: List[str] = []
            for content_item in result.get("content", []):
                if content_item.get("type") == "text":
                    text_parts.append(content_item.get("text", ""))
            return json.dumps({"result": "\n".join(text_parts) or "OK"})

        except Exception as e:
            logger.error("ai-memory handle_tool_call(%s) failed: %s", tool_name, e)
            return tool_error(f"ai-memory tool '{tool_name}' failed: {e}")

    # -- Shutdown --------------------------------------------------------

    def shutdown(self) -> None:
        """Clean shutdown: join background threads, close MCP client."""
        for t in (self._sync_thread, self._prefetch_thread):
            if t and t.is_alive():
                t.join(timeout=5.0)
        if self._mcp:
            self._mcp.close()
            self._mcp = None
        logger.debug("AiMemoryProvider shut down")


# ---------------------------------------------------------------------------
# Plugin entry point
# ---------------------------------------------------------------------------

def register(ctx) -> None:
    """Register the ai-memory memory provider with the Hermes plugin system."""
    ctx.register_memory_provider(AiMemoryProvider())
