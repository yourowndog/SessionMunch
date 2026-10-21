//! Tool-schema dialects for clients (Gemini, Bedrock) that reject parts of JSON Schema.

use super::*;

/// Tool input-schema dialect served for one `tools/list`, ordered by
/// strictness: each variant applies every rewrite of the one before it, plus
/// its own. That ordering is what lets the operator's configured floor and a
/// request's `?flavor=` marker combine with a plain `max`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum SchemaDialect {
    /// The schemas `schemars` generated, verbatim.
    #[default]
    Upstream,
    /// Root-level `anyOf`/`oneOf`/`allOf` stripped (Moonshot, Bedrock).
    RootCombinators,
    /// Also collapses the nullable unions `Option<T>` produces into Google's
    /// single-`type` plus `nullable` form (Gemini / Vertex).
    GeminiSafe,
}

/// Bedrock and Moonshot reject root-level
/// `anyOf`/`oneOf`/`allOf` in tool parameter schemas with a 400 at
/// `tools/list` time. Kimi's legacy `?flavor=moonshot` and Kiro's
/// `?flavor=bedrock` both get schemas with those root keys stripped;
/// nested combinators stay, and runtime validation remains unchanged.
/// [`SchemaDialect::GeminiSafe`] strips the same root keys and additionally
/// rewrites every subschema through [`gemini_safe_schema`].
pub(super) fn restricted_schema_tool_list(tools: Vec<Tool>, dialect: SchemaDialect) -> Vec<Tool> {
    const ROOT_COMBINATORS: [&str; 3] = ["anyOf", "oneOf", "allOf"];
    let gemini_safe = dialect >= SchemaDialect::GeminiSafe;
    tools
        .into_iter()
        .map(|mut tool| {
            let has_root_combinator = ROOT_COMBINATORS
                .iter()
                .any(|key| tool.input_schema.contains_key(*key));
            if !has_root_combinator && !gemini_safe {
                return tool;
            }
            let mut schema = (*tool.input_schema).clone();
            for key in ROOT_COMBINATORS {
                schema.shift_remove(key);
            }
            if gemini_safe {
                gemini_safe_schema(&mut schema);
            }
            tool.input_schema = Arc::new(schema);
            tool
        })
        .collect()
}

/// Rewrite one subschema — and everything below it — into the subset Google's
/// `Schema` (Vertex/Gemini `functionDeclaration.parameters`) accepts.
///
/// `schemars` renders every `Option<T>` field as a union type, e.g.
/// `{"description": …, "type": ["integer", "null"], "format": "uint"}`. Google's
/// `Schema` takes a single `type`, so a converter that forwards our schema
/// verbatim turns the union into `any_of` and leaves `description` beside it —
/// which Vertex rejects outright ("specified other fields alongside any_of.
/// When using any_of, it must be the only field set"), failing the whole
/// session at `tools/list`. Gemini CLI never hits this because it performs the
/// same collapse client-side; OpenCode and other pass-through clients do.
///
/// Two rewrites, both keyed on what `schemars` actually emits, applied
/// bottom-up so a merged branch is already normalized:
/// [`collapse_nullable_type`] and [`flatten_sibling_combinator`]. Everything
/// else is left alone — Gemini CLI ships `$ref`, `$defs`, `title`, `const`, and
/// `format: "uint"` to Vertex untouched, so those are not part of the problem.
/// This mirrors [`sessionmunch_llm`]'s outbound `normalize_nullable_types`, which
/// solves the same mismatch for structured-output schemas.
///
/// Known limit, deliberate: a `$defs` entry can still carry a combinator with a
/// sibling `description` — `FeedbackKind` renders as a `oneOf` of `const`
/// branches — because a union of several real branches has no single-subschema
/// equivalent, and collapsing it to `enum` would drop the per-value docs a
/// working client receives today.
pub(super) fn gemini_safe_schema(schema: &mut serde_json::Map<String, serde_json::Value>) {
    for nested in schema.values_mut() {
        gemini_safe_value(nested);
    }
    collapse_nullable_type(schema);
    flatten_sibling_combinator(schema);
}

