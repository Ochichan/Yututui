//! Windows SMTC adapter (`SystemMediaTransportControls`), per the media-controls
//! spec §4.
//!
//! A console/TUI process has no top-level window, but the desktop interop
//! (`ISystemMediaTransportControlsInterop::GetForWindow`) requires one owned by the
//! calling thread — so a dedicated "smtc-worker" thread creates an invisible
//! top-level window (a real one, not message-only, which has known compatibility
//! problems) and runs the message pump SMTC needs. Snapshot diffs coalesce in a
//! bounded latest-state slot (the pump is woken with a posted thread message);
//! WinRT event handlers only forward a [`MediaCommand`] through the sink and return
//! (spec C-1).
//!
//! SMTC does not interpolate playback position: the worker re-pushes timeline
//! properties every ~5s while playing (spec W-3) from the snapshot's position
//! clock, and immediately on track change / seek / pause. Live streams get a
//! cleared timeline so no scrubber shows (W-4).

use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use windows::Foundation::{TimeSpan, TypedEventHandler};
use windows::Media::{
    AutoRepeatModeChangeRequestedEventArgs, MediaPlaybackAutoRepeatMode, MediaPlaybackStatus,
    MediaPlaybackType, PlaybackPositionChangeRequestedEventArgs,
    PlaybackRateChangeRequestedEventArgs, ShuffleEnabledChangeRequestedEventArgs,
    SystemMediaTransportControls, SystemMediaTransportControlsButton,
    SystemMediaTransportControlsButtonPressedEventArgs,
    SystemMediaTransportControlsTimelineProperties,
};
use windows::Storage::Streams::{
    DataWriter, InMemoryRandomAccessStream, RandomAccessStreamReference,
};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::System::WinRT::{
    ISystemMediaTransportControlsInterop, RO_INIT_MULTITHREADED, RoInitialize,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW, KillTimer, MSG,
    PM_NOREMOVE, PeekMessageW, PostThreadMessageW, RegisterClassW, SetTimer, TranslateMessage,
    WINDOW_EX_STYLE, WM_APP, WM_QUIT, WM_TIMER, WM_USER, WNDCLASSW, WS_OVERLAPPED,
};
use windows::core::{HSTRING, w};

use super::delivery::{
    DeliveryBatch, LatestMediaReceiver, LatestMediaSender, PositionClock, SubmitOutcome,
    latest_media_channel,
};
use super::{
    CommandSink, MediaChanges, MediaCommand, MediaPlaybackStatus as Status, MediaSnapshot,
};
use crate::queue::Repeat;

/// Wait for the first *playing* snapshot before enabling the session, so launching
/// the app never surfaces a blank SMTC entry or grabs media-key routing from the
/// app the user is actually listening to.
pub const EAGER: bool = false;

/// Posted to the worker thread to drain the snapshot channel.
const WM_APP_UPDATE: u32 = WM_APP + 1;
/// Timeline refresh cadence while playing (SMTC doesn't interpolate; spec W-3).
const TIMELINE_REFRESH_MS: u32 = 5_000;

pub struct Backend {
    updates: LatestMediaSender,
    worker_thread_id: u32,
    join: Option<std::thread::JoinHandle<()>>,
}

