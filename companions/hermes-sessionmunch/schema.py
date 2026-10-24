"""Typed parsing of live ai-memory MCP retrieval payloads.

Every field name, encoding quirk, and array name in this module was read off
the *deployed* server (Weakling's shared ``ai-memory.service``,
v2.2.1-retrieval-section-index) with a live ``tools/list`` + ``tools/call``
probe, not from ``API_MAP.md`` — that map documents 17 tools (the server
exposes 19) and predates the passage index entirely.

Three wire facts drive the whole module, all verified against the live
server on 2026-09-30:

1. **Retrieval unit changes the hit shape.** ``unit="page"`` hits carry
   ``id`` / ``path`` / ``title`` / ``snippet``. ``unit="passage"`` hits carry
   ``page_id`` / ``page_path`` / ``page_title`` / ``text`` and no ``id`` /
   ``path`` / ``title`` / ``snippet`` at all. A parser keyed on the old
   page-only names silently renders every passage hit as an empty body.

2. **Hits do not all land in ``hits``.** A ``global=true`` query returns the
   results in ``global_hits`` and leaves ``hits`` *empty*; ``scopes=`` and
   time-travel return ``global_scope_hits`` / ``raw_hits`` respectively. A
   parser that only reads ``payload["hits"]`` reports "no results" for every
   global query.

3. **Numbers and nested values arrive as strings.** In passage mode
   ``start_byte`` / ``end_byte`` / ``rank`` / ``dense_rank`` / ``rrf_score``
   are JSON *strings*; ``heading_path`` is a JSON string *encoding a JSON
   array* (``'"[\\"Technical Findings\\"]"'``); and with
   ``parent_expansion`` set, ``parent`` is a **Python-repr** string
   (``"{'section_id': '...'}"``) that ``json.loads`` rejects outright.
"""

from __future__ import annotations

import ast
import json
import logging
from dataclasses import dataclass, field
from typing import Any, Dict, List, Optional, Tuple, Union

logger = logging.getLogger(__name__)

# Response keys the server uses for hit arrays, in the order they should be
# merged. ``hits`` is project/scopes-scoped; the rest are the other modes.
_HIT_ARRAY_KEYS: Tuple[str, ...] = (
    "hits",
    "global_hits",
    "global_scope_hits",
    "raw_hits",
)

# Keys whose value the server emits as a Python-repr / JSON-encoded string
# rather than a real JSON value.
_EMBEDDED_STRING_KEYS: Tuple[str, ...] = ("heading_path", "parent")

#: Largest magnitude still read as a relevance score. ``memory_recent`` reuses
#: ``rank`` as an *ordering key* rather than a score — the live server puts a
#: microsecond-precision epoch there (``1790731354569632.0``) — while
#: ``memory_query(unit="page")`` puts a signed relevance score in roughly
#: (-1, 1). Rendering the former as ``[score: 1.79e+15]`` gives the model a
#: number that looks broken and carries no ranking meaning, so any magnitude
#: past this bound is treated as a timestamp and not displayed.
_MAX_PLAUSIBLE_SCORE = 1e9

# Accepted field names, most-current first. The live v2.2.1 server sends
# ``page_path`` / ``page_title`` / ``text`` for a passage and
# ``path`` / ``title`` / ``snippet`` for a page, but pre-I1 payloads and test
# doubles used ``excerpt`` / ``content`` / ``body``, and some callers send an
# explicit ``score``. Accepting the older names costs nothing and keeps such a
# payload renderable instead of silently bodyless.
_PASSAGE_PATH_KEYS: Tuple[str, ...] = ("page_path", "path")
_PASSAGE_TITLE_KEYS: Tuple[str, ...] = ("page_title", "title")
_BODY_KEYS: Tuple[str, ...] = ("text", "body", "snippet", "excerpt", "content")
_SCORE_KEYS: Tuple[str, ...] = ("rrf_score", "score", "rank")


def _first_present(raw: Dict[str, Any], keys: Tuple[str, ...]) -> Any:
    """The first key in ``keys`` whose value is non-empty, else ``None``."""
    for key in keys:
        value = raw.get(key)
        if value:
            return value
    return None


# ---------------------------------------------------------------------------
# Scalar coercion
# ---------------------------------------------------------------------------

