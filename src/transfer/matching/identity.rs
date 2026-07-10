//! Structured recording-version evidence for transfer matching.
//!
//! This module deliberately keeps presentation words ("official video", "visualizer")
//! separate from recording identity.  Marker recognition is context-aware: short or
//! ordinary words such as `live`, `cover`, and `inst` are only strong when they occur in
//! an annotation/version suffix or in an unambiguous multi-word phrase.

use super::normalize;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum VocalKind {
    #[default]
    OriginalOrUnknown,
    Instrumental,
    Karaoke,
    BackingTrack,
    Acapella,
    VocalRemoved,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum PerformanceKind {
    #[default]
    StudioOrUnknown,
    Live,
    Acoustic,
    Demo,
    Rehearsal,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum MixKind {
    #[default]
    OriginalOrUnknown,
    Remix,
    RadioEdit,
    Extended,
    SpedUp,
    Slowed,
    SlowedReverb,
    Nightcore,
    EightD,
    BassBoosted,
    Lofi,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum AuthorshipKind {
    #[default]
    OriginalOrUnknown,
    Cover,
    Tribute,
    AiCover,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum StereoKind {
    #[default]
    Unknown,
    Mono,
    Stereo,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct EditionProfile {
    pub remastered: bool,
    pub remaster_year: Option<u16>,
    pub radio_edit: bool,
    pub extended: bool,
    pub taylor_version: bool,
    pub deluxe_or_reissue: bool,
    pub stereo: StereoKind,
    pub language_version: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum EvidenceField {
    TitleAnnotation,
    TitleSuffix,
    TitlePhrase,
    Artist,
    Album,
    Channel,
    ExplicitMetadata,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct VersionEvidence {
    pub marker: &'static str,
    pub field: EvidenceField,
    /// 100 = explicit structured/annotation evidence, 70 = strong phrase, lower values
    /// are corroboration only and must not independently reject a candidate.
    pub confidence: u8,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct VersionProfile {
    pub vocal: VocalKind,
    pub performance: PerformanceKind,
    pub mix: MixKind,
    pub authorship: AuthorshipKind,
    pub edition: EditionProfile,
    pub explicit: Option<bool>,
    pub evidence: Vec<VersionEvidence>,
}

impl VersionProfile {
    #[cfg(test)]
    pub fn is_plain(&self) -> bool {
        self.vocal == VocalKind::OriginalOrUnknown
            && self.performance == PerformanceKind::StudioOrUnknown
            && self.mix == MixKind::OriginalOrUnknown
            && self.authorship == AuthorshipKind::OriginalOrUnknown
            && !self.edition.taylor_version
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CandidateDisposition {
    AutoEligible,
    ReviewOnly(&'static str),
    Rejected(&'static str),
}

pub(super) fn parse_version_profile(
    title: &str,
    artist: &str,
    album: Option<&str>,
    channel: Option<&str>,
    explicit: Option<bool>,
) -> VersionProfile {
    let mut profile = VersionProfile {
        explicit,
        ..VersionProfile::default()
    };
    if explicit.is_some() {
        profile.evidence.push(VersionEvidence {
            marker: if explicit == Some(true) {
                "explicit_metadata"
            } else {
                "clean_metadata"
            },
            field: EvidenceField::ExplicitMetadata,
            confidence: 100,
        });
    }

    let title_context = TitleContext::new(title);
    parse_title_markers(&title_context, &mut profile);
    parse_supporting_markers(artist, EvidenceField::Artist, &mut profile);
    if let Some(album) = album {
        parse_supporting_markers(album, EvidenceField::Album, &mut profile);
    }
    if let Some(channel) = channel {
        parse_supporting_markers(channel, EvidenceField::Channel, &mut profile);
    }
    profile
}

#[derive(Debug)]
struct TitleContext {
    normalized: String,
    annotations: Vec<String>,
    suffix: String,
    raw_lower: String,
}

impl TitleContext {
    fn new(title: &str) -> Self {
        let raw_lower = title.to_lowercase();
        let annotations = bracket_segments(title)
            .into_iter()
            .map(|value| normalize(&value))
            .filter(|value| !value.is_empty())
            .collect();
        let suffix = title
            .rfind(" - ")
            .map(|index| normalize(&title[index + 3..]))
            .unwrap_or_default();
        Self {
            normalized: normalize(title),
            annotations,
            suffix,
            raw_lower,
        }
    }

    fn phrase(&self, phrase: &str) -> bool {
        contains_phrase(&self.normalized, phrase)
    }

    fn version_context(&self, phrase: &str) -> Option<EvidenceField> {
        if self
            .annotations
            .iter()
            .any(|annotation| contains_phrase(annotation, phrase))
        {
            return Some(EvidenceField::TitleAnnotation);
        }
        if contains_phrase(&self.suffix, phrase) {
            return Some(EvidenceField::TitleSuffix);
        }
        None
    }

    fn unambiguous_phrase(&self, phrase: &str) -> Option<EvidenceField> {
        self.version_context(phrase)
            .or_else(|| self.phrase(phrase).then_some(EvidenceField::TitlePhrase))
    }
}

fn bracket_segments(value: &str) -> Vec<String> {
    let mut segments = Vec::new();
    let mut chars = value.chars().peekable();
    while let Some(ch) = chars.next() {
        if !matches!(ch, '(' | '[' | '{') {
            continue;
        }
        let close = match ch {
            '(' => ')',
            '[' => ']',
            _ => '}',
        };
        let mut depth = 1usize;
        let mut inner = String::new();
        for next in chars.by_ref() {
            if next == ch {
                depth += 1;
            } else if next == close {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
            inner.push(next);
        }
        if !inner.trim().is_empty() {
            segments.push(inner);
        }
    }
    segments
}

fn contains_phrase(normalized: &str, phrase: &str) -> bool {
    let needle = normalize(phrase);
    !needle.is_empty() && format!(" {normalized} ").contains(&format!(" {needle} "))
}

fn push_evidence(
    profile: &mut VersionProfile,
    marker: &'static str,
    field: EvidenceField,
    confidence: u8,
) {
    if !profile
        .evidence
        .iter()
        .any(|evidence| evidence.marker == marker && evidence.field == field)
    {
        profile.evidence.push(VersionEvidence {
            marker,
            field,
            confidence,
        });
    }
}

fn parse_title_markers(title: &TitleContext, profile: &mut VersionProfile) {
    let first_context = |phrases: &[&str]| {
        phrases
            .iter()
            .find_map(|phrase| title.unambiguous_phrase(phrase))
    };

    if let Some(field) = first_context(&[
        "instrumental",
        "instrumental version",
        "off vocal",
        "no vocal",
        "반주",
        "伴奏",
    ]) {
        profile.vocal = VocalKind::Instrumental;
        push_evidence(profile, "instrumental", field, 100);
    } else if let Some(field) = title.version_context("inst") {
        // `inst` is intentionally annotation/suffix-only so `Instinct` and artists
        // containing those letters never trigger it.
        profile.vocal = VocalKind::Instrumental;
        push_evidence(profile, "instrumental", field, 100);
    }
    if let Some(field) = first_context(&["karaoke", "노래방", "カラオケ"]) {
        profile.vocal = VocalKind::Karaoke;
        push_evidence(profile, "karaoke", field, 100);
    }
    if let Some(field) = first_context(&["backing track", "accompaniment", "mr version"]) {
        profile.vocal = VocalKind::BackingTrack;
        push_evidence(profile, "backing_track", field, 100);
    }
    if let Some(field) = first_context(&["acapella", "a cappella"]) {
        profile.vocal = VocalKind::Acapella;
        push_evidence(profile, "acapella", field, 100);
    }
    if let Some(field) = first_context(&["vocal removed", "vocal remover", "no vocals"]) {
        profile.vocal = VocalKind::VocalRemoved;
        push_evidence(profile, "vocal_removed", field, 100);
    }

    // A lone "live" is only a recording marker in annotation/suffix form. Longer
    // phrases are unambiguous in the whole title.
    let live_field = title.version_context("live").or_else(|| {
        ["live at", "live from", "live in", "live version", "concert"]
            .iter()
            .find_map(|phrase| title.phrase(phrase).then_some(EvidenceField::TitlePhrase))
    });
    if let Some(field) = live_field {
        profile.performance = PerformanceKind::Live;
        push_evidence(profile, "live", field, 100);
    }
    if let Some(field) = first_context(&["acoustic", "어쿠스틱", "アコースティック"]) {
        profile.performance = PerformanceKind::Acoustic;
        push_evidence(profile, "acoustic", field, 100);
    }
    if let Some(field) = title.version_context("demo") {
        profile.performance = PerformanceKind::Demo;
        push_evidence(profile, "demo", field, 100);
    }
    if let Some(field) = first_context(&["rehearsal", "practice recording"]) {
        profile.performance = PerformanceKind::Rehearsal;
        push_evidence(profile, "rehearsal", field, 100);
    }

    if let Some(field) = first_context(&["slowed reverb", "slowed and reverb"]) {
        profile.mix = MixKind::SlowedReverb;
        push_evidence(profile, "slowed_reverb", field, 100);
    } else if title.raw_lower.contains("slowed + reverb")
        || title.raw_lower.contains("slowed+reverb")
    {
        profile.mix = MixKind::SlowedReverb;
        push_evidence(profile, "slowed_reverb", EvidenceField::TitlePhrase, 100);
    } else if let Some(field) = first_context(&["sped up", "speed up", "spedup"]) {
        profile.mix = MixKind::SpedUp;
        push_evidence(profile, "sped_up", field, 100);
    } else if let Some(field) = first_context(&["nightcore"]) {
        profile.mix = MixKind::Nightcore;
        push_evidence(profile, "nightcore", field, 100);
    } else if let Some(field) = first_context(&["8d audio", "8d version"]) {
        profile.mix = MixKind::EightD;
        push_evidence(profile, "8d", field, 100);
    } else if let Some(field) = title.version_context("8d") {
        profile.mix = MixKind::EightD;
        push_evidence(profile, "8d", field, 100);
    } else if let Some(field) = first_context(&["bass boosted", "bass boost"]) {
        profile.mix = MixKind::BassBoosted;
        push_evidence(profile, "bass_boosted", field, 100);
    } else if let Some(field) = title.version_context("lofi") {
        profile.mix = MixKind::Lofi;
        push_evidence(profile, "lofi", field, 100);
    } else if let Some(field) = title.version_context("slowed") {
        profile.mix = MixKind::Slowed;
        push_evidence(profile, "slowed", field, 100);
    } else if let Some(field) = first_context(&["radio edit", "radio version"]) {
        profile.mix = MixKind::RadioEdit;
        profile.edition.radio_edit = true;
        push_evidence(profile, "radio_edit", field, 100);
    } else if let Some(field) = first_context(&["extended mix", "extended version"]) {
        profile.mix = MixKind::Extended;
        profile.edition.extended = true;
        push_evidence(profile, "extended", field, 100);
    } else if let Some(field) = first_context(&["remix", "리믹스", "リミックス"]) {
        profile.mix = MixKind::Remix;
        push_evidence(profile, "remix", field, 100);
    }

    let cover_field = ["covered by", "cover by", "tribute to", "ai cover"]
        .iter()
        .find_map(|phrase| title.phrase(phrase).then_some(EvidenceField::TitlePhrase))
        .or_else(|| title.version_context("cover"));
    if let Some(field) = cover_field {
        profile.authorship = if title.phrase("ai cover") {
            AuthorshipKind::AiCover
        } else if title.phrase("tribute to") {
            AuthorshipKind::Tribute
        } else {
            AuthorshipKind::Cover
        };
        push_evidence(profile, "cover", field, 100);
    } else if title.raw_lower.contains("歌ってみた") || title.raw_lower.contains("커버") {
        profile.authorship = AuthorshipKind::Cover;
        push_evidence(profile, "cover", EvidenceField::TitlePhrase, 100);
    }

    if title.raw_lower.contains("taylor's version") || title.raw_lower.contains("taylors version") {
        profile.edition.taylor_version = true;
        push_evidence(profile, "taylor_version", EvidenceField::TitlePhrase, 100);
    }
    if let Some(field) = first_context(&["mono"]) {
        profile.edition.stereo = StereoKind::Mono;
        push_evidence(profile, "mono", field, 100);
    } else if let Some(field) = first_context(&["stereo"]) {
        profile.edition.stereo = StereoKind::Stereo;
        push_evidence(profile, "stereo", field, 100);
    }
    if let Some((year, field)) = remaster_marker(title) {
        profile.edition.remastered = true;
        profile.edition.remaster_year = year;
        push_evidence(profile, "remaster", field, 100);
    }
    if first_context(&[
        "deluxe edition",
        "anniversary edition",
        "expanded edition",
        "reissue",
    ])
    .is_some()
    {
        profile.edition.deluxe_or_reissue = true;
    }
    for (marker, language) in [
        ("english version", "english"),
        ("japanese version", "japanese"),
        ("japanese ver", "japanese"),
        ("korean version", "korean"),
        ("chinese version", "chinese"),
        ("spanish version", "spanish"),
    ] {
        if let Some(field) = title.unambiguous_phrase(marker) {
            profile.edition.language_version = Some(language.to_owned());
            push_evidence(profile, "language_version", field, 100);
            break;
        }
    }
    if title.version_context("clean").is_some() || title.phrase("clean version") {
        profile.explicit = Some(false);
        push_evidence(profile, "clean_title", EvidenceField::TitlePhrase, 70);
    } else if title.version_context("explicit").is_some() || title.phrase("explicit version") {
        profile.explicit = Some(true);
        push_evidence(profile, "explicit_title", EvidenceField::TitlePhrase, 70);
    }
}

fn parse_supporting_markers(text: &str, field: EvidenceField, profile: &mut VersionProfile) {
    let normalized = normalize(text);
    // Supporting fields never independently set ambiguous short markers. They can only
    // corroborate unambiguous authorship/version phrases.
    if contains_phrase(&normalized, "tribute band") || contains_phrase(&normalized, "cover channel")
    {
        if profile.authorship == AuthorshipKind::OriginalOrUnknown {
            profile.authorship = AuthorshipKind::Cover;
        }
        push_evidence(profile, "cover_support", field, 70);
    }
    if contains_phrase(&normalized, "karaoke") {
        profile.vocal = VocalKind::Karaoke;
        push_evidence(profile, "karaoke_support", field, 80);
    }
}

fn remaster_marker(title: &TitleContext) -> Option<(Option<u16>, EvidenceField)> {
    for (text, field) in title
        .annotations
        .iter()
        .map(|value| (value.as_str(), EvidenceField::TitleAnnotation))
        .chain(std::iter::once((
            title.suffix.as_str(),
            EvidenceField::TitleSuffix,
        )))
    {
        if !contains_phrase(text, "remaster") && !contains_phrase(text, "remastered") {
            continue;
        }
        let year = text.split_whitespace().find_map(|token| {
            token
                .parse::<u16>()
                .ok()
                .filter(|year| (1900..=2200).contains(year))
        });
        return Some((year, field));
    }
    None
}

/// Version compatibility is intentionally asymmetric where uncertainty is common.
/// Clearly unsafe gimmick/utility recordings are rejected; live/remix/edition
/// uncertainty remains reviewable so retrieval recall is not converted into a wrong
/// automatic import.
pub(super) fn disposition(
    source: &VersionProfile,
    candidate: &VersionProfile,
) -> CandidateDisposition {
    use CandidateDisposition::{AutoEligible, Rejected, ReviewOnly};

    if source.vocal != candidate.vocal {
        return match candidate.vocal {
            VocalKind::Karaoke => Rejected("karaoke_mismatch"),
            VocalKind::BackingTrack | VocalKind::VocalRemoved => Rejected("backing_track_mismatch"),
            VocalKind::Instrumental | VocalKind::Acapella => {
                if source.vocal == VocalKind::OriginalOrUnknown {
                    ReviewOnly("instrumental_mismatch")
                } else {
                    ReviewOnly("vocal_version_mismatch")
                }
            }
            VocalKind::OriginalOrUnknown => ReviewOnly("missing_vocal_version_marker"),
        };
    }
    if source.authorship != candidate.authorship {
        return match candidate.authorship {
            AuthorshipKind::Cover | AuthorshipKind::Tribute | AuthorshipKind::AiCover => {
                ReviewOnly("cover_mismatch")
            }
            AuthorshipKind::OriginalOrUnknown => ReviewOnly("missing_cover_marker"),
        };
    }
    if source.mix != candidate.mix {
        return match candidate.mix {
            MixKind::SpedUp => Rejected("sped_up_mismatch"),
            MixKind::SlowedReverb => Rejected("slowed_reverb_mismatch"),
            MixKind::Nightcore => Rejected("nightcore_mismatch"),
            MixKind::EightD => Rejected("8d_mismatch"),
            MixKind::BassBoosted => Rejected("bass_boosted_mismatch"),
            MixKind::Lofi => Rejected("lofi_mismatch"),
            MixKind::OriginalOrUnknown => ReviewOnly("missing_mix_marker"),
            MixKind::Remix => ReviewOnly("remix_mismatch"),
            MixKind::RadioEdit => ReviewOnly("radio_edit_mismatch"),
            MixKind::Extended => ReviewOnly("extended_mismatch"),
            MixKind::Slowed => ReviewOnly("slowed_mismatch"),
        };
    }
    if source.performance != candidate.performance {
        return match candidate.performance {
            PerformanceKind::Live => ReviewOnly("live_mismatch"),
            PerformanceKind::Acoustic => ReviewOnly("acoustic_mismatch"),
            PerformanceKind::Demo => ReviewOnly("demo_mismatch"),
            PerformanceKind::Rehearsal => ReviewOnly("rehearsal_mismatch"),
            PerformanceKind::StudioOrUnknown => ReviewOnly("missing_performance_marker"),
        };
    }
    if source.edition.taylor_version != candidate.edition.taylor_version {
        return ReviewOnly(if source.edition.taylor_version {
            "missing_taylor_version_marker"
        } else {
            "taylor_version_mismatch"
        });
    }
    if source.edition.remastered != candidate.edition.remastered {
        return ReviewOnly(if source.edition.remastered {
            "missing_remaster_marker"
        } else {
            "remaster_mismatch"
        });
    }
    if source.edition.remaster_year.is_some()
        && candidate.edition.remaster_year.is_some()
        && source.edition.remaster_year != candidate.edition.remaster_year
    {
        return ReviewOnly("remaster_year_mismatch");
    }
    if source.edition.language_version != candidate.edition.language_version {
        return ReviewOnly(
            if source.edition.language_version.is_some()
                && candidate.edition.language_version.is_none()
            {
                "missing_language_version_marker"
            } else {
                "language_version_mismatch"
            },
        );
    }
    if source.edition.radio_edit != candidate.edition.radio_edit {
        return ReviewOnly("radio_edit_mismatch");
    }
    if source.edition.extended != candidate.edition.extended {
        return ReviewOnly("extended_mismatch");
    }
    if source.edition.deluxe_or_reissue != candidate.edition.deluxe_or_reissue {
        return ReviewOnly(if source.edition.deluxe_or_reissue {
            "missing_edition_marker"
        } else {
            "edition_mismatch"
        });
    }
    if source.edition.stereo != candidate.edition.stereo {
        return ReviewOnly(
            if source.edition.stereo != StereoKind::Unknown
                && candidate.edition.stereo == StereoKind::Unknown
            {
                "missing_stereo_marker"
            } else {
                "mono_stereo_mismatch"
            },
        );
    }
    if source.explicit.is_some()
        && candidate.explicit.is_some()
        && source.explicit != candidate.explicit
    {
        return ReviewOnly(if source.explicit == Some(true) {
            "clean_mismatch"
        } else {
            "explicit_mismatch"
        });
    }
    AutoEligible
}

pub(super) fn version_agreement(source: &VersionProfile, candidate: &VersionProfile) -> f32 {
    let mut compared = 0u32;
    let mut agreed = 0u32;
    macro_rules! compare {
        ($left:expr, $right:expr) => {{
            compared += 1;
            if $left == $right {
                agreed += 1;
            }
        }};
    }
    compare!(source.vocal, candidate.vocal);
    compare!(source.performance, candidate.performance);
    compare!(source.mix, candidate.mix);
    compare!(source.authorship, candidate.authorship);
    compare!(
        source.edition.taylor_version,
        candidate.edition.taylor_version
    );
    compare!(source.edition.remastered, candidate.edition.remastered);
    if source.edition.remaster_year.is_some() || candidate.edition.remaster_year.is_some() {
        compare!(
            source.edition.remaster_year,
            candidate.edition.remaster_year
        );
    }
    if source.edition.deluxe_or_reissue || candidate.edition.deluxe_or_reissue {
        compare!(
            source.edition.deluxe_or_reissue,
            candidate.edition.deluxe_or_reissue
        );
    }
    if source.edition.stereo != StereoKind::Unknown
        || candidate.edition.stereo != StereoKind::Unknown
    {
        compare!(source.edition.stereo, candidate.edition.stereo);
    }
    if source.edition.language_version.is_some() || candidate.edition.language_version.is_some() {
        compare!(
            source.edition.language_version,
            candidate.edition.language_version
        );
    }
    if source.explicit.is_some() && candidate.explicit.is_some() {
        compare!(source.explicit, candidate.explicit);
    }
    agreed as f32 / compared.max(1) as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(title: &str) -> VersionProfile {
        parse_version_profile(title, "Artist", Some("Album"), Some("Artist"), None)
    }

    #[test]
    fn marker_boundaries_avoid_ordinary_names() {
        assert!(profile("Live Forever").is_plain());
        assert!(parse_version_profile("Song", "Cover Drive", None, None, None).is_plain());
        assert!(profile("Instinct").is_plain());
        assert!(profile("8days").is_plain());
    }

    #[test]
    fn annotation_and_suffix_markers_are_structured() {
        assert_eq!(profile("Song (Live)").performance, PerformanceKind::Live);
        assert_eq!(
            profile("Song - Instrumental").vocal,
            VocalKind::Instrumental
        );
        assert_eq!(profile("Song [Radio Edit]").mix, MixKind::RadioEdit);
        assert_eq!(profile("Song (Slowed + Reverb)").mix, MixKind::SlowedReverb);
        assert_eq!(
            profile("Song (AI Cover)").authorship,
            AuthorshipKind::AiCover
        );
    }

    #[test]
    fn compatibility_is_symmetric_for_requested_versions() {
        let plain = profile("Song");
        let live = profile("Song (Live)");
        assert_eq!(
            disposition(&plain, &live),
            CandidateDisposition::ReviewOnly("live_mismatch")
        );
        assert_eq!(
            disposition(&live, &plain),
            CandidateDisposition::ReviewOnly("missing_performance_marker")
        );
        assert_eq!(
            disposition(&live, &live),
            CandidateDisposition::AutoEligible
        );
    }

    #[test]
    fn utility_and_gimmick_versions_are_rejected() {
        let plain = profile("Song");
        assert!(matches!(
            disposition(&plain, &profile("Song (Karaoke)")),
            CandidateDisposition::Rejected("karaoke_mismatch")
        ));
        assert!(matches!(
            disposition(&plain, &profile("Song (Nightcore)")),
            CandidateDisposition::Rejected("nightcore_mismatch")
        ));
    }

    #[test]
    fn explicit_metadata_is_stronger_than_title_guess() {
        let explicit = parse_version_profile("Song", "Artist", None, None, Some(true));
        let clean = parse_version_profile("Song", "Artist", None, None, Some(false));
        assert_eq!(
            disposition(&explicit, &clean),
            CandidateDisposition::ReviewOnly("clean_mismatch")
        );
    }
}
