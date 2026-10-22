//! `memory_explore`: gap detection and the LLM request it builds.

use super::*;

/// Description of how long it's been since the last observation.
/// `memory_explore` uses this both to size its prompt verbosity and
/// to give the LLM an explicit "time gap is N hours" cue.
#[derive(Debug, Serialize)]
pub(super) struct ExploreGap {
    /// Hours since the last observation, or `None` if nothing has
    /// ever been observed for this project.
    pub(super) hours_since_last: Option<f64>,
    /// Coarse bucket name used to drive the prompt:
    /// `none` — no prior activity at all.
    /// `fresh` — last observation < 1 h ago.
    /// `today` — < 24 h ago.
    /// `recent` — < 7 days ago.
    /// `dormant` — < 30 days ago.
    /// `stale` — > 30 days ago.
    pub(super) bucket: &'static str,
    /// Plain-English description for the LLM prompt.
    pub(super) description: String,
}

pub(super) fn explore_gap_from_snapshot(s: &sessionmunch_store::BriefingSnapshot) -> ExploreGap {
    let Some(last) = s.last_observation_at.as_deref() else {
        return ExploreGap {
            hours_since_last: None,
            bucket: "none",
            description: "no prior activity recorded for this project".into(),
        };
    };
    let Ok(last_ts) = last.parse::<jiff::Timestamp>() else {
        return ExploreGap {
            hours_since_last: None,
            bucket: "none",
            description: format!("last observation timestamp unparseable: {last}"),
        };
    };
    let delta_us = jiff::Timestamp::now().as_microsecond() - last_ts.as_microsecond();
    let hours = (delta_us as f64) / 1_000_000.0 / 3600.0;
    let (bucket, description) = if hours < 1.0 {
        (
            "fresh",
            format!("{:.1} minutes since last observation", hours * 60.0),
        )
    } else if hours < 24.0 {
        ("today", format!("{hours:.1} hours since last observation"))
    } else if hours < 24.0 * 7.0 {
        (
            "recent",
            format!("{:.1} days since last observation", hours / 24.0),
        )
    } else if hours < 24.0 * 30.0 {
        (
            "dormant",
            format!("{:.1} days since last observation", hours / 24.0),
        )
    } else {
        (
            "stale",
            format!("{:.1} days since last observation", hours / 24.0),
        )
    };
    ExploreGap {
        hours_since_last: Some(hours),
        bucket,
        description,
    }
}

/// Build the ChatRequest for `memory_explore`. The user message
/// inlines the entire briefing as JSON — small enough (a few KB) that
/// model context is not a concern. The system prompt + the gap
/// bucket together steer verbosity.
pub(super) fn build_explore_request(
    snapshot: &sessionmunch_store::BriefingSnapshot,
    gap: &ExploreGap,
    focus: Option<&str>,
) -> sessionmunch_llm::ChatRequest {
    let snapshot_json = serde_json::to_string_pretty(snapshot).unwrap_or_else(|_| "{}".into());
    let mut user = String::new();
    user.push_str("## Project state snapshot\n\n");
    user.push_str("```json\n");
    user.push_str(&snapshot_json);
    user.push_str("\n```\n\n");
    user.push_str(&format!(
        "## Time gap\n\nBucket: `{}` — {}.\n\n",
        gap.bucket, gap.description
    ));
    if let Some(focus) = focus {
        user.push_str("## Focus\n\nThe user is specifically interested in: ");
        user.push_str(focus);
        user.push_str("\n\nBias the digest toward this topic while still covering anything urgent (pending handoffs, recently-changed rules).\n");
    }
    sessionmunch_llm::ChatRequest {
        system: Some(EXPLORE_SYSTEM_PROMPT.into()),
        messages: vec![sessionmunch_llm::ChatMessage {
            role: sessionmunch_llm::Role::User,
            content: user,
        }],
        // memory_explore returns prose, not JSON, so a truncated
        // response is degraded but not unparseable. Still generous
        // so the long `dormant`/`stale` digests don't get cut off.
        max_tokens: 16_000,
        temperature: Some(0.2),
    }
}

/// System prompt for `memory_explore`. Loaded at compile time from
/// `prompts/explore_system.md`.
pub(super) const EXPLORE_SYSTEM_PROMPT: &str = include_str!("../../prompts/explore_system.md");
