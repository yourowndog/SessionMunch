//! Managed-run (`sessionmunch run`) status, context and workstream event reads.

use super::*;

impl ReaderPool {
    /// Return the current state of one `sessionmunch run` invocation.
    pub async fn managed_run_status(
        &self,
        run_id: ManagedRunId,
    ) -> StoreResult<Option<StoredManagedRunStatus>> {
        self.with_conn(move |conn| crate::workstream::run_status(conn, run_id))
            .await
    }

    /// Return the unseen portable range assigned to a managed SessionStart.
    pub async fn managed_run_context(
        &self,
        run_id: ManagedRunId,
        max_events: usize,
    ) -> StoreResult<Option<ManagedRunContext>> {
        self.with_conn(move |conn| crate::workstream::run_context(conn, run_id, max_events))
            .await
    }

    /// Search visible events in one managed workstream, or return its newest
    /// events when `query` is empty.
    pub async fn search_workstream_events(
        &self,
        workstream_id: WorkstreamId,
        query: String,
        limit: usize,
    ) -> StoreResult<Vec<WorkstreamEvent>> {
        self.with_conn(move |conn| {
            crate::workstream::search_events(conn, workstream_id, &query, limit)
        })
        .await
    }

    /// List recent managed workstreams for one exact repository/worktree.
    pub async fn recent_workstreams(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        repo_fingerprint: String,
        worktree_fingerprint: String,
        limit: usize,
    ) -> StoreResult<Vec<StoredWorkstreamSummary>> {
        self.with_conn(move |conn| {
            crate::workstream::list_recent(
                conn,
                workspace_id,
                project_id,
                &repo_fingerprint,
                &worktree_fingerprint,
                limit,
            )
        })
        .await
    }
}
