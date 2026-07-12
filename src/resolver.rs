//! Stream prefetch: resolve a track's direct audio URL ahead of time so skipping to it
//! is instant (priority #2).
//!
//! Normally mpv resolves a `watch?v=…` URL itself via its bundled yt-dlp hook on load —
//! ~2-3 s. By pre-resolving the *next* track with `yt-dlp -g` while the current one plays,
//! a skip hands mpv an already-resolved CDN URL it can open immediately (~100 ms). The
//! resolved URL is the app's; staleness (yt-dlp URLs are ~6 h, IP-bound) only matters for
//! sessions longer than that, where mpv falls back to a fresh resolve on error.

use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::sync::watch;
use tokio::task::{JoinHandle, JoinSet};

use crate::ids::{StreamUrl, VideoId, WatchUrl};
use crate::util::backpressure::{QueuePolicy, RESOLVER_QUEUE, bounded_channel};
use crate::util::delivery::{DeliveryError, DeliveryReceipt, DeliveryResult};
use crate::util::process;

/// Most concurrent resolves (we only look one ahead, but `prev`/retries can overlap).
const MAX_CONCURRENT: usize = 2;
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(20);
const RESOLVE_STDOUT_MAX: usize = 16 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ResolvePurpose {
    Prefetch,
    SelfHeal,
}

pub enum ResolveCmd {
    Resolve {
        video_id: VideoId,
        watch_url: WatchUrl,
        purpose: ResolvePurpose,
    },
}

impl ResolveCmd {
    fn watch_url(&self) -> &WatchUrl {
        match self {
            Self::Resolve { watch_url, .. } => watch_url,
        }
    }

    fn purpose(&self) -> ResolvePurpose {
        match self {
            Self::Resolve { purpose, .. } => *purpose,
        }
    }

    fn merge_latest(&mut self, latest: Self) -> bool {
        let merged_purpose = self.purpose().max(latest.purpose());
        let replaced_existing =
            self.watch_url() != latest.watch_url() || self.purpose() != merged_purpose;
        if replaced_existing {
            let Self::Resolve {
                video_id,
                watch_url,
                ..
            } = latest;
            *self = Self::Resolve {
                video_id,
                watch_url,
                purpose: merged_purpose,
            };
        }
        replaced_existing
    }
}

pub enum ResolverEvent {
    Resolved {
        video_id: VideoId,
        stream_url: StreamUrl,
        purpose: ResolvePurpose,
    },
    /// Resolution failed (yt-dlp error/timeout). Consumed by the playback self-heal's
    /// retry (which must not hang on a silent failure); plain prefetch listeners
    /// ignore it — a missed prefetch only means a slower skip later.
    Failed {
        video_id: VideoId,
        purpose: ResolvePurpose,
    },
}

type EventSink = Arc<dyn Fn(ResolverEvent) + Send + Sync>;

pub struct ResolverHandle {
    /// The bounded channel carries only semantic keys. The latest command for each key
    /// lives in `queue`, so duplicate prefetches replace one pending value without
    /// consuming another channel slot.
    tx: mpsc::Sender<VideoId>,
    queue: Arc<Mutex<ResolverQueueState>>,
    shutdown_tx: Option<watch::Sender<bool>>,
    actor_task: Option<JoinHandle<()>>,
}

impl ResolverHandle {
    pub fn resolve(&self, video_id: String, watch_url: String) -> DeliveryResult {
        self.resolve_with_purpose(video_id, watch_url, ResolvePurpose::Prefetch)
    }

    /// Force a self-heal resolve to start after any ordinary prefetch already in flight.
    /// The tool updater may have replaced yt-dlp without changing the watch URL, so an
    /// identical active prefetch cannot satisfy this request.
    pub fn resolve_for_self_heal(&self, video_id: String, watch_url: String) -> DeliveryResult {
        self.resolve_with_purpose(video_id, watch_url, ResolvePurpose::SelfHeal)
    }