def as_int(value: Any) -> Optional[int]:
    """Coerce a wire scalar to ``int``, or ``None``.

    The server serialises ``start_byte`` / ``end_byte`` / ``rank`` /
    ``dense_rank`` as JSON strings in passage mode, so a plain ``int()`` on
    the decoded payload raises. Returns ``None`` for absent or unparseable
    values rather than guessing.
    """
    if value is None or isinstance(value, bool):
        return None
    if isinstance(value, int):
        return value
    if isinstance(value, float):
        return int(value) if value.is_integer() else None
    if isinstance(value, str):
        text = value.strip()
        if not text:
            return None
        try:
            return int(text)
        except ValueError:
            try:
                as_float = float(text)
            except ValueError:
                return None
            return int(as_float) if as_float.is_integer() else None
    return None


def as_float(value: Any) -> Optional[float]:
    """Coerce a wire scalar to ``float``, or ``None``. See :func:`as_int`."""
    if value is None or isinstance(value, bool):
        return None
    if isinstance(value, (int, float)):
        return float(value)
    if isinstance(value, str):
        text = value.strip()
        if not text:
            return None
        try:
            return float(text)
        except ValueError:
            return None
    return None


def as_text(value: Any) -> str:
    """Coerce a wire scalar to ``str``, trimming. ``None`` becomes ``""``."""
    if value is None:
        return ""
    if isinstance(value, str):
        return value.strip()
    return str(value).strip()


def decode_embedded(value: Any) -> Any:
    """Decode a value the server serialised as a string *inside* the JSON.

    Two encodings are in use on the live server:

    * ``heading_path`` is a JSON array re-encoded as a JSON string, so one
      ``json.loads`` yields the list.
    * ``parent`` is a Python ``repr`` of a dict (single-quoted keys, bare
      ``None``/``True``), which ``json.loads`` rejects and ``ast.literal_eval``
      accepts.

    Anything that decodes to a container is returned decoded; anything else —
    including a plain unencoded string — is returned unchanged, so a
    non-embedded value passes through untouched.
    """
    if not isinstance(value, str):
        return value
    text = value.strip()
    if not text:
        return value
    if text[0] in "[{":
        for loads in (json.loads, ast.literal_eval):
            try:
                decoded = loads(text)
            except (ValueError, SyntaxError):
                continue
            if isinstance(decoded, (list, dict)):
                return decoded
    return value


def as_heading_path(value: Any) -> Tuple[str, ...]:
    """Normalise ``heading_path`` to a tuple of heading segments.

    The live value is the JSON *string* ``'["Technical Findings"]'``. A
    future server that emits a real array, or a plain heading string, is
    accepted too.
    """
    decoded = decode_embedded(value)
    if isinstance(decoded, (list, tuple)):
        return tuple(as_text(item) for item in decoded if as_text(item))
    text = as_text(decoded)
    return (text,) if text else ()


def as_mapping(value: Any) -> Dict[str, Any]:
    """Normalise a nested object field to a ``dict``.

    ``parent`` arrives as a Python-repr string; an undecodable value is
    preserved under ``{"raw": <original>}`` rather than dropped, so a caller
    can still surface what the server sent.
    """
    decoded = decode_embedded(value)
    if isinstance(decoded, dict):
        return dict(decoded)
    if isinstance(decoded, list):
        return {"items": decoded}
    if decoded is None:
        return {}
    return {"raw": decoded}


# ---------------------------------------------------------------------------
# Hit records
# ---------------------------------------------------------------------------

@dataclass(frozen=True)
class PageHit:
    """A ``unit="page"`` hit.

    Field names verified live: ``id``, ``path``, ``title``, ``snippet``,
    ``rank`` (a JSON *string*). ``workspace_name`` / ``project_name`` are
    populated only on ``global=true`` results, which is the server's own way
    of saying "this hit came from somewhere else". ``heading_path`` and
    ``score`` are accepted but not sent by the current server — the pre-I1 and
    prototype payloads did, and dropping them would render those bodyless.
    """

    id: str = ""
    path: str = ""
    title: str = ""
    snippet: str = ""
    heading_path: Tuple[str, ...] = ()
    rank: Optional[float] = None
    score: Optional[float] = None
    score_details: Dict[str, Any] = field(default_factory=dict)
    workspace_name: str = ""
    project_name: str = ""
    cross_scope: bool = False

    @property
    def body(self) -> str:
        return self.snippet


