//! Canonical managed sessionmunch Agent Skill assets.
//!
//! The installer-facing crates consume these definitions so every path that
//! writes sessionmunch routing skills uses the same metadata and `SKILL.md` bytes.

/// Stable ownership marker embedded in every managed sessionmunch skill file.
pub const MANAGED_MARKER: &str = "<!-- sessionmunch-managed: routing-skill -->";

/// Pre-rename ownership marker. Recognized read-only so upgrades replace or
/// remove old-managed files instead of duplicating or orphaning them;
/// never written by SessionMunch (t_3f5184b0).
pub const LEGACY_MANAGED_MARKER: &str = "<!-- sessionmunch-managed: routing-skill -->";

/// Either generation of ownership marker.
#[must_use]
pub fn is_managed_skill_content(content: &str) -> bool {
    content.contains(MANAGED_MARKER) || content.contains(LEGACY_MANAGED_MARKER)
}

/// Claude-compatible Agent Skill directory below a project or home root.
pub const CLAUDE_SKILL_DIR: &str = ".claude";
/// AGENTS-aware cross-client Agent Skill directory below a project or home root.
pub const AGENTS_SKILL_DIR: &str = ".agents";
/// Devin-compatible Agent Skill directory below a project or home root.
pub const DEVIN_SKILL_DIR: &str = ".devin";
/// Grok Build CLI Agent Skill directory below a project or home root.
pub const GROK_SKILL_DIR: &str = ".grok";
/// Leaf directory that contains individual Agent Skill directories.
pub const SKILLS_DIR: &str = "skills";

const RETRIEVAL_DESCRIPTION: &str = "Use this skill for any request whose goal is read-only retrieval from sessionmunch: project history, prior context, decisions, rules, gotchas, recent activity, full wiki pages, or status/briefing. Trigger by semantic intent rather than exact wording, including when sessionmunch is not named.";
const HANDOFF_DESCRIPTION: &str = "Use this skill for any request whose goal is session continuity across agents or time: finding a pending handoff, resuming previous work, saving next-session context, wrapping up, or discarding a mistaken handoff. Trigger by semantic intent rather than exact wording.";
const DURABLE_PAGES_DESCRIPTION: &str = "Use this skill for any explicit wiki mutation in sessionmunch: saving durable or time-bounded project knowledge, recording a rule or annotation, updating a note, or deleting a memory page. Trigger by semantic intent rather than exact wording; routine session capture is not a durable-page request.";
const LEARNING_MAINTENANCE_DESCRIPTION: &str = "Use this skill for any sessionmunch knowledge-base maintenance request: consolidating observations, reviewing session lessons, proposing durable learnings, auditing or linting the wiki, finding contradictions, pruning stale memory, or running auto-improvement. Trigger by semantic intent rather than exact wording.";
const ROUTING_INSTALL_DESCRIPTION: &str = "Use this skill for any request to install, refresh, repair, inspect, or remove sessionmunch's agent-facing routing: managed instruction snippets, Agent Skills, CLAUDE.md/AGENTS.md integration, or local/global skill roots. Trigger by semantic intent rather than exact wording.";

/// One sessionmunch-managed Agent Skill file bundled by the core crate.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ManagedSkill {
    /// Skill directory name and frontmatter `name` value.
    pub name: &'static str,
    /// Trigger-rich frontmatter `description` value.
    pub description: &'static str,
    /// File path relative to an agent skill root.
    pub relative_path: &'static str,
    /// Complete `SKILL.md` contents.
    pub content: &'static str,
}