    fn resolve_with_purpose(
        &self,
        video_id: String,
        watch_url: String,
        purpose: ResolvePurpose,
    ) -> DeliveryResult {
        let video_id = VideoId::from(video_id);
        let cmd = ResolveCmd::Resolve {
            video_id: video_id.clone(),
            watch_url: WatchUrl::from(watch_url),
            purpose,
        };

        let mut queue = self
            .queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !queue.accepting || self.tx.is_closed() {
            close_resolver_admission(&mut queue);
            return Err(DeliveryError::Closed);
        }
        if let Some(in_flight) = queue.in_flight.get(&video_id).cloned() {
            if let Some(mut rerun) = queue.rerun_after_flight.remove(&video_id) {
                let required_purpose = rerun.purpose().max(cmd.purpose());
                let active_satisfies_latest = in_flight.watch_url.as_str()
                    == cmd.watch_url().as_str()
                    && in_flight.purpose >= required_purpose;
                let replaced_existing = if active_satisfies_latest {
                    true
                } else {
                    let replaced_existing = rerun.merge_latest(cmd);
                    queue.rerun_after_flight.insert(video_id, rerun);
                    replaced_existing
                };
                return Ok(DeliveryReceipt::Coalesced {
                    replaced_existing,
                    evicted_oldest: false,
                });
            }
            let active_satisfies = in_flight.watch_url.as_str() == cmd.watch_url().as_str()
                && in_flight.purpose >= cmd.purpose();
            if active_satisfies {
                return Ok(DeliveryReceipt::Coalesced {
                    replaced_existing: false,
                    evicted_oldest: false,
                });
            }
            if queue.queued_len() >= queue.capacity {
                return Err(DeliveryError::Busy);
            }

            queue.rerun_after_flight.insert(video_id, cmd);
            return Ok(DeliveryReceipt::Deferred);
        }
        if let Some(pending) = queue.pending.get_mut(&video_id) {
            let replaced_existing = pending.merge_latest(cmd);
            return Ok(DeliveryReceipt::Coalesced {
                replaced_existing,
                evicted_oldest: false,
            });
        }
        if queue.queued_len() >= queue.capacity {
            return Err(DeliveryError::Busy);
        }

        queue.pending.insert(video_id.clone(), cmd);
        match self.tx.try_send(video_id.clone()) {
            Ok(()) => Ok(DeliveryReceipt::Enqueued),
            Err(mpsc::error::TrySendError::Full(_)) => {
                // `pending` and the key channel grow in lockstep, so this should only be
                // reachable if their contract is changed independently. Roll back rather
                // than leave an entry that can never be observed by the actor.
                queue.pending.remove(&video_id);
                Err(DeliveryError::Busy)
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                queue.pending.remove(&video_id);
                Err(DeliveryError::Closed)
            }
        }
    }

    /// Close admission, cancel every accepted resolve, and join the actor which owns them.
    ///
    /// The join handle remains in `self` while it is awaited, so cancelling this future does not
    /// detach the actor; a later call can finish the join and `Drop` retains an abort fallback.
    pub async fn shutdown(&mut self) -> bool {
        self.close_admission();
        if let Some(shutdown_tx) = &self.shutdown_tx {
            let _ = shutdown_tx.send(true);
        }

        let Some(actor_task) = self.actor_task.as_mut() else {
            return true;
        };
        let joined = (&mut *actor_task).await;
        self.actor_task.take();
        match joined {
            Ok(()) => true,
            Err(error) => {
                tracing::warn!(%error, "resolver actor failed during shutdown");
                false
            }
        }
    }

    fn close_admission(&self) {
        let mut queue = self
            .queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        close_resolver_admission(&mut queue);
    }
}

impl Drop for ResolverHandle {
    fn drop(&mut self) {
        self.close_admission();
        if let Some(shutdown_tx) = &self.shutdown_tx {
            let _ = shutdown_tx.send(true);
        }
        if let Some(actor_task) = self.actor_task.take() {
            actor_task.abort();
        }
    }
}

struct ResolverQueueState {
    capacity: usize,
    accepting: bool,
    pending: HashMap<VideoId, ResolveCmd>,
    in_flight: HashMap<VideoId, InFlightResolve>,
    /// At most one changed payload per active semantic key. It does not receive a
    /// channel token until the current resolve finishes, so the same key cannot run
    /// concurrently with itself.
    rerun_after_flight: HashMap<VideoId, ResolveCmd>,
}

fn close_resolver_admission(queue: &mut ResolverQueueState) {
    queue.accepting = false;
    queue.pending.clear();
    queue.rerun_after_flight.clear();
}

#[derive(Clone)]
struct InFlightResolve {
    watch_url: WatchUrl,
    purpose: ResolvePurpose,
}

impl ResolverQueueState {
    fn queued_len(&self) -> usize {
        self.pending.len() + self.rerun_after_flight.len()
    }
}

struct ResolverInbox {
    rx: mpsc::Receiver<VideoId>,
    tx: mpsc::Sender<VideoId>,
    queue: Arc<Mutex<ResolverQueueState>>,
}

