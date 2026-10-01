"""Scope routing for the ai-memory MCP surface.

The live server is a *static* MCP client target: it has no session identity
to resolve "the current project" from, so every project-scoped call must name
its scope explicitly. What it actually accepts was measured against
Weakling's shared ``ai-memory.service`` (v2.2.1) on 2026-09-30, and the
contract is asymmetric in a way that is easy to get wrong:

===================================  =======================================
argument shape                       server behaviour
===================================  =======================================
``project`` only                     OK — server fills workspace ``default``
``workspace`` only                   ERROR ``workspace and project must be
                                     provided together``
both                                 OK
neither                              OK, but the scope is *auto-resolved*
                                     from the active-project pointer
``global=true`` + any scope          ERROR ``global cannot be combined with
                                     workspace/project/scopes``
``scopes=[...]`` + ``project``       ERROR ``scopes cannot be combined with
                                     workspace/project``
===================================  =======================================

The dangerous row is "neither". The server does not error — it silently
resolves to whatever project the bearer credential's active pointer names.
On a shared server that pointer is frequently a throwaway auto-created
project: a live ``memory_read_page`` with no scope resolved to
``default/claude-directsdk-cwd-_w97k3rh`` and reported the page missing,
while the same path existed under ``default/hermes-agent``. An unscoped read
therefore *looks* like a legitimately empty result, which is how a wrong
scope hides behind "no memories found".

This module turns that into an explicit decision the provider can act on and
test, instead of a silent misroute.
"""

from __future__ import annotations

import logging
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Callable, Dict, Iterable, List, Optional, Sequence, Set, Tuple

# The adapter directory is named ``ai-memory``, which is not a valid Python
# identifier, so it can never be imported as a package. The plugin is loaded
# by file path (and Hermes discovers it by directory), which leaves its
# siblings reachable only as top-level modules — hence the explicit sys.path
# registration here rather than a relative import.
if str(Path(__file__).resolve().parent) not in sys.path:
    sys.path.append(str(Path(__file__).resolve().parent))

from schema import QueryResult, parse_query_payload  # noqa: E402

logger = logging.getLogger(__name__)

# Scope resolution modes, in the order :func:`resolve_scope` considers them.
MODE_GLOBAL = "global"
MODE_SCOPES = "scopes"
MODE_PAIR = "pair"
MODE_PROJECT_ONLY = "project-only"
MODE_UNSCOPED = "unscoped"

#: Arguments that name a scope, and therefore conflict with each other.
_SCOPE_ARGS: Tuple[str, ...] = ("workspace", "project", "scopes")


class ScopeError(ValueError):
    """A call whose scope arguments the server is known to reject.

    Raised *before* the round trip so the model gets the production rule in
    plain language instead of a raw JSON-RPC ``-32603``.
    """


@dataclass(frozen=True)
class ScopeResolution:
    """The outcome of routing one call's scope.

    ``arguments`` is the argument dict to actually send. ``mode`` records
    which of :data:`MODE_GLOBAL` / :data:`MODE_SCOPES` / :data:`MODE_PAIR` /
    :data:`MODE_PROJECT_ONLY` / :data:`MODE_UNSCOPED` applied, and ``warnings``
    carries the conditions worth surfacing without failing the call.
    """

    arguments: Dict[str, Any]
    mode: str
    warnings: Tuple[str, ...] = ()

    @property
    def is_global(self) -> bool:
        return self.mode == MODE_GLOBAL

    @property
    def is_unscoped(self) -> bool:
        return self.mode == MODE_UNSCOPED


def resolve_scope(
    arguments: Dict[str, Any],
    *,
    default_project: str = "",
    default_workspace: str = "",
) -> ScopeResolution:
    """Route one call's scope, mutating and returning ``arguments`` in place.

    Precedence, matching the server's own precedence:

    1. ``global: true`` — cross-project search. Must carry nothing else.
    2. ``scopes=[...]`` — explicit multi-project search. Must carry neither
       ``project`` nor ``workspace``.
    3. An explicit ``project`` (with or without ``workspace``).
    4. The configured default pair, applied only when *both* halves exist.
    5. Nothing — unscoped, with a warning.

    A caller-supplied half always wins over configuration; defaults never
    overwrite an explicit argument.

    Raises:
        ScopeError: the argument combination is one the server rejects.
    """
    warnings: List[str] = []

    is_global = bool(arguments.get("global"))
    scopes = arguments.get("scopes")
    has_scopes = isinstance(scopes, (list, tuple)) and len(scopes) > 0
    project = arguments.get("project") or ""
    workspace = arguments.get("workspace") or ""

    # -- 1. global ---------------------------------------------------------
    if is_global:
        conflicts = [
            key for key in ("workspace", "project")
            if arguments.get(key)
        ]
        if has_scopes:
            conflicts.append("scopes")
        if conflicts:
            raise ScopeError(
                "global=true cannot be combined with "
                f"{'/'.join(conflicts)} on the ai-memory server; drop "
                f"{'/'.join(conflicts)} or drop global=true."
            )
        return ScopeResolution(arguments, MODE_GLOBAL)

    # -- 2. explicit multi-project scopes ---------------------------------
    if has_scopes:
        conflicts = [key for key in ("project", "workspace") if arguments.get(key)]
        if conflicts:
            raise ScopeError(
                "scopes=[...] cannot be combined with "
                f"{'/'.join(conflicts)} on the ai-memory server; pass the "
                "multi-project list in scopes= and nothing else."
            )
        return ScopeResolution(arguments, MODE_SCOPES)

    # -- 3. caller-supplied scope -----------------------------------------
    if workspace and not project:
        # The server fails closed on this shape. Complete the pair from
        # configuration rather than sending a call that cannot succeed.
        if not default_project:
            raise ScopeError(
                f"workspace={workspace!r} was passed without a project. The "
                "ai-memory server requires both, and no default_project is "
                "configured; pass project= as well."
            )
        arguments["project"] = default_project
        warnings.append(
            f"workspace={workspace!r} completed with the configured "
            f"project={default_project!r}; the server rejects a "
            "workspace without a project."
        )
        return ScopeResolution(arguments, MODE_PAIR, tuple(warnings))

    if project and workspace:
        return ScopeResolution(arguments, MODE_PAIR)

    if project:
        return ScopeResolution(arguments, MODE_PROJECT_ONLY)

    # -- 4. configured default pair ---------------------------------------
    if default_project and default_workspace:
        arguments["project"] = default_project
        arguments["workspace"] = default_workspace
        return ScopeResolution(arguments, MODE_PAIR)

    # -- 5. unscoped -------------------------------------------------------
    warnings.append(
        "no project scope: the ai-memory server will auto-resolve this call "
        "to the credential's active project, which on a shared deployment is "
        "often a throwaway project. Configure default_project and "
        "default_workspace, or pass project=."
    )
    return ScopeResolution(arguments, MODE_UNSCOPED, tuple(warnings))


