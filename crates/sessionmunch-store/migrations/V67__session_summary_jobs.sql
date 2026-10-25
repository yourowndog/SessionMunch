-- V67: cache of accepted AI session-summary job answers.
--
-- Optional AI summaries of session cards run as jobs keyed by a stable job
-- id (session, scope, builder version, prompt version, input hash). An
-- accepted answer is kept here so an unchanged session makes no provider
-- calls, even after its card page was rewritten. The rows are derived data:
-- they belong to their session and go with it.

CREATE TABLE session_summary_jobs (
    session_id  BLOB NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    job_id      TEXT NOT NULL,
    model       TEXT NOT NULL,
    fields_json TEXT NOT NULL,
    created_at  INTEGER NOT NULL,
    PRIMARY KEY (session_id, job_id)
);