impl ResolverInbox {
    async fn recv(&mut self) -> Option<ResolveCmd> {
        while let Some(video_id) = self.rx.recv().await {
            let mut queue = self
                .queue
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(cmd) = queue.pending.remove(&video_id) {
                queue.in_flight.insert(
                    video_id,
                    InFlightResolve {
                        watch_url: cmd.watch_url().clone(),
                        purpose: cmd.purpose(),
                    },
                );
                return Some(cmd);
            }
        }
        None
    }

    #[cfg(test)]
    fn finish(&self, video_id: &VideoId) -> bool {
        finish_resolve(&self.queue, &self.tx, video_id)
    }
}

struct InFlightGuard {
    video_id: VideoId,
    tx: mpsc::Sender<VideoId>,
    queue: Arc<Mutex<ResolverQueueState>>,
    finished: bool,
}

impl InFlightGuard {
    /// Finish the active request before publishing its result. A retained rerun makes
    /// this completion stale, so only the newer request is allowed to reach the owner.
    fn finish(&mut self) -> bool {
        self.finished = true;
        finish_resolve(&self.queue, &self.tx, &self.video_id)
    }
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        if !self.finished {
            let _ = finish_resolve(&self.queue, &self.tx, &self.video_id);
        }
    }
}

fn finish_resolve(
    queue: &Arc<Mutex<ResolverQueueState>>,
    tx: &mpsc::Sender<VideoId>,
    video_id: &VideoId,
) -> bool {
    let mut queue = queue
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    queue.in_flight.remove(video_id);

    let Some(cmd) = queue.rerun_after_flight.remove(video_id) else {
        return true;
    };
    let permit = match tx.try_reserve() {
        Ok(permit) => permit,
        Err(mpsc::error::TrySendError::Full(_)) => {
            // A deferred rerun counts against `capacity`, so every other pending key
            // leaves one channel slot reserved for this transition. Preserve the value
            // if that invariant is ever broken instead of silently losing it.
            queue.rerun_after_flight.insert(video_id.clone(), cmd);
            tracing::error!(video_id = %video_id, "resolver rerun scheduling invariant violated");
            return false;
        }
        Err(mpsc::error::TrySendError::Closed(_)) => {
            tracing::debug!(video_id = %video_id, "resolver closed before deferred rerun");
            return false;
        }
    };

    queue.pending.insert(video_id.clone(), cmd);
    permit.send(video_id.clone());
    false
}

fn resolver_channel(policy: QueuePolicy) -> (ResolverHandle, ResolverInbox) {
    let capacity = policy
        .capacity()
        .expect("resolver channel requires a bounded policy");
    let (tx, rx) = bounded_channel(policy);
    let queue = Arc::new(Mutex::new(ResolverQueueState {
        capacity,
        accepting: true,
        pending: HashMap::new(),
        in_flight: HashMap::new(),
        rerun_after_flight: HashMap::new(),
    }));
    let inbox_tx = tx.clone();
    (
        ResolverHandle {
            tx,
            queue: Arc::clone(&queue),
            shutdown_tx: None,
            actor_task: None,
        },
        ResolverInbox {
            rx,
            tx: inbox_tx,
            queue,
        },
    )
}

/// Spawn the resolver actor; results return as [`ResolverEvent`]s.
pub fn spawn<F>(emit: F, cookies: Option<PathBuf>) -> ResolverHandle
where
    F: Fn(ResolverEvent) + Send + Sync + 'static,
{
    spawn_with(
        emit,
        cookies,
        RESOLVER_QUEUE,
        |watch_url, cookies| async move { resolve_url(watch_url.as_str(), cookies.as_deref()).await },
    )
}

fn spawn_with<F, R, Fut>(
    emit: F,
    cookies: Option<PathBuf>,
    policy: QueuePolicy,
    resolve: R,
) -> ResolverHandle
where
    F: Fn(ResolverEvent) + Send + Sync + 'static,
    R: Fn(WatchUrl, Option<PathBuf>) -> Fut + Clone + Send + Sync + 'static,
    Fut: Future<Output = Option<String>> + Send + 'static,
{
    let (mut handle, inbox) = resolver_channel(policy);
    let emit: EventSink = Arc::new(emit);
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let actor_task = tokio::spawn(run_resolver_actor(
        inbox,
        emit,
        cookies,
        shutdown_rx,
        resolve,
    ));
    handle.shutdown_tx = Some(shutdown_tx);
    handle.actor_task = Some(actor_task);
    handle
}

