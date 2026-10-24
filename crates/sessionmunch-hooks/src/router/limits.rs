//! Ingest admission limits: in-flight gates, project cache, subagent set and per-key rate limiting.

use super::*;

/// Resolved-project cache key:
/// `(cwd, workspace_override, project_override, project_strategy)`.
pub type ProjectCacheKey = (String, String, String, String);

/// Shared bounded resolved-project cache.
pub type ProjectCache = Arc<tokio::sync::Mutex<ProjectCacheStore>>;

pub(super) type IngestGate = tokio::sync::Mutex<()>;

pub(super) type IngestGateMap = HashMap<(ProjectId, String), Weak<IngestGate>>;

/// Per-key process gates prevent an overlapping retry from racing the original
/// delivery's downstream wiki/handoff effects.
#[derive(Clone, Default)]
pub struct IngestGates {
    entries: Arc<tokio::sync::Mutex<IngestGateMap>>,
}

impl IngestGates {
    pub(super) async fn lock(
        &self,
        project_id: ProjectId,
        ingest_key: &str,
    ) -> tokio::sync::OwnedMutexGuard<()> {
        let gate = {
            let mut entries = self.entries.lock().await;
            if entries.len() >= DEFAULT_INGEST_GATE_MAX_ENTRIES {
                entries.retain(|_, gate| gate.strong_count() > 0);
            }
            let map_key = (project_id, ingest_key.to_owned());
            if let Some(gate) = entries.get(&map_key).and_then(Weak::upgrade) {
                gate
            } else {
                let gate = Arc::new(IngestGate::new(()));
                entries.insert(map_key, Arc::downgrade(&gate));
                gate
            }
        };
        gate.lock_owned().await
    }
}

/// Bounded cwd-resolution cache used by the hook router.
#[derive(Debug)]
pub struct ProjectCacheStore {
    entries: HashMap<ProjectCacheKey, (WorkspaceId, ProjectId)>,
    order: VecDeque<ProjectCacheKey>,
    max_entries: usize,
}

impl Default for ProjectCacheStore {
    fn default() -> Self {
        Self::new(DEFAULT_PROJECT_CACHE_MAX_ENTRIES)
    }
}

impl ProjectCacheStore {
    #[must_use]
    pub(super) fn new(max_entries: usize) -> Self {
        Self {
            entries: HashMap::new(),
            order: VecDeque::new(),
            max_entries: max_entries.max(1),
        }
    }

    pub(super) fn get(&mut self, key: &ProjectCacheKey) -> Option<(WorkspaceId, ProjectId)> {
        let ids = self.entries.get(key).copied()?;
        self.touch(key);
        Some(ids)
    }

    pub(super) fn insert(&mut self, key: ProjectCacheKey, ids: (WorkspaceId, ProjectId)) {
        if self.entries.contains_key(&key) {
            self.entries.insert(key.clone(), ids);
            self.touch(&key);
            return;
        }
        self.entries.insert(key.clone(), ids);
        self.order.push_back(key);
        while self.entries.len() > self.max_entries {
            if let Some(oldest) = self.order.pop_front() {
                self.entries.remove(&oldest);
            } else {
                break;
            }
        }
    }

    pub(super) fn remove(&mut self, key: &ProjectCacheKey) {
        self.entries.remove(key);
        self.order.retain(|k| k != key);
    }

    #[must_use]
    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.entries.len()
    }

    #[must_use]
    #[cfg(test)]
    pub(super) fn contains_key(&self, key: &ProjectCacheKey) -> bool {
        self.entries.contains_key(key)
    }

    #[cfg(test)]
    pub(super) fn values(&self) -> impl Iterator<Item = &(WorkspaceId, ProjectId)> {
        self.entries.values()
    }

    /// Retain only cache entries that match `keep`.
    pub fn retain<F>(&mut self, mut keep: F)
    where
        F: FnMut(&ProjectCacheKey, &(WorkspaceId, ProjectId)) -> bool,
    {
        self.entries.retain(|key, ids| keep(key, ids));
        self.order.retain(|key| self.entries.contains_key(key));
    }

    pub(super) fn touch(&mut self, key: &ProjectCacheKey) {
        self.order.retain(|k| k != key);
        self.order.push_back(key.clone());
    }
}

/// Shared bounded set of scoped session keys known to belong to a SUBAGENT.
pub type SubagentSessions = Arc<tokio::sync::Mutex<SubagentSessionSet>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) struct SubagentSessionKey {
    pub(super) workspace_id: WorkspaceId,
    pub(super) project_id: ProjectId,
    pub(super) session_id: SessionId,
}

/// Tracks the scoped session keys of subagent (nested/spawned) sessions so that
/// the `drop_subagent_captures` gate can also drop the **unmarked tail** of those
/// sessions (`user_prompt_submit` / `stop` / `session_end`), which the
/// per-event marker (`subagentType` / `agent_type`) does not cover. A session
/// is seeded when a `SubagentStart` or any marker-bearing event arrives, and
/// forgotten on `SessionEnd` after the tail has been dropped. Bounded LRU so a
/// missed terminal event cannot leak memory.
#[derive(Debug)]
pub struct SubagentSessionSet {
    ids: HashSet<SubagentSessionKey>,
    order: VecDeque<SubagentSessionKey>,
    max: usize,
}

impl Default for SubagentSessionSet {
    fn default() -> Self {
        Self {
            ids: HashSet::new(),
            order: VecDeque::new(),
            max: SUBAGENT_SESSIONS_MAX,
        }
    }
}

