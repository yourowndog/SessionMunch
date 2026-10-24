//! Single binary for this crate's integration tests.
//!
//! Every file in this directory is a module of this one test binary: one
//! link per rebuild instead of one per file. Cargo treats `tests/suite/main.rs`
//! as the single `suite` target and never builds the sibling files on their own,
//! so a new file must be declared here (`scripts/check-test-suites.*` enforces it).

mod autoscope_env;
mod backfill_sections;
// Test setup may read the real environment; invariant #1 governs runtime code.
#[allow(clippy::disallowed_methods)]
mod branding;
mod completions;
// Test setup may read the real environment; invariant #1 governs runtime code.
#[allow(clippy::disallowed_methods)]
mod history_import_e2e;
mod hook_drain;
mod hook_payload;
// Test setup may read the real environment; invariant #1 governs runtime code.
#[allow(clippy::disallowed_methods)]
mod legacy_import_e2e;
mod marker_scope;
// Test setup may read the real environment; invariant #1 governs runtime code.
#[allow(clippy::disallowed_methods)]
mod packaging;
mod removal;
mod repo_layout;
mod routing_instructions;
mod routing_skills;
mod serve_shutdown;
mod shutdown_signals;
mod zero_telemetry;