@dataclass(frozen=True)
class PassageHit:
    """A ``unit="passage"`` hit from the retrieval-section index.

    Field names verified live: ``passage_id``, ``section_id``, ``page_id``,
    ``page_path``, ``page_title``, ``heading_path``, ``text``, ``start_byte``,
    ``end_byte``, ``rank``, ``lexical_rank``, ``dense_rank``, ``rrf_score`` —
    the ranks as JSON *strings*. ``parent`` is present only when
    ``parent_expansion`` is ``section`` or ``document``.
    """

    passage_id: str = ""
    section_id: str = ""
    page_id: str = ""
    page_path: str = ""
    page_title: str = ""
    heading_path: Tuple[str, ...] = ()
    text: str = ""
    start_byte: Optional[int] = None
    end_byte: Optional[int] = None
    rank: Optional[int] = None
    lexical_rank: Optional[int] = None
    dense_rank: Optional[int] = None
    rrf_score: Optional[float] = None
    parent: Dict[str, Any] = field(default_factory=dict)

    @property
    def body(self) -> str:
        return self.text


Hit = Union[PageHit, PassageHit]


def parse_hit(raw: Any) -> Optional[Hit]:
    """Parse one hit dict into a :class:`PageHit` or :class:`PassageHit`.

    The discriminator is ``passage_id``: it exists on every passage hit and
    on no page hit. Returns ``None`` for a non-dict, so a caller can filter
    without a try/except.
    """
    if not isinstance(raw, dict):
        return None

    if raw.get("passage_id") is not None:
        return PassageHit(
            passage_id=as_text(raw.get("passage_id")),
            section_id=as_text(raw.get("section_id")),
            page_id=as_text(raw.get("page_id")),
            page_path=as_text(_first_present(raw, _PASSAGE_PATH_KEYS)),
            page_title=as_text(_first_present(raw, _PASSAGE_TITLE_KEYS)),
            heading_path=as_heading_path(raw.get("heading_path")),
            text=as_text(_first_present(raw, _BODY_KEYS)),
            start_byte=as_int(raw.get("start_byte")),
            end_byte=as_int(raw.get("end_byte")),
            rank=as_int(raw.get("rank")),
            lexical_rank=as_int(raw.get("lexical_rank")),
            dense_rank=as_int(raw.get("dense_rank")),
            rrf_score=as_float(raw.get("rrf_score")),
            parent=as_mapping(raw.get("parent")),
        )

    # Page mode. `rank` is a string on the live server; `id` doubles as the
    # page identity that passage hits spell `page_id`.
    return PageHit(
        id=as_text(raw.get("id")),
        path=as_text(_first_present(raw, _PASSAGE_PATH_KEYS)),
        title=as_text(_first_present(raw, _PASSAGE_TITLE_KEYS)),
        snippet=as_text(_first_present(raw, _BODY_KEYS)),
        heading_path=as_heading_path(raw.get("heading_path")),
        rank=as_float(raw.get("rank")),
        score=as_float(_first_present(raw, _SCORE_KEYS)),
        score_details=as_mapping(raw.get("score_details")),
        workspace_name=as_text(raw.get("workspace_name")),
        project_name=as_text(raw.get("project_name")),
        cross_scope=bool(raw.get("workspace_name") or raw.get("project_name")),
    )


# ---------------------------------------------------------------------------
# Result record
# ---------------------------------------------------------------------------

@dataclass(frozen=True)
class QueryResult:
    """Normalised view of a ``memory_query`` / ``memory_recent`` payload.

    ``hits`` merges every hit array the server used, in
    :data:`_HIT_ARRAY_KEYS` order, so a caller never has to know whether the
    query was project-scoped, ``global``, or ``scopes``-based. ``sources``
    records which arrays actually contributed, which is what makes "the
    global query returned nothing" distinguishable from "the global query
    put its results somewhere the old parser never looked".
    """

    hits: Tuple[Hit, ...] = ()
    sources: Tuple[str, ...] = ()
    streams_active: Tuple[str, ...] = ()
    mode: str = "project"

    @property
    def passages(self) -> Tuple[PassageHit, ...]:
        return tuple(h for h in self.hits if isinstance(h, PassageHit))

    @property
    def pages(self) -> Tuple[PageHit, ...]:
        return tuple(h for h in self.hits if isinstance(h, PageHit))

    def cross_scope_hits(self) -> Tuple[PageHit, ...]:
        """Hits the server tagged with a foreign workspace/project."""
        return tuple(h for h in self.hits if isinstance(h, PageHit) and h.cross_scope)


