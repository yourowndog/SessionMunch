use super::*;

fn stored(fm: serde_json::Value, body: &str) -> sessionmunch_store::StoredPageBody {
    sessionmunch_store::StoredPageBody {
        title: "T".into(),
        body: body.into(),
        frontmatter_json: fm.to_string(),
        tier: "semantic".into(),
        pinned: false,
    }
}

/// Two pages whose only difference is WHEN they were written are the
/// same content — the projection matches the store's idempotency rule.
#[test]
fn generated_at_alone_is_not_a_conflict() {
    let existing = stored(
        serde_json::json!({"title": "T", "generated": {"by": "x", "at": "2026-09-01T00:00:00Z"}}),
        "same",
    );
    let source = Markdown {
        frontmatter: serde_json::json!({"title": "T", "generated": {"by": "x", "at": "2026-09-02T00:00:00Z"}}),
        body: "same".into(),
    };
    assert!(!page_copy_differs(
        &existing,
        &source,
        "T",
        Tier::Semantic,
        false
    ));
}

#[test]
fn body_and_real_frontmatter_changes_still_conflict() {
    let existing = stored(serde_json::json!({"title": "T"}), "one");
    let body_diff = Markdown {
        frontmatter: serde_json::json!({"title": "T"}),
        body: "two".into(),
    };
    assert!(page_copy_differs(
        &existing,
        &body_diff,
        "T",
        Tier::Semantic,
        false
    ));
    let fm_diff = Markdown {
        frontmatter: serde_json::json!({"title": "T", "tags": ["x"]}),
        body: "one".into(),
    };
    assert!(page_copy_differs(
        &existing,
        &fm_diff,
        "T",
        Tier::Semantic,
        false
    ));
}
