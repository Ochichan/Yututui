//! Local-file crossfade policy.
//!
//! This module owns every rule about *whether* two tracks may overlap and *by how much*. The
//! reducer state that feeds the rules lives in [`crate::app`]. Splitting it that way means the
//! eligibility rules have exactly one home, so a new caller cannot re-derive "is this a local
//! file" and drift from it.
//!
//! Driving two mpv processes is deliberately absent. [`overlap_support`] is the single place
//! that answers whether this build can overlap at all, and while it says no every transition
//! stays the `loadfile … replace` it is today.

use std::num::NonZeroU8;
use std::time::Duration;

use crate::player::PlaybackLoad;

/// Longest supported fade, in tenths of a second.
const MAX_TENTHS: u8 = 30;

/// A configured crossfade length, 0.1s to 3.0s in tenths.
///
/// Tenths rather than an `f64` because the slider, the `[`/`]` nudge, the config file, and the
/// label all agree on tenths. A NaN, a negative, an off-tenth value, and an out-of-range value
/// are unrepresentable rather than clamped at each use. Zero lives in [`LocalCrossfade::Off`],
/// so "on, but zero seconds long" cannot be spelled.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct CrossfadeSecs(NonZeroU8);

impl CrossfadeSecs {
    pub const MAX: Self = Self(NonZeroU8::new(MAX_TENTHS).expect("MAX_TENTHS is non-zero"));

    /// `None` for zero or for anything past [`Self::MAX`].
    pub const fn from_tenths(tenths: u8) -> Option<Self> {
        match NonZeroU8::new(tenths) {
            Some(tenths) if tenths.get() <= MAX_TENTHS => Some(Self(tenths)),
            _ => None,
        }
    }

    pub const fn tenths(self) -> u8 {
        self.0.get()
    }

    pub fn as_secs_f64(self) -> f64 {
        f64::from(self.0.get()) / 10.0
    }
}

/// The stored setting: off, or on at a specific length.
///
/// Config, the settings draft, the status chip, and doctor all hold this, so none of them can
/// represent "on" without a length or disagree about what zero means.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum LocalCrossfade {
    #[default]
    Off,
    On(CrossfadeSecs),
}

impl LocalCrossfade {
    /// The config-file boundary. A non-finite, negative, or oversized number becomes a legal
    /// value here and never reaches the rest of the app, mirroring `config::clamp_seek_seconds`.
    pub fn from_secs(secs: f64) -> Self {
        let tenths = (crate::util::finite_or(secs, 0.0) * 10.0).round();
        Self::from_tenths(tenths.clamp(0.0, f64::from(MAX_TENTHS)) as u8)
    }

    /// Tenths past [`CrossfadeSecs::MAX`] saturate, which is what makes [`Self::nudge`] a
    /// plain `saturating_add_signed`.
    pub const fn from_tenths(tenths: u8) -> Self {
        match CrossfadeSecs::from_tenths(tenths) {
            Some(secs) => Self::On(secs),
            None if tenths > MAX_TENTHS => Self::On(CrossfadeSecs::MAX),
            None => Self::Off,
        }
    }

    pub const fn is_off(self) -> bool {
        matches!(self, Self::Off)
    }

    /// Tenths, zero when off. The slider bar and the persisted seconds both derive from this.
    pub const fn tenths(self) -> u8 {
        match self {
            Self::Off => 0,
            Self::On(secs) => secs.tenths(),
        }
    }

    pub fn as_secs_f64(self) -> f64 {
        f64::from(self.tenths()) / 10.0
    }

    /// One `[` / `]` press or one settings-slider step. Saturates at both ends.
    pub const fn nudge(self, steps: i8) -> Self {
        Self::from_tenths(self.tenths().saturating_add_signed(steps))
    }

    /// `Off` or `1.5s`. The single source for the settings row, the chip, and doctor, so those
    /// three can never disagree about how a value reads.
    pub fn label(self) -> String {
        match self {
            Self::Off => crate::t!("Off", "꺼짐", "オフ").to_owned(),
            Self::On(_) => format!("{:.1}s", self.as_secs_f64()),
        }
    }
}

/// A crossfade length known to be schedulable against the outgoing track.
///
/// [`envelope`] divides by this, and a zero-length overlap has no meaning, so the zero case is
/// removed from the type instead of re-checked in the transport. Distinct from
/// [`CrossfadeSecs`] because the configured length still has to survive the half-track clamp
/// in [`handoff`]; only what comes out of that clamp can open an overlap.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FadeLength(NonZeroU8);

