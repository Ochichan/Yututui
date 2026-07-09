use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

/// Resolved stream URLs are yt-dlp CDN URLs: useful for immediate skips, but not durable.
pub(in crate::app) const PREFETCH_TTL: Duration = Duration::from_secs(60 * 60);
const PREFETCH_MAX: usize = 64;

pub(in crate::app) struct ResolvedStream {
    url: String,
    inserted_at: Instant,
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

    fn insert_inner(&mut self, video_id: String, url: String, inserted_at: Instant) {
        self.prune_expired();
        self.order.retain(|existing| existing != &video_id);
        self.entries
            .insert(video_id.clone(), ResolvedStream { url, inserted_at });
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
        self.entries
            .get(video_id)
            .is_some_and(|entry| entry.inserted_at.elapsed() < PREFETCH_TTL)
    }

    fn touch(&mut self, video_id: &str) {
        self.order.retain(|existing| existing != video_id);
        self.order.push_back(video_id.to_owned());
    }

    fn prune_expired(&mut self) {
        let now = Instant::now();
        self.entries
            .retain(|_, entry| now.duration_since(entry.inserted_at) < PREFETCH_TTL);
        self.order
            .retain(|video_id| self.entries.contains_key(video_id));
    }
}