def _detect_mode(payload: Dict[str, Any]) -> str:
    """Infer the query mode from the response shape.

    ``global=true`` is the only mode that populates ``global_hits``, and the
    server leaves ``hits`` empty for it — the distinction the old parser
    collapsed.
    """
    if payload.get("global_hits"):
        return "global"
    if payload.get("global_scope_hits"):
        return "scopes"
    if payload.get("raw_hits"):
        return "time-travel"
    return "project"


def parse_query_payload(payload: Union[str, bytes, Dict[str, Any]]) -> QueryResult:
    """Parse a decoded-or-encoded retrieval payload into a :class:`QueryResult`.

    Accepts the raw text the MCP ``tools/call`` returns as well as an
    already-decoded dict. Unparseable text yields an empty result rather than
    raising, because the provider's contract is to return a string to the
    model, not to propagate a parse failure into a turn.
    """
    if isinstance(payload, (str, bytes)):
        text = payload.decode("utf-8", "replace") if isinstance(payload, bytes) else payload
        text = text.strip()
        if not text:
            return QueryResult()
        try:
            payload = json.loads(text)
        except (TypeError, ValueError):
            logger.debug("ai-memory payload was not JSON; treating as prose")
            return QueryResult()
    if not isinstance(payload, dict):
        return QueryResult()

    hits: List[Hit] = []
    sources: List[str] = []
    for key in _HIT_ARRAY_KEYS:
        raw_hits = payload.get(key)
        if not isinstance(raw_hits, list):
            continue
        parsed = [hit for hit in (parse_hit(item) for item in raw_hits) if hit is not None]
        if parsed:
            hits.extend(parsed)
            sources.append(key)

    streams = payload.get("streams_active")
    return QueryResult(
        hits=tuple(hits),
        sources=tuple(sources),
        streams_active=tuple(as_text(s) for s in streams) if isinstance(streams, list) else (),
        mode=_detect_mode(payload),
    )


# ---------------------------------------------------------------------------
# Rendering
# ---------------------------------------------------------------------------

def display_score(hit: Hit) -> Optional[float]:
    """The relevance score worth showing for ``hit``, or ``None``.

    Passage hits report ``rrf_score`` — a real fused-relevance value. Page
    hits report ``rank``, which is a relevance score on a query but a
    microsecond epoch on ``memory_recent``; see
    :data:`_MAX_PLAUSIBLE_SCORE` for why the magnitude decides.
    """
    if isinstance(hit, PassageHit):
        return hit.rrf_score
    score = hit.score if hit.score is not None else hit.rank
    if score is None or abs(score) > _MAX_PLAUSIBLE_SCORE:
        return None
    return score


def hit_label(hit: Hit) -> str:
    """The context label for ``hit``: title, then heading segments, then filename.

    A title-less hit renders under its filename, which is what a reader needs
    to act on it — an empty label tells the model nothing.
    """
    if isinstance(hit, PassageHit):
        path, title = hit.page_path, hit.page_title
    else:
        path, title = hit.path, hit.title
    parts = [part for part in (title, *hit.heading_path) if part]
    if not parts and path:
        parts = [path.rsplit("/", 1)[-1]]
    return " / ".join(parts) if parts else "memory hit"


def format_hit(hit: Hit) -> str:
    """Render one hit as a single context line for ``prefetch()``.

    Prefetch returns plain text, not structured data, so this is the only
    place the normalised record meets the model's context window.
    """
    if isinstance(hit, PassageHit):
        path, title = hit.page_path, hit.page_title
    else:
        path, title = hit.path, hit.title
    label = hit_label(hit)
    line = f"- {label} ({path})" if path and title else f"- {label}"
    score = display_score(hit)
    if score is not None:
        line += f" [score: {score:g}]"
    if hit.body:
        line += f": {hit.body}"
    return line


def format_result(result: QueryResult) -> str:
    """Render a whole result as newline-delimited context, or ``""`` if empty."""
    return "\n".join(format_hit(hit) for hit in result.hits)