async fn run_resolver_actor<R, Fut>(
    mut inbox: ResolverInbox,
    emit: EventSink,
    cookies: Option<PathBuf>,
    mut shutdown_rx: watch::Receiver<bool>,
    resolve: R,
) where
    R: Fn(WatchUrl, Option<PathBuf>) -> Fut + Clone + Send + Sync + 'static,
    Fut: Future<Output = Option<String>> + Send + 'static,
{
    let mut children = JoinSet::new();
    loop {
        if *shutdown_rx.borrow() {
            break;
        }

        tokio::select! {
            biased;
            changed = shutdown_rx.changed() => {
                if changed.is_err() || *shutdown_rx.borrow() {
                    break;
                }
            }
            joined = children.join_next(), if !children.is_empty() => {
                if let Some(Err(error)) = joined
                    && !error.is_cancelled()
                {
                    tracing::warn!(%error, "resolver child task failed");
                }
            }
            cmd = inbox.recv(), if children.len() < MAX_CONCURRENT => {
                let Some(ResolveCmd::Resolve {
                    video_id,
                    watch_url,
                    purpose,
                }) = cmd else {
                    break;
                };
                let emit = Arc::clone(&emit);
                let child_cookies = cookies.clone();
                let tx = inbox.tx.clone();
                let queue = Arc::clone(&inbox.queue);
                let resolve = resolve.clone();
                children.spawn(async move {
                    // Keep the semantic key retryable even if the resolver or callback panics.
                    let mut in_flight = InFlightGuard {
                        video_id: video_id.clone(),
                        tx,
                        queue,
                        finished: false,
                    };
                    let result = resolve(watch_url, child_cookies).await;
                    if !in_flight.finish() {
                        tracing::debug!(video_id = %video_id, "suppressing superseded resolver result");
                        return;
                    }
                    match result {
                        Some(stream_url) => {
                            tracing::info!(video_id = %video_id, "prefetched");
                            emit(ResolverEvent::Resolved {
                                video_id: video_id.clone(),
                                stream_url: StreamUrl::from(stream_url),
                                purpose,
                            });
                        }
                        None => {
                            tracing::warn!(video_id = %video_id, "prefetch failed");
                            emit(ResolverEvent::Failed {
                                video_id: video_id.clone(),
                                purpose,
                            });
                        }
                    }
                });
            }
        }
    }

    inbox.rx.close();
    {
        let mut queue = inbox
            .queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        close_resolver_admission(&mut queue);
    }
    children.abort_all();
    while let Some(joined) = children.join_next().await {
        if let Err(error) = joined
            && !error.is_cancelled()
        {
            tracing::warn!(%error, "resolver child task failed while joining shutdown");
        }
    }
}

/// Resolve a watch URL to a direct audio stream URL via `yt-dlp -g`.
async fn resolve_url(watch_url: &str, cookies: Option<&std::path::Path>) -> Option<String> {
    // Looked up per invocation (not captured at actor spawn) so a managed yt-dlp
    // installed mid-session — including by the playback self-heal — applies to the
    // very next resolve.
    resolve_url_with_program(&crate::tools::ytdlp_program(), watch_url, cookies).await
}

async fn resolve_url_with_program(
    program: &str,
    watch_url: &str,
    cookies: Option<&std::path::Path>,
) -> Option<String> {
    let watch_url = crate::api::validate_playable_url_destination(
        crate::search_source::SearchSource::Youtube,
        watch_url,
    )
    .await
    .map_err(|error| {
        tracing::warn!(%error, "refusing to resolve unsafe watch URL");
    })
    .ok()?;
    if let Some(stream_url) =
        resolve_url_with_format(program, &watch_url, cookies, "bestaudio", "audio-only").await
    {
        return Some(stream_url);
    }
    resolve_url_with_format(
        program,
        &watch_url,
        cookies,
        "bestaudio/best[acodec!=none]/best",
        "audio-containing fallback",
    )
    .await
}