impl FadeLength {
    pub fn as_secs_f64(self) -> f64 {
        f64::from(self.0.get()) / 10.0
    }

    pub fn duration(self) -> Duration {
        Duration::from_millis(u64::from(self.0.get()) * 100)
    }
}

/// How the outgoing file becomes the incoming file.
///
/// This travels with exactly one admitted load, like [`crate::player::MediaSourceContext`]. It
/// is owner-known intent that mpv cannot infer, not a wire type.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum TrackHandoff {
    /// Today's behavior. `loadfile <target> replace` on the deck that owns the transport.
    #[default]
    Cut,
    /// Start the incoming file on a second deck and ramp both for `fade`.
    Overlap { fade: FadeLength },
}

/// Whether this build and this machine can hold two audio outputs at once.
///
/// Probed, never assumed. `AudioBackendCaps::supports_gapless` is hardcoded `true`, and that is
/// the mistake this type exists to avoid repeating.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum OverlapSupport {
    /// Before the answer is known. Deliberately not available, so a load admitted during
    /// startup cannot be planned as an overlap on a host that turns out to refuse one.
    #[default]
    Untried,
    Available,
    Unavailable(OverlapBlocker),
}

impl OverlapSupport {
    pub const fn is_available(self) -> bool {
        matches!(self, Self::Available)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OverlapBlocker {
    /// The player transport drives one mpv whose IPC actor owns a single file-generation
    /// identity. A second lead-capable deck needs that identity per deck, so overlap is
    /// refused here rather than faked with a volume dip.
    SingleDeckTransport,
    /// mpv is absent, or its lifetime protection cannot be armed for a second deck. An
    /// unguarded mpv is refused by policy, so this is a hard no.
    Mpv,
    /// The audio output refused a second concurrent open. An exclusive-mode device or a bare
    /// ALSA `hw:` sink does this. Only the running transport can observe it.
    OutputBusy,
    /// The video overlay owns audio right now, with the audio engine paused underneath it.
    VideoOverlay,
}

impl OverlapBlocker {
    /// Doctor's reason line. Language-neutral, like every other capability label.
    pub const fn reason(self) -> &'static str {
        match self {
            Self::SingleDeckTransport => "single-deck transport",
            Self::Mpv => "mpv cannot hold a second guarded deck",
            Self::OutputBusy => "the audio output refused a second open",
            Self::VideoOverlay => "the video overlay owns audio",
        }
    }
}

/// Whether this build can overlap two files.
///
/// One deck is all the player transport has. `player::ipc`'s dispatch state carries a single
/// `issued_file_generation` plus the load/start/eof correlation keyed to it, and `PlayerHandle`
/// publishes one admitted generation to one actor, so promoting a second deck to lead means
/// remapping that identity across two actors. Until that lands this answers no, every
/// transition stays a `loadfile … replace`, and nothing pretends otherwise. Flipping this one
/// function is what turns the setting, the chip, and doctor on together.
pub const fn overlap_support() -> OverlapSupport {
    OverlapSupport::Unavailable(OverlapBlocker::SingleDeckTransport)
}

/// The one place crossfade eligibility is decided.
///
/// Gating on the resolved [`crate::playback_target::PlaybackDestination`] rather than on
/// `Song::local_path` or `local_dedicated_mode` is what makes the remote cases fall out for
/// free. A prefetched CDN URL, a sealed OpenSubsonic loopback route, and a radio stream are all
/// non-paths, so none of them needs its own guard here or at any call site.
///
/// `outgoing == None` is a cold start. `outgoing_duration == None` is an unknown length, which
/// cannot be scheduled against. Identical files are a self-reload (previous-at-queue-start,
/// repeat-one), which would double the same audio.
pub fn handoff(
    outgoing: Option<&PlaybackLoad>,
    outgoing_duration: Option<f64>,
    incoming: &PlaybackLoad,
    setting: LocalCrossfade,
    support: OverlapSupport,
) -> TrackHandoff {
    let LocalCrossfade::On(setting) = setting else {
        return TrackHandoff::Cut;
    };
    if !support.is_available() {
        return TrackHandoff::Cut;
    }
    let Some(outgoing) = outgoing else {
        return TrackHandoff::Cut;
    };
    if outgoing.source_context().is_live() || incoming.source_context().is_live() {
        return TrackHandoff::Cut;
    }
    let (Some(from), Some(to)) = (
        outgoing.destination().local_file_path(),
        incoming.destination().local_file_path(),
    ) else {
        return TrackHandoff::Cut;
    };
    let Some(duration) = outgoing_duration.filter(|secs| secs.is_finite() && *secs > 0.0) else {
        return TrackHandoff::Cut;
    };
    if !distinct_files(from, to) {
        return TrackHandoff::Cut;
    }
    // A 4s interlude must not be 75% crossfade, so the fade never exceeds half the outgoing
    // track. Truncating tenths keeps it under that half, and below one tenth there is nothing
    // left to fade.
    let half_track_tenths = (duration * 5.0).min(f64::from(MAX_TENTHS)) as u8;
    match CrossfadeSecs::from_tenths(setting.tenths().min(half_track_tenths)) {
        Some(fade) => TrackHandoff::Overlap {
            fade: FadeLength(fade.0),
        },
        None => TrackHandoff::Cut,
    }
}

/// Whether two playback targets are provably two different files.
///
/// The raw string is what mpv receives, but `./track.flac` and an absolute path through a
/// symlinked library root are the same audio, and overlapping a file with itself doubles it.
/// An unresolvable path (moved or unreadable between the load and this decision) counts as not
/// distinct, so the uncertain case degrades to a cut rather than to doubled audio.
fn distinct_files(from: &str, to: &str) -> bool {
    from != to
        && match (std::fs::canonicalize(from), std::fs::canonicalize(to)) {
            (Ok(from), Ok(to)) => from != to,
            _ => false,
        }
}

/// Per-deck gain during a fade, guaranteed to be a finite 0.0..=1.0 multiplier.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FadeGain(f64);

impl FadeGain {
    pub const fn get(self) -> f64 {
        self.0
    }
}

/// `(outgoing, incoming)` gains at `elapsed` into a `length` fade.
///
/// Equal power, not linear amplitude. Ramping both decks linearly dips about 3 dB at the
/// midpoint, which is audible as a hole in the middle of every transition.
pub fn envelope(elapsed: Duration, length: FadeLength) -> (FadeGain, FadeGain) {
    let progress = (elapsed.as_secs_f64() / length.as_secs_f64()).clamp(0.0, 1.0);
    let angle = progress * std::f64::consts::FRAC_PI_2;
    (FadeGain(angle.cos()), FadeGain(angle.sin()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::playback_target::{CredentialedPlaybackRef, PlaybackDestination};
    use crate::player::MediaSourceContext;

    /// Two real files, because `handoff` resolves both paths before it will overlap them.
    struct LocalPair {
        dir: std::path::PathBuf,
        first: String,
        second: String,
    }

    impl LocalPair {
        fn create(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("ytt-xfade-{}-{name}", std::process::id()));
            std::fs::create_dir_all(&dir).expect("temp dir");
            let first = dir.join("a.flac");
            let second = dir.join("b.flac");
            std::fs::write(&first, b"a").expect("write a");
            std::fs::write(&second, b"b").expect("write b");
            Self {
                first: first.to_string_lossy().into_owned(),
                second: second.to_string_lossy().into_owned(),
                dir,
            }
        }

        fn missing(&self) -> String {
            self.dir.join("gone.flac").to_string_lossy().into_owned()
        }
    }

    impl Drop for LocalPair {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn on_demand(target: &str) -> PlaybackLoad {
        PlaybackLoad::from_destination(
            PlaybackDestination::direct(target),
            MediaSourceContext::OnDemand,
        )
    }

    fn live(target: &str) -> PlaybackLoad {
        PlaybackLoad::from_destination(
            PlaybackDestination::direct(target),
            MediaSourceContext::Live,
        )
    }

    fn credentialed() -> PlaybackLoad {
        PlaybackLoad::from_destination(
            PlaybackDestination::Credentialed(CredentialedPlaybackRef::OpenSubsonic {
                backend_id: "backend".to_owned(),
                account_scope_id: "account".to_owned(),
                item_id: "item".to_owned(),
            }),
            MediaSourceContext::OnDemand,
        )
    }

    fn decide(outgoing: Option<&PlaybackLoad>, incoming: &PlaybackLoad) -> TrackHandoff {
        handoff(
            outgoing,
            Some(240.0),
            incoming,
            LocalCrossfade::from_tenths(15),
            OverlapSupport::Available,
        )
    }

    fn overlap(tenths: u8) -> TrackHandoff {
        TrackHandoff::Overlap {
            fade: FadeLength(NonZeroU8::new(tenths).expect("test fade is non-zero")),
        }
    }

    #[test]
    fn two_distinct_local_files_overlap_for_the_configured_length() {
        let pair = LocalPair::create("overlap");
        assert_eq!(
            decide(Some(&on_demand(&pair.first)), &on_demand(&pair.second)),
            overlap(15)
        );
    }

    #[test]
    fn every_non_local_transition_cuts() {
        let pair = LocalPair::create("cuts");
        let local = on_demand(&pair.first);
        let other = on_demand(&pair.second);
        let cdn = on_demand("https://rr3---sn-x.googlevideo.com/videoplayback?id=1");
        let station = live("https://stream.example/live.mp3");
        for (case, outgoing, incoming) in [
            ("cold start", None, &other),
            ("http prefetch incoming", Some(&local), &cdn),
            ("http prefetch outgoing", Some(&cdn), &other),
            ("credentialed incoming", Some(&local), &credentialed()),
            ("credentialed outgoing", Some(&credentialed()), &other),
            ("radio incoming", Some(&local), &station),
            ("radio outgoing", Some(&station), &other),
            ("self reload", Some(&local), &local),
        ] {
            assert_eq!(decide(outgoing, incoming), TrackHandoff::Cut, "{case}");
        }
    }

    #[test]
    fn two_spellings_of_one_file_are_a_self_reload() {
        let pair = LocalPair::create("spellings");
        let redundant = pair.first.replace("/a.flac", "/./a.flac");
        assert_ne!(redundant, pair.first);
        assert_eq!(
            decide(Some(&on_demand(&pair.first)), &on_demand(&redundant)),
            TrackHandoff::Cut
        );
    }

    #[test]
    fn an_unresolvable_path_cuts_instead_of_risking_doubled_audio() {
        let pair = LocalPair::create("missing");
        assert_eq!(
            decide(Some(&on_demand(&pair.first)), &on_demand(&pair.missing())),
            TrackHandoff::Cut
        );
    }

    #[test]
    fn an_unknown_or_degenerate_outgoing_length_cuts() {
        let pair = LocalPair::create("duration");
        for length in [None, Some(f64::NAN), Some(0.0), Some(-30.0)] {
            assert_eq!(
                handoff(
                    Some(&on_demand(&pair.first)),
                    length,
                    &on_demand(&pair.second),
                    LocalCrossfade::from_tenths(15),
                    OverlapSupport::Available,
                ),
                TrackHandoff::Cut,
                "{length:?}"
            );
        }
    }

    #[test]
    fn a_short_outgoing_track_fades_for_at_most_half_its_length() {
        let pair = LocalPair::create("short");
        for (duration, expected) in [
            (1.0, overlap(5)),
            (0.4, overlap(2)),
            (0.1, TrackHandoff::Cut),
        ] {
            assert_eq!(
                handoff(
                    Some(&on_demand(&pair.first)),
                    Some(duration),
                    &on_demand(&pair.second),
                    LocalCrossfade::On(CrossfadeSecs::MAX),
                    OverlapSupport::Available,
                ),
                expected,
                "{duration}s"
            );
        }
    }

    #[test]
    fn a_configured_length_is_not_lost_to_float_truncation() {
        let pair = LocalPair::create("tenths");
        for tenths in 1..=MAX_TENTHS {
            assert_eq!(
                handoff(
                    Some(&on_demand(&pair.first)),
                    Some(240.0),
                    &on_demand(&pair.second),
                    LocalCrossfade::from_tenths(tenths),
                    OverlapSupport::Available,
                ),
                overlap(tenths),
                "{tenths} tenths"
            );
        }
    }

    #[test]
    fn off_and_unsupported_both_cut() {
        let pair = LocalPair::create("gates");
        for support in [
            OverlapSupport::Untried,
            OverlapSupport::Unavailable(OverlapBlocker::SingleDeckTransport),
            OverlapSupport::Unavailable(OverlapBlocker::VideoOverlay),
            OverlapSupport::Unavailable(OverlapBlocker::Mpv),
            OverlapSupport::Unavailable(OverlapBlocker::OutputBusy),
        ] {
            assert_eq!(
                handoff(
                    Some(&on_demand(&pair.first)),
                    Some(240.0),
                    &on_demand(&pair.second),
                    LocalCrossfade::from_tenths(15),
                    support,
                ),
                TrackHandoff::Cut,
                "{support:?}"
            );
        }
        assert_eq!(
            handoff(
                Some(&on_demand(&pair.first)),
                Some(240.0),
                &on_demand(&pair.second),
                LocalCrossfade::Off,
                OverlapSupport::Available,
            ),
            TrackHandoff::Cut
        );
    }

    #[test]
    fn this_build_refuses_overlap_and_says_why() {
        assert_eq!(
            overlap_support(),
            OverlapSupport::Unavailable(OverlapBlocker::SingleDeckTransport)
        );
        assert!(!overlap_support().is_available());
        assert!(!OverlapSupport::Untried.is_available());
        assert_eq!(
            OverlapBlocker::SingleDeckTransport.reason(),
            "single-deck transport"
        );
    }

    #[test]
    fn the_config_boundary_clamps_every_hostile_number() {
        for (secs, expected) in [
            // A non-finite value falls back to the default before any clamping, exactly like
            // `config::clamp_seek_seconds`; only a finite oversized number clamps to the top.
            (f64::NAN, LocalCrossfade::Off),
            (f64::INFINITY, LocalCrossfade::Off),
            (f64::NEG_INFINITY, LocalCrossfade::Off),
            (-4.0, LocalCrossfade::Off),
            (0.0, LocalCrossfade::Off),
            (0.04, LocalCrossfade::Off),
            (0.06, LocalCrossfade::from_tenths(1)),
            (1.47, LocalCrossfade::from_tenths(15)),
            (3.0, LocalCrossfade::On(CrossfadeSecs::MAX)),
            (99.0, LocalCrossfade::On(CrossfadeSecs::MAX)),
        ] {
            assert_eq!(LocalCrossfade::from_secs(secs), expected, "{secs}");
        }
    }

    #[test]
    fn a_nudge_saturates_at_off_and_at_three_seconds() {
        assert_eq!(LocalCrossfade::Off.nudge(-1), LocalCrossfade::Off);
        assert_eq!(LocalCrossfade::Off.nudge(1), LocalCrossfade::from_tenths(1));
        assert_eq!(
            LocalCrossfade::from_tenths(1).nudge(-1),
            LocalCrossfade::Off
        );
        assert_eq!(
            LocalCrossfade::from_tenths(29).nudge(1),
            LocalCrossfade::On(CrossfadeSecs::MAX)
        );
        assert_eq!(
            LocalCrossfade::On(CrossfadeSecs::MAX).nudge(1),
            LocalCrossfade::On(CrossfadeSecs::MAX)
        );
    }

    #[test]
    fn the_label_reads_the_same_everywhere_it_is_shown() {
        let _guard = crate::i18n::lock_for_test();
        crate::i18n::set_language(crate::i18n::Language::English);
        assert_eq!(LocalCrossfade::Off.label(), "Off");
        assert_eq!(LocalCrossfade::from_tenths(1).label(), "0.1s");
        assert_eq!(LocalCrossfade::from_tenths(15).label(), "1.5s");
        assert_eq!(LocalCrossfade::On(CrossfadeSecs::MAX).label(), "3.0s");
    }

    #[test]
    fn the_envelope_holds_equal_power_across_the_fade() {
        let fade = FadeLength(NonZeroU8::new(20).expect("test fade is non-zero"));
        assert_eq!(fade.duration(), Duration::from_millis(2000));
        assert!((fade.as_secs_f64() - 2.0).abs() < 1e-9);

        let (out, incoming) = envelope(Duration::ZERO, fade);
        assert!((out.get() - 1.0).abs() < 1e-9);
        assert!(incoming.get().abs() < 1e-9);

        // Equal power means both decks sit at -3 dB in the middle, not at half amplitude.
        let (out, incoming) = envelope(Duration::from_millis(1000), fade);
        assert!((out.get() - std::f64::consts::FRAC_1_SQRT_2).abs() < 1e-9);
        assert!((incoming.get() - std::f64::consts::FRAC_1_SQRT_2).abs() < 1e-9);

        for elapsed in [0, 500, 1000, 1500, 2000, 9000] {
            let (out, incoming) = envelope(Duration::from_millis(elapsed), fade);
            let power = out.get().powi(2) + incoming.get().powi(2);
            assert!((power - 1.0).abs() < 1e-9, "{elapsed}ms power {power}");
        }

        let (out, incoming) = envelope(fade.duration(), fade);
        assert!(out.get().abs() < 1e-9);
        assert!((incoming.get() - 1.0).abs() < 1e-9);
    }
}