impl Backend {
    pub fn new(sink: CommandSink) -> Result<Self> {
        let (updates, rx) = latest_media_channel();
        let (ready_tx, ready_rx) = mpsc::channel::<std::result::Result<u32, String>>();
        let join = std::thread::Builder::new()
            .name("smtc-worker".to_owned())
            .spawn(move || worker(rx, sink, ready_tx))
            .context("could not spawn the SMTC worker thread")?;
        match ready_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(worker_thread_id)) => {
                tracing::info!("media controls: Windows SMTC session ready");
                Ok(Self {
                    updates,
                    worker_thread_id,
                    join: Some(join),
                })
            }
            Ok(Err(message)) => {
                let _ = join.join();
                Err(anyhow!(message))
            }
            Err(_) => Err(anyhow!("SMTC worker did not become ready in time")),
        }
    }

    pub fn apply(&mut self, snapshot: &MediaSnapshot, changes: MediaChanges) {
        if self.updates.submit(snapshot, changes) == SubmitOutcome::Wake {
            // SAFETY: `worker_thread_id` was captured from the live SMTC worker after its message
            // queue was created. If the post fails, re-arm the delivery slot: otherwise every
            // later update would coalesce behind a wake that never reached the worker.
            if let Err(error) = unsafe {
                PostThreadMessageW(self.worker_thread_id, WM_APP_UPDATE, WPARAM(0), LPARAM(0))
            } {
                self.updates.wake_failed();
                tracing::debug!(%error, "SMTC worker wake failed");
            }
        }
    }
}