async fn resolve_url_with_format(
    program: &str,
    watch_url: &str,
    cookies: Option<&std::path::Path>,
    format_selector: &str,
    stage: &str,
) -> Option<String> {
    let mut cmd = crate::tools::ytdlp_command_for(program);
    cmd.args(["-f", format_selector, "-g", "--no-playlist"])
        .arg(watch_url);
    crate::tools::append_ytdlp_cookie_args(&mut cmd, cookies);
    // Match mpv's cookie-auth stream client so prefetched CDN URLs are not the
    // TVHTML5 URLs that ffmpeg then rejects with HTTP 403.
    if cookies.is_some() {
        crate::tools::append_ytdlp_youtube_stream_extractor_args(&mut cmd);
    }
    cmd.stdin(Stdio::null());
    let out = process::tokio_output_limited(
        cmd,
        process::ProcessProfile::YtDlp,
        RESOLVE_TIMEOUT,
        RESOLVE_STDOUT_MAX,
    )
    .await
    .ok()?;
    if !out.status.success() {
        tracing::warn!(
            status = %out.status,
            stage,
            detail = %crate::tools::ytdlp_failure_detail(&out.stderr_tail).trim_start(),
            "yt-dlp resolve failed"
        );
        return None;
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    stdout
        .lines()
        .next()
        .map(str::trim)
        .filter(|l| is_acceptable_stream_url(l))
        .map(str::to_owned)
}

/// Guard the URL yt-dlp hands back before it is cached and passed to mpv `loadfile`.
/// The API validator rejects non-HTTP(S), credentials, control characters, and local/private IPs.
/// This keeps `file:`/`data:`/local-path/garbage — e.g. from an override binary or a broken
/// yt-dlp — out of the player. Same-user trust is already low; this matches the resolver's
/// existing defense-in-depth (bounded stdout, timeout).
fn is_acceptable_stream_url(s: &str) -> bool {
    crate::api::validate_playable_url(crate::search_source::SearchSource::Youtube, s).is_ok()
}

#[cfg(test)]
mod queue_tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;

    const ONE_SLOT_RESOLVER: QueuePolicy = QueuePolicy::CoalescedByKey {
        name: "test-resolver",
        capacity: 1,
    };

    #[tokio::test]
    async fn reports_saturation_without_evicting_the_pending_request() {
        let (handle, mut inbox) = resolver_channel(ONE_SLOT_RESOLVER);

        assert_eq!(
            handle.resolve("first".to_owned(), "https://example.test/first".to_owned()),
            Ok(DeliveryReceipt::Enqueued)
        );
        assert_eq!(
            handle.resolve(
                "second".to_owned(),
                "https://example.test/second".to_owned()
            ),
            Err(DeliveryError::Busy)
        );

        let ResolveCmd::Resolve {
            video_id,
            watch_url,
            ..
        } = inbox.recv().await.expect("first request remains pending");
        assert_eq!(video_id.as_str(), "first");
        assert_eq!(watch_url.as_str(), "https://example.test/first");
    }

    #[tokio::test]
    async fn duplicate_pending_resolve_replaces_the_value_without_using_another_slot() {
        let (handle, mut inbox) = resolver_channel(ONE_SLOT_RESOLVER);

        assert_eq!(
            handle.resolve("same".to_owned(), "https://example.test/old".to_owned()),
            Ok(DeliveryReceipt::Enqueued)
        );
        assert_eq!(
            handle.resolve("same".to_owned(), "https://example.test/new".to_owned()),
            Ok(DeliveryReceipt::Coalesced {
                replaced_existing: true,
                evicted_oldest: false,
            })
        );

        let ResolveCmd::Resolve {
            video_id,
            watch_url,
            ..
        } = inbox.recv().await.expect("coalesced request is delivered");
        assert_eq!(video_id.as_str(), "same");
        assert_eq!(watch_url.as_str(), "https://example.test/new");
        assert_eq!(
            handle.resolve("same".to_owned(), "https://example.test/new".to_owned()),
            Ok(DeliveryReceipt::Coalesced {
                replaced_existing: false,
                evicted_oldest: false,
            }),
            "an in-flight resolve satisfies duplicate requests"
        );
    }

    #[tokio::test]
    async fn changed_in_flight_url_suppresses_old_result_and_reruns_latest() {
        let (handle, mut inbox) = resolver_channel(ONE_SLOT_RESOLVER);

        assert_eq!(
            handle.resolve("same".to_owned(), "https://example.test/old".to_owned()),
            Ok(DeliveryReceipt::Enqueued)
        );
        let ResolveCmd::Resolve {
            video_id,
            watch_url,
            ..
        } = inbox.recv().await.expect("initial request is delivered");
        assert_eq!(watch_url.as_str(), "https://example.test/old");

        assert_eq!(
            handle.resolve("same".to_owned(), "https://example.test/new".to_owned()),
            Ok(DeliveryReceipt::Deferred),
            "a changed payload is not falsely reported as satisfied by the active resolve"
        );
        {
            let queue = handle
                .queue
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            assert_eq!(queue.queued_len(), 1);
            assert_eq!(
                queue
                    .rerun_after_flight
                    .get(&video_id)
                    .expect("changed payload is retained")
                    .watch_url()
                    .as_str(),
                "https://example.test/new"
            );
        }

        assert!(
            !inbox.finish(&video_id),
            "the superseded result must not reach the owner"
        );
        let ResolveCmd::Resolve {
            video_id: rerun_id,
            watch_url: rerun_url,
            ..
        } = inbox.recv().await.expect("changed payload is rerun");
        assert_eq!(rerun_id, video_id);
        assert_eq!(rerun_url.as_str(), "https://example.test/new");
    }

    #[tokio::test]
    async fn forced_same_url_self_heal_runs_exactly_once_after_active_prefetch() {
        let (handle, mut inbox) = resolver_channel(ONE_SLOT_RESOLVER);
        let video_id = "same".to_owned();
        let watch_url = "https://example.test/watch".to_owned();

        assert_eq!(
            handle.resolve(video_id.clone(), watch_url.clone()),
            Ok(DeliveryReceipt::Enqueued)
        );
        let ResolveCmd::Resolve {
            video_id: active_id,
            purpose: active_purpose,
            ..
        } = inbox.recv().await.expect("ordinary prefetch starts");
        assert_eq!(active_purpose, ResolvePurpose::Prefetch);

        assert_eq!(
            handle.resolve_for_self_heal(video_id.clone(), watch_url.clone()),
            Ok(DeliveryReceipt::Deferred),
            "the post-update resolve cannot coalesce into old in-flight work"
        );
        assert_eq!(
            handle.resolve_for_self_heal(video_id.clone(), watch_url.clone()),
            Ok(DeliveryReceipt::Coalesced {
                replaced_existing: false,
                evicted_oldest: false,
            }),
            "duplicate forced work remains bounded to one rerun"
        );
        assert_eq!(
            handle.resolve(video_id.clone(), watch_url.clone()),
            Ok(DeliveryReceipt::Coalesced {
                replaced_existing: false,
                evicted_oldest: false,
            }),
            "ordinary prefetch cannot cancel the stronger forced rerun"
        );
        assert_eq!(
            handle
                .queue
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .queued_len(),
            1
        );

        assert!(
            !inbox.finish(&active_id),
            "the old pre-update result is suppressed"
        );
        let ResolveCmd::Resolve {
            purpose,
            watch_url: rerun_url,
            ..
        } = inbox.recv().await.expect("forced resolve is scheduled");
        assert_eq!(purpose, ResolvePurpose::SelfHeal);
        assert_eq!(rerun_url.as_str(), watch_url);
        assert!(
            inbox.finish(&active_id),
            "the forced result is authoritative"
        );
        assert!(
            handle
                .queue
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .rerun_after_flight
                .is_empty(),
            "no duplicate forced rerun remains"
        );
    }

    #[tokio::test]
    async fn changed_in_flight_url_keeps_only_the_latest_bounded_rerun() {
        let (handle, mut inbox) = resolver_channel(ONE_SLOT_RESOLVER);

        assert_eq!(
            handle.resolve("same".to_owned(), "https://example.test/old".to_owned()),
            Ok(DeliveryReceipt::Enqueued)
        );
        let ResolveCmd::Resolve { video_id, .. } =
            inbox.recv().await.expect("initial request is delivered");

        assert_eq!(
            handle.resolve("same".to_owned(), "https://example.test/next".to_owned()),
            Ok(DeliveryReceipt::Deferred)
        );
        assert_eq!(
            handle.resolve("same".to_owned(), "https://example.test/latest".to_owned()),
            Ok(DeliveryReceipt::Coalesced {
                replaced_existing: true,
                evicted_oldest: false,
            })
        );
        assert_eq!(
            handle.resolve("same".to_owned(), "https://example.test/latest".to_owned()),
            Ok(DeliveryReceipt::Coalesced {
                replaced_existing: false,
                evicted_oldest: false,
            })
        );
        assert_eq!(
            handle
                .queue
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .queued_len(),
            1,
            "rerun storage remains bounded to one payload for the semantic key"
        );

        inbox.finish(&video_id);
        let ResolveCmd::Resolve { watch_url, .. } =
            inbox.recv().await.expect("latest payload is rerun");
        assert_eq!(watch_url.as_str(), "https://example.test/latest");
    }

    #[tokio::test]
    async fn changed_in_flight_url_reports_busy_when_no_rerun_slot_is_available() {
        let (handle, mut inbox) = resolver_channel(ONE_SLOT_RESOLVER);

        assert_eq!(
            handle.resolve("active".to_owned(), "https://example.test/old".to_owned()),
            Ok(DeliveryReceipt::Enqueued)
        );
        let ResolveCmd::Resolve { video_id, .. } =
            inbox.recv().await.expect("initial request is delivered");
        assert_eq!(
            handle.resolve(
                "queued".to_owned(),
                "https://example.test/queued".to_owned()
            ),
            Ok(DeliveryReceipt::Enqueued)
        );

        assert_eq!(
            handle.resolve("active".to_owned(), "https://example.test/new".to_owned()),
            Err(DeliveryError::Busy),
            "an unretained changed payload must not be reported as coalesced"
        );
        assert!(
            handle
                .queue
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .rerun_after_flight
                .is_empty()
        );

        inbox.finish(&video_id);
        let ResolveCmd::Resolve { video_id, .. } = inbox
            .recv()
            .await
            .expect("unrelated pending request remains");
        assert_eq!(video_id.as_str(), "queued");
    }

    #[tokio::test]
    async fn saturated_request_can_be_retried_after_the_pending_key_is_drained() {
        let (handle, mut inbox) = resolver_channel(ONE_SLOT_RESOLVER);

        assert_eq!(
            handle.resolve("first".to_owned(), "https://example.test/first".to_owned()),
            Ok(DeliveryReceipt::Enqueued)
        );
        assert_eq!(
            handle.resolve("retry".to_owned(), "https://example.test/retry".to_owned()),
            Err(DeliveryError::Busy)
        );

        let ResolveCmd::Resolve { video_id, .. } =
            inbox.recv().await.expect("first request is drained");
        inbox.finish(&video_id);
        assert_eq!(
            handle.resolve("retry".to_owned(), "https://example.test/retry".to_owned()),
            Ok(DeliveryReceipt::Enqueued)
        );
        let ResolveCmd::Resolve { video_id, .. } =
            inbox.recv().await.expect("retried request is accepted");
        assert_eq!(video_id.as_str(), "retry");
    }

    #[test]
    fn reports_closed_even_when_a_key_was_pending() {
        let (handle, inbox) = resolver_channel(ONE_SLOT_RESOLVER);
        assert_eq!(
            handle.resolve("pending".to_owned(), "https://example.test/old".to_owned()),
            Ok(DeliveryReceipt::Enqueued)
        );

        drop(inbox);

        assert_eq!(
            handle.resolve("pending".to_owned(), "https://example.test/new".to_owned()),
            Err(DeliveryError::Closed)
        );
    }

    #[tokio::test]
    async fn shutdown_closes_admission_and_joins_an_active_child() {
        struct ActiveResolve(Arc<AtomicBool>);

        impl Drop for ActiveResolve {
            fn drop(&mut self) {
                self.0.store(false, Ordering::SeqCst);
            }
        }

        let active = Arc::new(AtomicBool::new(false));
        let started = Arc::new(tokio::sync::Notify::new());
        let resolve_active = Arc::clone(&active);
        let resolve_started = Arc::clone(&started);
        let mut handle = spawn_with(
            |_| {},
            None,
            ONE_SLOT_RESOLVER,
            move |_, _| {
                let active = Arc::clone(&resolve_active);
                let started = Arc::clone(&resolve_started);
                async move {
                    active.store(true, Ordering::SeqCst);
                    let _active = ActiveResolve(active);
                    started.notify_one();
                    std::future::pending().await
                }
            },
        );

        let started_wait = started.notified();
        assert_eq!(
            handle.resolve(
                "active".to_owned(),
                "https://example.test/active".to_owned()
            ),
            Ok(DeliveryReceipt::Enqueued)
        );
        tokio::time::timeout(Duration::from_secs(1), started_wait)
            .await
            .expect("injected resolver child starts");
        assert!(active.load(Ordering::SeqCst));

        assert!(
            tokio::time::timeout(Duration::from_secs(1), handle.shutdown())
                .await
                .expect("resolver shutdown remains bounded")
        );
        assert!(
            !active.load(Ordering::SeqCst),
            "child was dropped and joined"
        );
        assert_eq!(
            handle.resolve("late".to_owned(), "https://example.test/late".to_owned()),
            Err(DeliveryError::Closed),
            "shutdown closes admission synchronously"
        );
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    #[tokio::test]
    async fn resolve_url_uses_fake_ytdlp_stdout() {
        let dir = temp_dir();
        fs::create_dir_all(&dir).unwrap();
        let fake = write_executable(
            &dir,
            "yt-dlp",
            "#!/bin/sh\nprintf '%s\\n' 'https://cdn.example/audio.m4a'\n",
        );

        let resolved = resolve_url_with_program(
            fake.to_str().unwrap(),
            "https://music.youtube.com/watch?v=abc123",
            None,
        )
        .await;

        assert_eq!(resolved.as_deref(), Some("https://cdn.example/audio.m4a"));
        let _ = fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn resolve_url_passes_cookie_file_to_ytdlp() {
        let dir = temp_dir();
        fs::create_dir_all(&dir).unwrap();
        let args_log = dir.join("args.txt");
        let cookies = dir.join("cookies.txt");
        fs::write(&cookies, "# Netscape HTTP Cookie File\n").unwrap();
        let fake = write_executable(
            &dir,
            "yt-dlp",
            &format!(
                "#!/bin/sh\nfor arg do printf '%s\\n' \"$arg\"; done > '{}'\nprintf '%s\\n' 'https://cdn.example/audio.m4a'\n",
                args_log.display()
            ),
        );

        let resolved = resolve_url_with_program(
            fake.to_str().unwrap(),
            "https://music.youtube.com/watch?v=abc123",
            Some(cookies.as_path()),
        )
        .await;

        assert_eq!(resolved.as_deref(), Some("https://cdn.example/audio.m4a"));
        let args: Vec<String> = fs::read_to_string(&args_log)
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect();
        let cookie_arg = cookies.to_string_lossy().into_owned();
        assert!(args.iter().any(|arg| arg == "--cookies"));
        assert!(args.iter().any(|arg| arg == &cookie_arg));
        let _ = fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn resolve_url_falls_back_to_audio_containing_format() {
        let dir = temp_dir();
        fs::create_dir_all(&dir).unwrap();
        let args_log = dir.join("args.txt");
        let fake = write_executable(
            &dir,
            "yt-dlp",
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\ncase \"$*\" in\n  *'bestaudio/best[acodec!=none]/best'*) printf '%s\\n' 'https://cdn.example/fallback.mp4' ;;\n  *) exit 1 ;;\nesac\n",
                args_log.display()
            ),
        );

        let resolved = resolve_url_with_program(
            fake.to_str().unwrap(),
            "https://music.youtube.com/watch?v=abc123",
            None,
        )
        .await;

        assert_eq!(
            resolved.as_deref(),
            Some("https://cdn.example/fallback.mp4")
        );
        let args = fs::read_to_string(&args_log).unwrap();
        assert!(args.contains("-f bestaudio -g --no-playlist"));
        assert!(args.contains("-f bestaudio/best[acodec!=none]/best -g --no-playlist"));
        let _ = fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn resolve_url_rejects_non_http_scheme() {
        let dir = temp_dir();
        fs::create_dir_all(&dir).unwrap();
        // A broken/hostile yt-dlp emitting a local path or file: URL must not reach mpv.
        let fake = write_executable(
            &dir,
            "yt-dlp",
            "#!/bin/sh\nprintf '%s\\n' 'file:///etc/passwd'\n",
        );
        let resolved =
            resolve_url_with_program(fake.to_str().unwrap(), "https://x/watch?v=abc123", None)
                .await;
        assert_eq!(resolved, None, "non-http scheme is rejected");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn stream_url_guard_allows_http_and_rejects_junk() {
        assert!(is_acceptable_stream_url("https://cdn.example/a.m4a"));
        assert!(is_acceptable_stream_url("HTTP://cdn.example/a.m4a"));
        assert!(!is_acceptable_stream_url("file:///etc/passwd"));
        assert!(!is_acceptable_stream_url("data:audio/mp3;base64,AAAA"));
        assert!(!is_acceptable_stream_url("/local/path.m4a"));
        assert!(!is_acceptable_stream_url("https://a\x1b.example/x"));
        assert!(!is_acceptable_stream_url(""));
    }

    fn write_executable(dir: &std::path::Path, name: &str, contents: &str) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, contents).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn temp_dir() -> PathBuf {
        // A per-call sequence, not just a timestamp: these tests start simultaneously and
        // macOS's clock granularity made same-nanos collisions real — two tests sharing a
        // dir overwrite each other's fake `yt-dlp`.
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "yututui-resolver-test-{}-{nanos}-{seq}",
            std::process::id()
        ))
    }
}