/// Recurse into whatever can hold a subschema: object values and array items
/// (`properties`, `items`, `anyOf` branches, `$defs`, …). Scalars are terminal.
pub(super) fn gemini_safe_value(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Array(items) => items.iter_mut().for_each(gemini_safe_value),
        serde_json::Value::Object(map) => gemini_safe_schema(map),
        _ => {}
    }
}

/// `type: [<t>, "null"]` becomes `type: <t>` plus `nullable: true`, and
/// `type: ["null"]` drops the type and keeps `nullable: true`.
///
/// A genuine multi-type union has no single-`type` equivalent, so it is left
/// for the client rather than silently narrowing the advertised contract;
/// `schemars` only emits the two-element form, for `Option<T>`.
pub(super) fn collapse_nullable_type(schema: &mut serde_json::Map<String, serde_json::Value>) {
    let Some(types) = schema.get("type").and_then(serde_json::Value::as_array) else {
        return;
    };
    if !types.iter().any(|entry| entry.as_str() == Some("null")) {
        return;
    }
    let mut non_null: Vec<serde_json::Value> = types
        .iter()
        .filter(|entry| entry.as_str() != Some("null"))
        .cloned()
        .collect();
    if non_null.len() > 1 {
        return;
    }
    match non_null.pop() {
        Some(single) => schema.insert("type".into(), single),
        None => schema.shift_remove("type"),
    };
    schema.insert("nullable".into(), serde_json::Value::Bool(true));
}

/// Google allows nothing beside `any_of`, so reconcile a combinator that
/// carries siblings: drop the `{"type": "null"}` branch `schemars` adds for
/// `Option<T>` and merge the lone survivor into the parent, where the parent's
/// own keys win so the field's doc comment survives.
///
/// Only that shape collapses cleanly. A combinator that is already the only key
/// is what Google wants, and a union of several real branches cannot be
/// expressed as one Google subschema at all — both are left untouched.
pub(super) fn flatten_sibling_combinator(schema: &mut serde_json::Map<String, serde_json::Value>) {
    const COMBINATORS: [&str; 2] = ["anyOf", "oneOf"];

    fn is_null_branch(branch: &serde_json::Value) -> bool {
        branch.as_object().is_some_and(|branch| {
            branch.len() == 1
                && branch.get("type").and_then(serde_json::Value::as_str) == Some("null")
        })
    }

    if schema.len() == 1 {
        return;
    }
    let Some(key) = COMBINATORS
        .into_iter()
        .find(|key| schema.contains_key(*key))
    else {
        return;
    };
    let Some(branches) = schema.get(key).and_then(serde_json::Value::as_array) else {
        return;
    };
    let kept: Vec<serde_json::Value> = branches
        .iter()
        .filter(|branch| !is_null_branch(branch))
        .cloned()
        .collect();
    let nullable = kept.len() < branches.len();
    let [serde_json::Value::Object(survivor)] = kept.as_slice() else {
        return;
    };
    let survivor = survivor.clone();
    schema.shift_remove(key);
    if nullable {
        schema.insert("nullable".into(), serde_json::Value::Bool(true));
    }
    for (field, value) in survivor {
        schema.entry(field).or_insert(value);
    }
    // The merged branch can itself carry a union type (`Option<Option<T>>`).
    collapse_nullable_type(schema);
}

/// The dialect a request's `?flavor=` marker asks for, if any. Several markers
/// resolve to the strictest one rather than to whichever came first.
pub(super) fn restricted_schema_flavor(query: &str) -> Option<SchemaDialect> {
    query
        .split('&')
        .filter_map(|pair| match pair {
            "flavor=moonshot" | "flavor=bedrock" => Some(SchemaDialect::RootCombinators),
            "flavor=gemini" | "flavor=vertex" => Some(SchemaDialect::GeminiSafe),
            _ => None,
        })
        .max()
}
