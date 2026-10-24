//! Buffered per-client activity counters, flushed to the store on an interval.

use super::*;

/// How long buffered client-activity counts may age before a background
/// task flushes them to the store. Counts inside the window are lost on
/// process exit; failed writes stay bounded in memory and retry.
pub(super) const CLIENT_ACTIVITY_FLUSH: std::time::Duration = std::time::Duration::from_secs(60);

pub(super) type ClientActivityEntry = (String, i64, u32, u32);

/// In-memory accumulation of per-client tool-call counts between
/// flushes. Keyed by `(client, utc_day)` so a flush that straddles
/// midnight books each call to the day it actually happened.
pub(super) struct ClientActivityBuffer {
    pub(super) pending: HashMap<(String, i64), (u32, u32)>,
    pub(super) flush_scheduled: bool,
}

impl ClientActivityBuffer {
    pub(super) fn new() -> Self {
        Self {
            pending: HashMap::new(),
            flush_scheduled: false,
        }
    }

    /// Record a delta and return whether the caller must start the sole
    /// background flusher for this shared buffer.
    pub(super) fn record(&mut self, client: String, day: i64, is_write: bool) -> bool {
        self.record_delta(client, day, u32::from(!is_write), u32::from(is_write));
        if self.flush_scheduled {
            false
        } else {
            self.flush_scheduled = true;
            true
        }
    }

    pub(super) fn record_delta(&mut self, client: String, day: i64, reads: u32, writes: u32) {
        let client = if client == CLIENT_ACTIVITY_OVERFLOW_CLIENT
            || self.pending.contains_key(&(client.clone(), day))
        {
            client
        } else {
            let named_clients = self
                .pending
                .keys()
                .filter(|(name, bucket_day)| {
                    *bucket_day == day && name.as_str() != CLIENT_ACTIVITY_OVERFLOW_CLIENT
                })
                .count();
            if named_clients < CLIENT_ACTIVITY_MAX_CLIENTS_PER_DAY {
                client
            } else {
                CLIENT_ACTIVITY_OVERFLOW_CLIENT.to_string()
            }
        };
        let slot = self.pending.entry((client, day)).or_insert((0, 0));
        slot.0 = slot.0.saturating_add(reads);
        slot.1 = slot.1.saturating_add(writes);
    }

    pub(super) fn take_entries(&mut self) -> Vec<ClientActivityEntry> {
        std::mem::take(&mut self.pending)
            .into_iter()
            .map(|((client, day), (reads, writes))| (client, day, reads, writes))
            .collect()
    }

    pub(super) fn restore_entries(&mut self, entries: Vec<ClientActivityEntry>) {
        for (client, day, reads, writes) in entries {
            self.record_delta(client, day, reads, writes);
        }
    }
}

pub(super) async fn flush_client_activity_loop(
    buffer: Arc<Mutex<ClientActivityBuffer>>,
    writer: WriterHandle,
    interval: Duration,
) {
    loop {
        tokio::time::sleep(interval).await;
        let entries = {
            let mut buffer = buffer
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if buffer.pending.is_empty() {
                buffer.flush_scheduled = false;
                None
            } else {
                Some(buffer.take_entries())
            }
        };
        let Some(entries) = entries else {
            return;
        };

        if let Err(error) = writer.bump_client_activity(entries.clone()).await {
            let mut buffer = buffer
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            buffer.restore_entries(entries);
            drop(buffer);
            tracing::warn!(%error, "client activity flush failed; retrying");
            continue;
        }

        let mut buffer = buffer
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if buffer.pending.is_empty() {
            buffer.flush_scheduled = false;
            return;
        }
    }
}

/// Whether an MCP tool mutates server state, for the reads/writes split
/// in `client_activity`. Anything unknown counts as a write: failing
/// toward "write" means a future tool added without updating this list
/// shows up as suspicious growth in the write column instead of being
/// silently misfiled as harmless reads.
pub(super) fn tool_call_is_write(tool: &str) -> bool {
    !matches!(
        tool,
        "memory_query"
            | "memory_read_page"
            | "memory_read_session_observations"
            | "memory_recent"
            | "memory_search_events"
            | "memory_briefing"
            | "memory_explore"
            | "memory_status"
            | "memory_install_self_routing"
    )
}

/// Trim, drop control characters, and cap an untrusted client name.
/// `None` when nothing printable remains.
pub(super) fn sanitize_client_name(raw: &str) -> Option<String> {
    let printable: String = raw
        .trim()
        .chars()
        .filter_map(|c| {
            if is_bidi_control(c) {
                None
            } else if c.is_whitespace() {
                Some(' ')
            } else if c.is_control() {
                None
            } else {
                Some(c)
            }
        })
        .collect();
    let cleaned: String = printable
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(CLIENT_ACTIVITY_MAX_NAME_CHARS)
        .collect();
    (!cleaned.is_empty()).then_some(cleaned)
}

pub(super) fn is_bidi_control(c: char) -> bool {
    matches!(
        c,
        '\u{061c}'
            | '\u{200e}'
            | '\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2066}'..='\u{2069}'
    )
}