def validate_pair(
    call_tool: Callable[[str, Dict[str, Any]], Dict[str, Any]],
    workspace: str,
    project: str,
) -> Tuple[bool, str]:
    """Check that a ``(workspace, project)`` pair exists on the server.

    Wraps ``memory_status``, which is the cheapest scoped read. Returns
    ``(ok, detail)``: ``detail`` is a short human-readable reason, suitable
    for a config-time error or a warning line.

    This is the check that would have caught the ``workspace=main`` /
    ``project=karellen-main`` defaults before they turned every read into an
    error — the workspace does not exist on the live server, and
    ``karellen-main`` resolves to zero pages.
    """
    try:
        result = call_tool("memory_status", {"workspace": workspace, "project": project})
    except Exception as exc:  # network/HTTP failures are not "pair missing"
        return False, f"could not reach the ai-memory server: {exc}"

    if not isinstance(result, dict):
        return False, "unexpected response shape from memory_status"
    if result.get("isError"):
        return False, _error_text(result)

    text = _first_text(result)
    if not text:
        return False, "empty response from memory_status"
    if "not found" in text:
        return False, text.strip()[:200]
    return True, "ok"


def discover_projects(
    call_tool: Callable[[str, Dict[str, Any]], Dict[str, Any]],
    *,
    probe_queries: Sequence[str] = ("memory", "session", "hermes"),
    limit: int = 50,
) -> List[Tuple[str, str]]:
    """Enumerate ``(workspace, project)`` pairs that hold retrievable content.

    The server exposes no project-listing tool, so this samples
    ``memory_query(global=true)`` — the one call whose hits are annotated
    with ``workspace_name`` / ``project_name`` — and collects the distinct
    pairs.

    Returns pairs sorted, workspace-major. A project with no content matching
    any probe query does not appear, so absence here is not proof of
    absence; it is good enough to tell a configured default that resolves to
    real data from one that resolves to nothing.
    """
    pairs: Set[Tuple[str, str]] = set()
    for query in probe_queries:
        arguments: Dict[str, Any] = {"query": query, "global": True, "limit": limit}
        try:
            result = call_tool("memory_query", arguments)
        except Exception as exc:
            logger.warning("ai-memory scope discovery query %r failed: %s", query, exc)
            continue
        if not isinstance(result, dict) or result.get("isError"):
            continue
        parsed = parse_query_payload(_first_text(result) or "{}")
        for hit in parsed.cross_scope_hits():
            if hit.workspace_name and hit.project_name:
                pairs.add((hit.workspace_name, hit.project_name))
    return sorted(pairs)


# ---------------------------------------------------------------------------
# Internals
# ---------------------------------------------------------------------------

def _first_text(result: Dict[str, Any]) -> str:
    """Concatenate the text blocks of an MCP tool result."""
    parts = [
        item.get("text", "")
        for item in (result.get("content") or [])
        if isinstance(item, dict) and item.get("type") == "text"
    ]
    return "".join(parts)


def _error_text(result: Dict[str, Any]) -> str:
    """Best-effort error message out of an MCP tool result."""
    text = _first_text(result)
    if text:
        return text.strip()[:200]
    return "ai-memory call failed"


def format_scope_hint(pairs: Iterable[Tuple[str, str]], limit: int = 12) -> str:
    """Render discovered pairs as a one-line, human-readable hint."""
    listed = [f"{ws}/{proj}" for ws, proj in list(pairs)[:limit]]
    if not listed:
        return "no projects discovered on the ai-memory server"
    suffix = " ..." if len(list(pairs)) > limit else ""
    return "known scopes: " + ", ".join(listed) + suffix