impl SubagentSessionSet {
    /// Mark a scoped session id as a subagent (idempotent). Refreshes recency
    /// and evicts the oldest id once the cap is exceeded.
    pub(super) fn insert(&mut self, key: SubagentSessionKey) {
        if self.ids.contains(&key) {
            self.touch(&key);
            return;
        }
        self.ids.insert(key);
        self.order.push_back(key);
        while self.ids.len() > self.max {
            if let Some(oldest) = self.order.pop_front() {
                self.ids.remove(&oldest);
            } else {
                break;
            }
        }
    }

    /// Whether this scoped session id is a known subagent.
    #[must_use]
    pub(super) fn contains(&self, key: &SubagentSessionKey) -> bool {
        self.ids.contains(key)
    }

    /// Forget a scoped session id (after `SessionEnd`).
    pub(super) fn remove(&mut self, key: &SubagentSessionKey) {
        if self.ids.remove(key) {
            self.order.retain(|k| k != key);
        }
    }

    pub(super) fn touch(&mut self, key: &SubagentSessionKey) {
        self.order.retain(|k| k != key);
        self.order.push_back(*key);
    }
}

/// Cap on distinct rate-limiter keys held in memory. Bounded like
/// [`SubagentSessionSet`] so a stream of unique keys can't grow unbounded.
pub(super) const INGEST_RATE_MAX_KEYS: usize = 4096;

/// One token bucket: `tokens` refills at `refill_per_sec` up to `burst`.
pub(super) struct TokenBucket {
    tokens: f64,
    last_refill: std::time::Instant,
}

/// Per-source admission rate limiter. A bounded LRU of token buckets; disabled
/// (pass-through) when `refill_per_sec <= 0`.
pub struct IngestRateLimiter {
    buckets: HashMap<String, TokenBucket>,
    order: VecDeque<String>,
    max_keys: usize,
    refill_per_sec: f64,
    burst: f64,
}

impl IngestRateLimiter {
    /// A disabled (pass-through) limiter — the default for tests and installs
    /// that don't set the env knobs.
    #[must_use]
    pub fn disabled() -> Self {
        Self::new(0.0, 0.0)
    }

    /// `refill_per_sec` tokens/second per key, up to `burst` (min 1).
    #[must_use]
    pub fn new(refill_per_sec: f64, burst: f64) -> Self {
        Self {
            buckets: HashMap::new(),
            order: VecDeque::new(),
            max_keys: INGEST_RATE_MAX_KEYS,
            refill_per_sec,
            burst: burst.max(1.0),
        }
    }

    /// Try to admit one event for `key` at `now`. `true` when disabled or a
    /// token was available; `false` when the key is over its burst. O(1)
    /// amortized; evicts the oldest-inserted key over the cap (a re-inserted
    /// key just starts full again — a memory bound, not a fairness knob).
    pub fn try_take(&mut self, key: &str, now: std::time::Instant) -> bool {
        if self.refill_per_sec <= 0.0 {
            return true;
        }
        let key = bounded_rate_key(key);
        if !self.buckets.contains_key(&key) {
            self.buckets.insert(
                key.clone(),
                TokenBucket {
                    tokens: self.burst,
                    last_refill: now,
                },
            );
            self.order.push_back(key.clone());
            while self.buckets.len() > self.max_keys {
                if let Some(oldest) = self.order.pop_front() {
                    self.buckets.remove(&oldest);
                } else {
                    break;
                }
            }
        }
        let Some(bucket) = self.buckets.get_mut(&key) else {
            return true;
        };
        let elapsed = now
            .saturating_duration_since(bucket.last_refill)
            .as_secs_f64();
        bucket.tokens = (bucket.tokens + elapsed * self.refill_per_sec).min(self.burst);
        bucket.last_refill = now;
        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            true
        } else {
            false
        }
    }

    /// Number of tracked keys (bounded-ness test).
    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.buckets.len()
    }

    #[cfg(test)]
    pub(super) fn max_stored_key_len(&self) -> usize {
        self.buckets.keys().map(String::len).max().unwrap_or(0)
    }
}

pub(super) const INGEST_RATE_MAX_KEY_BYTES: usize = 128;

pub(super) fn bounded_rate_key(raw: &str) -> String {
    if raw.len() <= INGEST_RATE_MAX_KEY_BYTES {
        return raw.to_string();
    }
    format!("h:{:016x}", fnv1a64(raw.as_bytes()))
}

pub(super) fn log_rate_key(raw: &str) -> String {
    bounded_rate_key(raw)
        .chars()
        .map(|c| if c.is_control() { '_' } else { c })
        .collect()
}

pub(super) fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for b in bytes {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

pub(super) fn ingest_rate_key(env: &HookEnvelope, actor_user: Option<&str>) -> String {
    let user = actor_user.unwrap_or("");
    if let Some(session_id) = env.session_id.as_deref().filter(|s| !s.is_empty()) {
        return format!("u:{user}\ns:{session_id}");
    }
    let fallback = serde_json::to_string(&env.raw).unwrap_or_default();
    format!(
        "u:{user}\nmissing:{}:{}:{}:{}:{}:{}:{:016x}",
        env.agent.as_str(),
        format_args!("{:?}", env.event),
        env.cwd.as_deref().unwrap_or(""),
        env.workspace_override.as_deref().unwrap_or(""),
        env.project_override.as_deref().unwrap_or(""),
        env.project_strategy.as_str(),
        fnv1a64(fallback.as_bytes())
    )
}