impl Drop for Backend {
    fn drop(&mut self) {
        self.updates.close();
        // SAFETY: best-effort shutdown post to the worker thread id captured at
        // startup; failure is harmless because join handles already-ended threads.
        unsafe {
            let _ = PostThreadMessageW(self.worker_thread_id, WM_QUIT, WPARAM(0), LPARAM(0));
        }
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

/// Everything the worker owns while the session lives.
struct Session {
    smtc: SystemMediaTransportControls,
    hwnd: HWND,
    tokens: Vec<(&'static str, i64)>,
    /// The last applied snapshot — the position clock the 5s timeline timer reads.
    snapshot: MediaSnapshot,
    /// Stream reference for the current artwork, keyed by cache file.
    thumbnail: Option<(PathBuf, RandomAccessStreamReference)>,
    /// Live thread-timer id, or 0 when stopped. With a NULL hwnd, `SetTimer`
    /// IGNORES the requested id and returns a fresh one — killing anything other
    /// than the returned id silently fails and leaks a timer per play/pause cycle.
    timer_id: usize,
}

fn worker(
    rx: LatestMediaReceiver,
    sink: CommandSink,
    ready_tx: mpsc::Sender<std::result::Result<u32, String>>,
) {
    let mut session = match init_session(&sink) {
        Ok(session) => session,
        Err(e) => {
            let _ = ready_tx.send(Err(format!("{e:#}")));
            return;
        }
    };
    // SAFETY: `GetCurrentThreadId` has no preconditions and identifies this worker
    // thread for later `PostThreadMessageW` wake-ups.
    let thread_id = unsafe { GetCurrentThreadId() };
    let _ = ready_tx.send(Ok(thread_id));

    // The message pump: SMTC event delivery and our posted wake-ups both flow
    // through here. Thread messages (hwnd == 0) are handled inline; anything for
    // the hidden window goes through DefWindowProc.
    // SAFETY: `msg` points to valid storage for the duration of the pump; Win32
    // message APIs report termination/errors through return values.
    unsafe {
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            if msg.hwnd == HWND::default() {
                match msg.message {
                    WM_APP_UPDATE => {
                        while let Some(batch) = rx.try_take() {
                            session.apply(batch);
                        }
                        continue;
                    }
                    WM_TIMER => {
                        session.refresh_timeline();
                        continue;
                    }
                    _ => {}
                }
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    session.teardown();
}

fn init_session(sink: &CommandSink) -> Result<Session> {
    // SAFETY: all Win32/WinRT calls are made on this dedicated worker thread after
    // RoInitialize; handles and HRESULT/Result values are checked before use.
    unsafe {
        // Per-thread WinRT init; a "already initialized" style failure is fine.
        let _ = RoInitialize(RO_INIT_MULTITHREADED);

        // Ensure this thread has a message queue before the facade can post to it.
        let mut msg = MSG::default();
        let _ = PeekMessageW(&mut msg, None, WM_USER, WM_USER, PM_NOREMOVE);

        // An invisible **top-level** window (never shown). Message-only windows
        // (HWND_MESSAGE) are avoided: GetForWindow against them misbehaves.
        let instance = GetModuleHandleW(None).context("GetModuleHandleW failed")?;
        let class_name = w!("yututui_smtc_window");
        let class = WNDCLASSW {
            lpfnWndProc: Some(default_wndproc),
            hInstance: instance.into(),
            lpszClassName: class_name,
            ..Default::default()
        };
        // Re-registering after a worker restart fails harmlessly; CreateWindowExW
        // is the real gate.
        let _ = RegisterClassW(&class);
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class_name,
            w!("yututui media session"),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(instance.into()),
            None,
        )
        .context("CreateWindowExW failed for the hidden SMTC window")?;

        let interop: ISystemMediaTransportControlsInterop = windows::core::factory::<
            SystemMediaTransportControls,
            ISystemMediaTransportControlsInterop,
        >()
        .context("SystemMediaTransportControls activation factory unavailable")?;
        let smtc: SystemMediaTransportControls = interop
            .GetForWindow(hwnd)
            .context("ISystemMediaTransportControlsInterop::GetForWindow failed")?;

        let mut tokens = Vec::new();

        let button_sink = std::sync::Arc::clone(sink);
        tokens.push((
            "ButtonPressed",
            smtc.ButtonPressed(&TypedEventHandler::new(
                move |_,
                      args: windows::core::Ref<
                    SystemMediaTransportControlsButtonPressedEventArgs,
                >| {
                    if let Some(args) = args.as_ref()
                        && let Ok(button) = args.Button()
                    {
                        let cmd = match button {
                            SystemMediaTransportControlsButton::Play => Some(MediaCommand::Play),
                            SystemMediaTransportControlsButton::Pause => Some(MediaCommand::Pause),
                            SystemMediaTransportControlsButton::Stop => Some(MediaCommand::Stop),
                            SystemMediaTransportControlsButton::Next => Some(MediaCommand::Next),
                            SystemMediaTransportControlsButton::Previous => {
                                Some(MediaCommand::Previous)
                            }
                            _ => None,
                        };
                        if let Some(cmd) = cmd {
                            button_sink(cmd);
                        }
                    }
                    Ok(())
                },
            ))?,
        ));

        let seek_sink = std::sync::Arc::clone(sink);
        tokens.push((
            "PlaybackPositionChangeRequested",
            smtc.PlaybackPositionChangeRequested(&TypedEventHandler::new(
                move |_, args: windows::core::Ref<PlaybackPositionChangeRequestedEventArgs>| {
                    if let Some(args) = args.as_ref()
                        && let Ok(position) = args.RequestedPlaybackPosition()
                    {
                        seek_sink(MediaCommand::SeekTo(position.Duration as f64 / 1e7));
                    }
                    Ok(())
                },
            ))?,
        ));

        let shuffle_sink = std::sync::Arc::clone(sink);
        tokens.push((
            "ShuffleEnabledChangeRequested",
            smtc.ShuffleEnabledChangeRequested(&TypedEventHandler::new(
                move |_, args: windows::core::Ref<ShuffleEnabledChangeRequestedEventArgs>| {
                    if let Some(args) = args.as_ref()
                        && let Ok(enabled) = args.RequestedShuffleEnabled()
                    {
                        shuffle_sink(MediaCommand::SetShuffle(enabled));
                    }
                    Ok(())
                },
            ))?,
        ));

        let rate_sink = std::sync::Arc::clone(sink);
        tokens.push((
            "PlaybackRateChangeRequested",
            smtc.PlaybackRateChangeRequested(&TypedEventHandler::new(
                move |_, args: windows::core::Ref<PlaybackRateChangeRequestedEventArgs>| {
                    if let Some(args) = args.as_ref()
                        && let Ok(rate) = args.RequestedPlaybackRate()
                    {
                        rate_sink(MediaCommand::SetRate(rate));
                    }
                    Ok(())
                },
            ))?,
        ));

        let repeat_sink = std::sync::Arc::clone(sink);
        tokens.push((
            "AutoRepeatModeChangeRequested",
            smtc.AutoRepeatModeChangeRequested(&TypedEventHandler::new(
                move |_, args: windows::core::Ref<AutoRepeatModeChangeRequestedEventArgs>| {
                    if let Some(args) = args.as_ref()
                        && let Ok(mode) = args.RequestedAutoRepeatMode()
                    {
                        let repeat = match mode {
                            MediaPlaybackAutoRepeatMode::Track => Repeat::One,
                            MediaPlaybackAutoRepeatMode::List => Repeat::All,
                            _ => Repeat::Off,
                        };
                        repeat_sink(MediaCommand::SetRepeat(repeat));
                    }
                    Ok(())
                },
            ))?,
        ));

        // Static button set; per-track enablement follows in `apply` (spec §4.2).
        smtc.SetIsStopEnabled(true)?;
        smtc.SetIsFastForwardEnabled(false)?;
        smtc.SetIsRewindEnabled(false)?;
        smtc.SetIsRecordEnabled(false)?;
        smtc.SetIsChannelUpEnabled(false)?;
        smtc.SetIsChannelDownEnabled(false)?;
        smtc.SetIsEnabled(true)?;

        Ok(Session {
            smtc,
            hwnd,
            tokens,
            snapshot: MediaSnapshot::idle(),
            thumbnail: None,
            timer_id: 0,
        })
    }
}

impl Session {
    fn apply(&mut self, mut batch: DeliveryBatch) {
        batch.prepare_snapshot();
        if let Some(snapshot) = batch.snapshot.take() {
            self.snapshot = snapshot;
        } else {
            batch.apply_clock_to(&mut self.snapshot);
        }
        let needs_final_timeline = batch.needs_final_timeline();
        if let Err(e) = self.apply_facets(batch.changes) {
            tracing::debug!(error = %e, "SMTC update failed");
        }
        // Keep every position epoch even when metadata/progress coalesced while the
        // message pump was busy. Each clock also retains its own track duration/live
        // semantics, so a rapid track transition cannot reuse the newest duration.
        for position in batch.discontinuities {
            if let Err(e) = self.push_timeline_clock(position) {
                tracing::debug!(error = %e, epoch = position.position_epoch, "SMTC discontinuity timeline update failed");
            }
        }
        if needs_final_timeline && let Err(e) = self.push_timeline() {
            tracing::debug!(error = %e, "SMTC final timeline update failed");
        }
        self.manage_timer();
    }

    fn apply_facets(&mut self, changes: MediaChanges) -> windows::core::Result<()> {
        if changes.track || changes.artwork {
            self.update_display()?;
        }
        if changes.status || changes.track {
            self.smtc.SetPlaybackStatus(match self.snapshot.status {
                Status::Playing => MediaPlaybackStatus::Playing,
                Status::Paused => MediaPlaybackStatus::Paused,
                Status::Stopped => MediaPlaybackStatus::Stopped,
            })?;
        }
        if changes.caps || changes.track {
            let caps = self.snapshot.caps;
            self.smtc.SetIsPlayEnabled(caps.can_play)?;
            self.smtc.SetIsPauseEnabled(caps.can_pause)?;
            self.smtc.SetIsNextEnabled(caps.can_next)?;
            self.smtc.SetIsPreviousEnabled(caps.can_previous)?;
        }
        if changes.options || changes.track {
            // Reflect shuffle/repeat back so the flyout toggles match (spec W-2).
            self.smtc.SetShuffleEnabled(self.snapshot.shuffle)?;
            self.smtc.SetAutoRepeatMode(match self.snapshot.repeat {
                Repeat::Off => MediaPlaybackAutoRepeatMode::None,
                Repeat::One => MediaPlaybackAutoRepeatMode::Track,
                Repeat::All => MediaPlaybackAutoRepeatMode::List,
            })?;
            // Session property, not a timeline field: consumers (Phone Link, AVRCP,
            // third-party flyouts) extrapolate position between our 5s timeline pushes
            // using this rate, and RateChangeRequested never fires until it's seeded.
            self.smtc.SetPlaybackRate(self.snapshot.rate)?;
        }
        Ok(())
    }

    /// Rebuild the display metadata (spec §4.3 order: ClearAll → Type → properties
    /// → thumbnail → one Update call).
    fn update_display(&mut self) -> windows::core::Result<()> {
        let updater = self.smtc.DisplayUpdater()?;
        match &self.snapshot.track {
            None => {
                self.thumbnail = None;
                updater.ClearAll()?;
                updater.Update()?;
            }
            Some(track) => {
                updater.ClearAll()?;
                // Type must be set before touching MusicProperties.
                updater.SetType(MediaPlaybackType::Music)?;
                let music = updater.MusicProperties()?;
                music.SetTitle(&HSTRING::from(track.title.as_str()))?;
                if !track.artist.is_empty() {
                    music.SetArtist(&HSTRING::from(track.artist.as_str()))?;
                }
                if let Some(album) = track.album.as_deref().filter(|a| !a.is_empty()) {
                    music.SetAlbumTitle(&HSTRING::from(album))?;
                }
                if let Some(reference) = self.thumbnail_reference() {
                    updater.SetThumbnail(&reference)?;
                }
                updater.Update()?;
            }
        }
        Ok(())
    }

    /// The current track's artwork as a stream reference, loaded from the cache
    /// file once and reused until the file changes.
    fn thumbnail_reference(&mut self) -> Option<RandomAccessStreamReference> {
        let path = self.snapshot.track.as_ref()?.art_file.clone()?;
        if self.thumbnail.as_ref().map(|(p, _)| p) != Some(&path) {
            let bytes = std::fs::read(&path).ok()?;
            let reference = stream_reference_from_bytes(&bytes).ok()?;
            self.thumbnail = Some((path, reference));
        }
        self.thumbnail.as_ref().map(|(_, r)| r.clone())
    }

    /// Push the current timeline (start/end/seek range/position). Live streams get
    /// a default (cleared) timeline so no scrubber is shown (spec W-4).
    fn push_timeline(&self) -> windows::core::Result<()> {
        let track = self.snapshot.track.as_ref();
        let duration = track
            .and_then(|t| t.duration)
            .filter(|_| !track.is_some_and(|t| t.is_live));
        self.update_timeline(duration, self.snapshot.position_now())
    }

    fn push_timeline_clock(&self, clock: PositionClock) -> windows::core::Result<()> {
        self.update_timeline(clock.timeline_duration(), clock.position_now())
    }

    fn update_timeline(&self, duration: Option<f64>, position: f64) -> windows::core::Result<()> {
        let props = SystemMediaTransportControlsTimelineProperties::new()?;
        if let Some(duration) = duration {
            props.SetStartTime(timespan(0.0))?;
            props.SetMinSeekTime(timespan(0.0))?;
            props.SetEndTime(timespan(duration))?;
            props.SetMaxSeekTime(timespan(duration))?;
            props.SetPosition(timespan(position))?;
        }
        self.smtc.UpdateTimelineProperties(&props)
    }

    /// The 5s periodic tick while playing: SMTC doesn't interpolate, so without
    /// this the flyout scrubber appears frozen (spec W-3).
    fn refresh_timeline(&self) {
        if self.snapshot.status == Status::Playing
            && let Err(e) = self.push_timeline()
        {
            tracing::debug!(error = %e, "SMTC timeline refresh failed");
        }
    }

    fn manage_timer(&mut self) {
        let want = self.snapshot.status == Status::Playing
            && self
                .snapshot
                .track
                .as_ref()
                .is_some_and(|t| !t.is_live && t.duration.is_some());
        // SAFETY: thread timers use a null HWND, so WM_TIMER is delivered to this
        // worker's message queue; the returned timer id is stored for KillTimer.
        unsafe {
            if want && self.timer_id == 0 {
                // Thread timer (no window): WM_TIMER lands in the pump directly.
                // The system assigns the id — the one KillTimer must receive.
                self.timer_id = SetTimer(None, 0, TIMELINE_REFRESH_MS, None);
            } else if !want && self.timer_id != 0 {
                let _ = KillTimer(None, self.timer_id);
                self.timer_id = 0;
            }
        }
    }

    /// Orderly shutdown (spec W-1): Closed status → handlers off → metadata
    /// cleared → disabled → window destroyed — so the flyout entry disappears
    /// immediately on quit. The ClearAll must come *before* the disable
    /// (a disabled control can reject updater calls) and everything runs
    /// sequentially on this thread: Firefox observed that a non-sequential
    /// cleanup leaves a ghost SMTC entry showing the bare executable name.
    fn teardown(mut self) {
        let _ = self.smtc.SetPlaybackStatus(MediaPlaybackStatus::Closed);
        for (name, token) in self.tokens.drain(..) {
            let result = match name {
                "ButtonPressed" => self.smtc.RemoveButtonPressed(token),
                "PlaybackPositionChangeRequested" => {
                    self.smtc.RemovePlaybackPositionChangeRequested(token)
                }
                "PlaybackRateChangeRequested" => self.smtc.RemovePlaybackRateChangeRequested(token),
                "ShuffleEnabledChangeRequested" => {
                    self.smtc.RemoveShuffleEnabledChangeRequested(token)
                }
                "AutoRepeatModeChangeRequested" => {
                    self.smtc.RemoveAutoRepeatModeChangeRequested(token)
                }
                _ => Ok(()),
            };
            if let Err(e) = result {
                tracing::debug!(handler = name, error = %e, "SMTC handler removal failed");
            }
        }
        if let Ok(updater) = self.smtc.DisplayUpdater() {
            let _ = updater.ClearAll();
            let _ = updater.Update();
        }
        let _ = self.smtc.SetIsEnabled(false);
        // SAFETY: `timer_id` was returned by SetTimer for this thread, and `hwnd` was
        // created by this worker; both calls are best-effort during teardown.
        unsafe {
            if self.timer_id != 0 {
                let _ = KillTimer(None, self.timer_id);
            }
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

/// The hidden window needs no behavior of its own — everything defers to
/// `DefWindowProcW` (which the `windows` crate wraps as a plain fn, so it can't be
/// used as a `WNDPROC` directly).
///
/// # Safety
/// Windows calls this function with the standard window-procedure ABI and message
/// parameters for the hidden SMTC window; the function forwards them unchanged.
// SAFETY: this function is installed as a WNDPROC for the hidden window and matches
// the required system ABI; all parameters are forwarded to DefWindowProcW.
unsafe extern "system" fn default_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // SAFETY: parameters are forwarded exactly as received from the window manager;
    // DefWindowProcW owns validation and returns the default result.
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

fn timespan(seconds: f64) -> TimeSpan {
    TimeSpan {
        Duration: (seconds * 1e7) as i64, // 100 ns units
    }
}

/// Wrap raw image bytes in a WinRT stream reference (in-memory stream, simpler and
/// more reliable from a console app than `StorageFile` round-trips — spec §4.3).
fn stream_reference_from_bytes(bytes: &[u8]) -> windows::core::Result<RandomAccessStreamReference> {
    let stream = InMemoryRandomAccessStream::new()?;
    let writer = DataWriter::CreateDataWriter(&stream)?;
    writer.WriteBytes(bytes)?;
    writer.StoreAsync()?.join()?;
    writer.FlushAsync()?.join()?;
    writer.DetachStream()?;
    stream.Seek(0)?;
    RandomAccessStreamReference::CreateFromStream(&stream)
}
