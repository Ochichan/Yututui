use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

/// Resolved stream URLs are yt-dlp CDN URLs: useful for immediate skips, but not durable.
pub(in crate::app) const PREFETCH_TTL: Duration = Duration::from_secs(30 * 60);
pub(in crate::app) const PREFETCH_FAILURE_WINDOW: Duration = Duration::from_secs(10 * 60);
pub(in crate::app) const PREFETCH_DISABLE_DURATION: Duration = Duration::from_secs(30 * 60);
const PREFETCH_FAILURE_THRESHOLD: usize = 2;
const PREFETCH_MAX: usize = 64;

pub(in crate::app) struct ResolvedStream {
    url: String,
    inserted_at: Instant,
    #[cfg(test)]
    force_expired: bool,
}

#[derive(Default)]
pub struct PrefetchCache {
    entries: HashMap<String, ResolvedStream>,
    /// Most-recently-used last.
    order: VecDeque<String>,
}

impl PrefetchCache {
    pub(in crate::app) fn insert(&mut self, video_id: String, url: String) {
        self.insert_at(video_id, url, Instant::now());
    }

    pub(in crate::app) fn get_fresh_url(&mut self, video_id: &str) -> Option<String> {
        if !self.is_fresh(video_id) {
            self.remove(video_id);
            return None;
        }
        self.touch(video_id);
        self.entries.get(video_id).map(|entry| entry.url.clone())
    }

    pub(in crate::app) fn contains_fresh(&mut self, video_id: &str) -> bool {
        if !self.is_fresh(video_id) {
            self.remove(video_id);
            return false;
        }
        self.touch(video_id);
        true
    }

    pub(in crate::app) fn remove(&mut self, video_id: &str) {
        self.entries.remove(video_id);
        self.order.retain(|existing| existing != video_id);
    }

    pub(in crate::app) fn clear(&mut self) {
        self.entries.clear();
        self.order.clear();
    }

    #[cfg(test)]
    pub(in crate::app) fn len(&self) -> usize {
        self.entries.len()
    }

    #[cfg(test)]
    pub(in crate::app) fn insert_at(
        &mut self,
        video_id: String,
        url: String,
        inserted_at: Instant,
    ) {
        self.insert_inner(video_id, url, inserted_at);
    }

    #[cfg(not(test))]
    fn insert_at(&mut self, video_id: String, url: String, inserted_at: Instant) {
        self.insert_inner(video_id, url, inserted_at);
    }

    #[cfg(test)]
    pub(in crate::app) fn insert_expired(&mut self, video_id: String, url: String) {
        self.insert_inner(video_id.clone(), url, Instant::now());
        if let Some(entry) = self.entries.get_mut(&video_id) {
            entry.force_expired = true;
        }
    }

    fn insert_inner(&mut self, video_id: String, url: String, inserted_at: Instant) {
        self.prune_expired();
        self.order.retain(|existing| existing != &video_id);
        self.entries.insert(
            video_id.clone(),
            ResolvedStream {
                url,
                inserted_at,
                #[cfg(test)]
                force_expired: false,
            },
        );
        self.order.push_back(video_id);
        while self.entries.len() > PREFETCH_MAX {
            if let Some(oldest) = self.order.pop_front() {
                self.entries.remove(&oldest);
            } else {
                break;
            }
        }
    }

    fn is_fresh(&self, video_id: &str) -> bool {
        let now = Instant::now();
        self.entries
            .get(video_id)
            .is_some_and(|entry| entry.is_fresh_at(now))
    }

    fn touch(&mut self, video_id: &str) {
        self.order.retain(|existing| existing != video_id);
        self.order.push_back(video_id.to_owned());
    }

    fn prune_expired(&mut self) {
        let now = Instant::now();
        self.entries.retain(|_, entry| entry.is_fresh_at(now));
        self.order
            .retain(|video_id| self.entries.contains_key(video_id));
    }
}

impl ResolvedStream {
    fn is_fresh_at(&self, now: Instant) -> bool {
        #[cfg(test)]
        if self.force_expired {
            return false;
        }

        now.checked_duration_since(self.inserted_at)
            .is_some_and(|age| age < PREFETCH_TTL)
    }
}

impl super::Prefetch {
    /// Whether ordinary skip-ahead prefetch is currently allowed. This also expires the session-only
    /// cooldown once its deadline passes.
    pub(in crate::app) fn enabled(&mut self) -> bool {
        self.enabled_at(Instant::now())
    }

    /// Record that a prefetched direct URL was loaded into mpv and failed. Returns true when this
    /// failure crosses the threshold and pauses ordinary prefetch for the cooldown period.
    pub(in crate::app) fn record_direct_url_failure(&mut self) -> bool {
        self.record_direct_url_failure_at(Instant::now())
    }

    fn enabled_at(&mut self, now: Instant) -> bool {
        if let Some(until) = self.disabled_until {
            if now < until {
                return false;
            }
            self.disabled_until = None;
            self.recent_failures.clear();
        }
        true
    }

    fn record_direct_url_failure_at(&mut self, now: Instant) -> bool {
        self.enabled_at(now);
        self.recent_failures.retain(|at| {
            now.checked_duration_since(*at)
                .is_some_and(|age| age <= PREFETCH_FAILURE_WINDOW)
        });
        self.recent_failures.push_back(now);
        if self.recent_failures.len() < PREFETCH_FAILURE_THRESHOLD {
            return false;
        }

        self.recent_failures.clear();
        self.disabled_until = now.checked_add(PREFETCH_DISABLE_DURATION);
        self.resolved.clear();
        true
    }
}