/// Canonical managed sessionmunch routing skills.
pub const MANAGED_SKILLS: &[ManagedSkill] = &[
    ManagedSkill {
        name: "sessionmunch-retrieval",
        description: RETRIEVAL_DESCRIPTION,
        relative_path: "sessionmunch-retrieval/SKILL.md",
        content: include_str!("routing_skills/sessionmunch-retrieval/SKILL.md"),
    },
    ManagedSkill {
        name: "sessionmunch-handoff",
        description: HANDOFF_DESCRIPTION,
        relative_path: "sessionmunch-handoff/SKILL.md",
        content: include_str!("routing_skills/sessionmunch-handoff/SKILL.md"),
    },
    ManagedSkill {
        name: "sessionmunch-durable-pages",
        description: DURABLE_PAGES_DESCRIPTION,
        relative_path: "sessionmunch-durable-pages/SKILL.md",
        content: include_str!("routing_skills/sessionmunch-durable-pages/SKILL.md"),
    },
    ManagedSkill {
        name: "sessionmunch-learning-maintenance",
        description: LEARNING_MAINTENANCE_DESCRIPTION,
        relative_path: "sessionmunch-learning-maintenance/SKILL.md",
        content: include_str!("routing_skills/sessionmunch-learning-maintenance/SKILL.md"),
    },
    ManagedSkill {
        name: "sessionmunch-routing-install",
        description: ROUTING_INSTALL_DESCRIPTION,
        relative_path: "sessionmunch-routing-install/SKILL.md",
        content: include_str!("routing_skills/sessionmunch-routing-install/SKILL.md"),
    },
];

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::ffi::OsStr;
    use std::path::{Component, Path};

    use super::{MANAGED_MARKER, MANAGED_SKILLS, ManagedSkill};

    const EXPECTED_SKILLS: &[&str] = &[
        "sessionmunch-retrieval",
        "sessionmunch-handoff",
        "sessionmunch-durable-pages",
        "sessionmunch-learning-maintenance",
        "sessionmunch-routing-install",
    ];

    const EXPECTED_TOOL_CLUSTERS: &[(&str, &str)] = &[
        ("memory_query", "sessionmunch-retrieval"),
        ("memory_recent", "sessionmunch-retrieval"),
        ("memory_read_page", "sessionmunch-retrieval"),
        ("memory_read_session_observations", "sessionmunch-retrieval"),
        ("memory_status", "sessionmunch-retrieval"),
        ("memory_briefing", "sessionmunch-retrieval"),
        ("memory_explore", "sessionmunch-retrieval"),
        ("memory_handoff_accept", "sessionmunch-handoff"),
        ("memory_handoff_begin", "sessionmunch-handoff"),
        ("memory_handoff_cancel", "sessionmunch-handoff"),
        ("memory_handoff_list", "sessionmunch-handoff"),
        ("memory_write_page", "sessionmunch-durable-pages"),
        ("memory_delete_page", "sessionmunch-durable-pages"),
        ("memory_consolidate", "sessionmunch-learning-maintenance"),
        ("memory_auto_improve", "sessionmunch-learning-maintenance"),
        ("memory_lint", "sessionmunch-learning-maintenance"),
        ("memory_forget_sweep", "sessionmunch-learning-maintenance"),
        ("memory_install_self_routing", "sessionmunch-routing-install"),
    ];
    const PROJECT_SCOPED_SKILLS: &[&str] = &[
        "sessionmunch-retrieval",
        "sessionmunch-handoff",
        "sessionmunch-durable-pages",
        "sessionmunch-learning-maintenance",
    ];

    #[derive(Debug, serde::Deserialize)]
    struct Frontmatter {
        name: String,
        description: String,
    }

    #[test]
    fn exposes_exact_managed_skill_set() {
        let names: Vec<_> = MANAGED_SKILLS.iter().map(|skill| skill.name).collect();
        assert_eq!(names, EXPECTED_SKILLS);
    }

    #[test]
    fn skill_frontmatter_is_valid_and_matches_metadata() {
        for skill in MANAGED_SKILLS {
            let frontmatter = parse_frontmatter(skill);
            assert_eq!(frontmatter.name, skill.name);
            assert_eq!(frontmatter.description, skill.description);
            assert_eq!(frontmatter.name, directory_name(skill));
            assert!(!frontmatter.description.trim().is_empty());
            assert!(
                frontmatter.description.chars().count() <= 1024,
                "{} description is over the Agent Skills limit",
                skill.name
            );
        }
    }

    #[test]
    fn every_skill_has_managed_marker() {
        assert!(!MANAGED_MARKER.is_empty());
        for skill in MANAGED_SKILLS {
            assert!(
                skill.content.contains(MANAGED_MARKER),
                "{} is missing the managed ownership marker",
                skill.name
            );
        }
    }

    #[test]
    fn managed_skill_payloads_use_lf_line_endings() {
        for skill in MANAGED_SKILLS {
            assert!(
                !skill.content.contains('\r'),
                "{} must use LF line endings",
                skill.name
            );
            assert!(
                skill.content.ends_with('\n'),
                "{} must end with a newline",
                skill.name
            );
        }
    }

    #[test]
    fn relative_paths_are_safe_relative_skill_markdown_files() {
        for skill in MANAGED_SKILLS {
            let expected_suffix = format!("{}/SKILL.md", skill.name);
            assert_eq!(skill.relative_path, expected_suffix);
            assert!(
                !skill.name.contains(['/', '\\']),
                "{} must be a single skill directory name",
                skill.name
            );

            let path = Path::new(skill.relative_path);
            assert!(
                !path.is_absolute(),
                "{} relative_path must not be absolute",
                skill.name
            );
            let components: Vec<_> = path.components().collect();
            assert_eq!(
                components,
                vec![
                    Component::Normal(OsStr::new(skill.name)),
                    Component::Normal(OsStr::new("SKILL.md")),
                ],
                "{} relative_path must be exactly <skill>/SKILL.md with no parent/current/root components",
                skill.name
            );
        }
    }

    #[test]
    fn every_routing_tool_appears_only_in_its_intended_cluster() {
        let expected_by_tool: BTreeMap<_, _> = EXPECTED_TOOL_CLUSTERS.iter().copied().collect();
        assert_eq!(expected_by_tool.len(), EXPECTED_TOOL_CLUSTERS.len());

        for (tool, expected_skill) in expected_by_tool {
            let containing_skills: Vec<_> = MANAGED_SKILLS
                .iter()
                .filter(|skill| skill.content.contains(tool))
                .map(|skill| skill.name)
                .collect();

            assert_eq!(
                containing_skills,
                vec![expected_skill],
                "{tool} should appear in exactly one intended skill"
            );
        }
    }

    #[test]
    fn project_scoped_skills_share_the_static_client_contract() {
        for skill_name in PROJECT_SCOPED_SKILLS {
            let skill = MANAGED_SKILLS
                .iter()
                .find(|skill| skill.name == *skill_name)
                .unwrap_or_else(|| panic!("missing managed skill {skill_name}"));
            for required in [
                "Session-aware MCP clients",
                "Static MCP clients",
                "must pass `workspace` and `project` together on every project-scoped call",
                "nearest `.sessionmunch.toml`",
                "never guess them from a directory name",
                "never rely on the server's last active project",
                "`global=true` must omit `workspace`, `project`, and `scopes`",
                "`scope: \"global\"`",
            ] {
                assert!(
                    skill.content.contains(required),
                    "{skill_name} is missing scope guidance: {required}"
                );
            }
            for contradictory in [
                "Pass workspace and project together only when",
                "Never pass scope arguments",
                "omit project, workspace, and cwd arguments unless",
            ] {
                assert!(
                    !skill.content.contains(contradictory),
                    "{skill_name} contains contradictory scope guidance: {contradictory}"
                );
            }
        }
    }

    fn parse_frontmatter(skill: &ManagedSkill) -> Frontmatter {
        let Some(rest) = skill.content.strip_prefix("---\n") else {
            panic!("{} must start with frontmatter", skill.name);
        };
        let Some((frontmatter, _body)) = rest.split_once("\n---\n") else {
            panic!("{} must close frontmatter", skill.name);
        };
        serde_yaml::from_str(frontmatter)
            .unwrap_or_else(|e| panic!("{} frontmatter must be valid YAML: {e}", skill.name))
    }

    fn directory_name(skill: &ManagedSkill) -> &str {
        skill
            .relative_path
            .split('/')
            .next()
            .unwrap_or_else(|| panic!("{} has an empty relative path", skill.name))
    }
}
