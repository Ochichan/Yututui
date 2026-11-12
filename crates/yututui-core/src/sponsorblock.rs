//! Non-music segment skipping shared by both playback owners.
//!
//! The fetcher yields raw SponsorBlock `music_offtopic` ranges; [`sanitize`] turns them into a
//! sorted, non-overlapping list and [`SkipPolicy`] decides when a position report should trigger
//! a seek. Both owners feed it the same observations so they cannot disagree about when to skip.

/// A span of a track, in seconds, that the listener should not hear.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Segment {
    pub start: f64,
    pub end: f64,
}

/// Shortest segment worth a seek. Positions arrive about once per second, so anything shorter
/// would be over before the skip lands.
pub const MIN_SEGMENT_SECS: f64 = 2.0;
/// A segment covering this share of the track or more is a mislabeled track, not an interlude.
pub const MAX_COVERAGE: f64 = 0.9;
pub const MAX_SEGMENTS: usize = 50;
/// A skip is pointless when less than this remains in the segment.
const MIN_REMAINING_SECS: f64 = 0.5;
/// A segment ending this close to the end of the track seeks to `duration - TAIL_GUARD_SECS`
/// and lets the track finish through the normal end path.
const TAIL_GUARD_SECS: f64 = 1.0;
/// Run origin for a fresh load, so a segment starting at 0:00 counts as entered by playback.
const FRESH_ORIGIN: f64 = -1.0;
/// Largest forward step between two reports that still counts as uninterrupted playback.
/// Reports arrive once per media second, so one dropped report is the most this allows; a
/// larger step is a seek that landed.
const MAX_NATURAL_STEP_SECS: f64 = 2.0;
/// The first report of a fresh load may arrive a few seconds in.
const MAX_FRESH_START_SECS: f64 = 4.0;
/// Reports are whole media seconds and never go backwards in playback; this absorbs jitter.
const MAX_BACKWARD_JITTER_SECS: f64 = 0.5;

/// Whether `segment` lies inside a known track and covers less than [`MAX_COVERAGE`] of it.
/// An unknown length does not reject here; [`SkipPolicy::observe`] withholds the seek until
/// a positive duration can apply this rule.
fn fits_duration(segment: Segment, duration: Option<f64>) -> bool {
    let Some(duration) = duration.filter(|d| d.is_finite() && *d > 0.0) else {
        return true;
    };
    segment.start < duration && (segment.end - segment.start) < MAX_COVERAGE * duration
}

/// Validate, sort, merge and cap raw segments. `duration` filters only when it is known.
pub fn sanitize(raw: &[(f64, f64)], duration: Option<f64>) -> Vec<Segment> {
    let mut segments: Vec<Segment> = raw
        .iter()
        .filter(|(start, end)| start.is_finite() && end.is_finite() && end > start)
        .map(|&(start, end)| Segment {
            start: start.max(0.0),
            end,
        })
        .filter(|segment| segment.end - segment.start >= MIN_SEGMENT_SECS)
        .filter(|segment| fits_duration(*segment, duration))
        .collect();
    segments.sort_by(|a, b| a.start.total_cmp(&b.start));

    let mut merged: Vec<Segment> = Vec::with_capacity(segments.len());
    for segment in segments {
        match merged.last_mut() {
            Some(last) if segment.start <= last.end => last.end = last.end.max(segment.end),
            _ => merged.push(segment),
        }
    }
    merged.retain(|segment| fits_duration(*segment, duration));
    merged.truncate(MAX_SEGMENTS);
    merged
}

/// What an owner knows at one position report.
#[derive(Debug, Clone, Copy)]
pub struct Observation<'a> {
    pub position: f64,
    pub duration: Option<f64>,
    /// The owner's current `position_epoch`.
    pub epoch: u64,
    /// YouTube id of the loaded track, or `None` when the track is not eligible.
    pub track_id: Option<&'a str>,
    /// A listening seek is pending, a scrub drag is active, or a recovery is in progress.
    pub blocked: bool,
}

/// Skip decisions for the loaded track.
///
/// A segment is skipped only when uninterrupted playback carried the position into it: the
/// current run must have started before the segment. A seek, resume or recovery that lands
/// inside a segment starts a new run there, so the listener hears what they asked for.
#[derive(Debug, Default)]
pub struct SkipPolicy {
    segments: Option<(String, Vec<Segment>)>,
    /// Where the current uninterrupted run began. `None` until two consecutive reports confirm
    /// one, because the first report after a seek can predate the seek.
    run_origin: Option<f64>,
    last: Option<f64>,
    /// Target of a skip already sent. Blocks further decisions until the player catches up.
    pending_skip: Option<f64>,
    /// The skip's own seek has been admitted, so the next epoch increment is ours.
    own_bump_expected: bool,
    epoch: u64,
}

impl SkipPolicy {
    /// A track was loaded at `epoch` (the epoch after the load's own bump).
    pub fn on_load(&mut self, epoch: u64) {
        self.segments = None;
        self.run_origin = Some(FRESH_ORIGIN);
        self.last = Some(FRESH_ORIGIN);
        self.pending_skip = None;
        self.own_bump_expected = false;
        self.epoch = epoch;
    }

    pub fn set_segments(&mut self, youtube_id: String, segments: Vec<Segment>) {
        self.segments = Some((youtube_id, segments));
    }

    /// The skip's seek was admitted. The one epoch increment that follows is its own, not a
    /// foreign discontinuity.
    pub fn skip_committed(&mut self) {
        self.own_bump_expected = true;
    }

    /// Drop a sent skip that the owner could not commit.
    pub fn skip_abandoned(&mut self) {
        self.pending_skip = None;
        self.own_bump_expected = false;
    }

    /// Returns the seek target when this report shows playback running into a segment.
    pub fn observe(&mut self, obs: Observation<'_>) -> Option<f64> {
        if obs.epoch != self.epoch {
            let own = std::mem::take(&mut self.own_bump_expected)
                && obs.epoch == self.epoch.wrapping_add(1);
            self.epoch = obs.epoch;
            if !own {
                self.run_origin = None;
                self.last = Some(obs.position);
                self.pending_skip = None;
                return None;
            }
        }
        if let Some(target) = self.pending_skip {
            self.last = Some(obs.position);
            if obs.position >= target - MIN_REMAINING_SECS {
                self.pending_skip = None;
                self.run_origin = Some(obs.position);
            }
            return None;
        }
        let last = self.last.replace(obs.position);
        let natural = match last {
            Some(last) if last == FRESH_ORIGIN => {
                (0.0..=MAX_FRESH_START_SECS).contains(&obs.position)
            }
            Some(last) => {
                (-MAX_BACKWARD_JITTER_SECS..=MAX_NATURAL_STEP_SECS).contains(&(obs.position - last))
            }
            None => false,
        };
        if !natural {
            self.run_origin = None;
            return None;
        }
        // The earlier report of the pair can predate a seek, so the run starts at the later
        // one. A fresh load keeps the origin it was given.
        self.run_origin.get_or_insert(obs.position);
        if obs.blocked {
            return None;
        }
        let origin = self.run_origin?;
        let track_id = obs.track_id?;
        let (stored_id, segments) = self.segments.as_ref()?;
        if stored_id != track_id {
            return None;
        }
        // `MAX_COVERAGE` needs a positive length. Until one is known, a fresh 0:00 report
        // must not seek a mislabeled full-track span to its far end; the next report decides.
        let duration = obs.duration.filter(|d| d.is_finite() && *d > 0.0)?;
        let segment = segments.iter().find(|segment| {
            fits_duration(**segment, Some(duration))
                && origin < segment.start
                && obs.position >= segment.start
                && obs.position < segment.end
                && segment.end - obs.position >= MIN_REMAINING_SECS
        })?;
        let target = if segment.end >= duration - TAIL_GUARD_SECS {
            duration - TAIL_GUARD_SECS
        } else {
            segment.end
        };
        if target <= obs.position {
            return None;
        }
        self.pending_skip = Some(target);
        Some(target)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy_with(id: &str, segments: &[(f64, f64)]) -> SkipPolicy {
        let mut policy = SkipPolicy::default();
        policy.on_load(1);
        policy.set_segments(id.to_owned(), sanitize(segments, None));
        policy
    }

    fn obs(position: f64, epoch: u64) -> Observation<'static> {
        Observation {
            position,
            duration: Some(200.0),
            epoch,
            track_id: Some("vid"),
            blocked: false,
        }
    }

    #[test]
    fn sanitize_drops_invalid_short_and_oversized_segments() {
        let raw = [
            (f64::NAN, 10.0),
            (10.0, f64::INFINITY),
            (30.0, 20.0),
            (40.0, 41.5),
            (50.0, 60.0),
            (250.0, 260.0),
            (10.0, 195.0),
        ];
        let out = sanitize(&raw, Some(200.0));
        assert_eq!(
            out,
            vec![Segment {
                start: 50.0,
                end: 60.0
            }]
        );
    }

    #[test]
    fn sanitize_clamps_sorts_merges_and_caps() {
        let out = sanitize(
            &[(30.0, 40.0), (-5.0, 5.0), (4.0, 12.0), (39.0, 50.0)],
            None,
        );
        assert_eq!(
            out,
            vec![
                Segment {
                    start: 0.0,
                    end: 12.0
                },
                Segment {
                    start: 30.0,
                    end: 50.0
                }
            ]
        );
        let many: Vec<(f64, f64)> = (0..80)
            .map(|i| (f64::from(i) * 10.0, f64::from(i) * 10.0 + 5.0))
            .collect();
        assert_eq!(sanitize(&many, None).len(), MAX_SEGMENTS);
    }

    /// Feed one report per second from `from` up to and including `to`. Returns every skip.
    fn play(policy: &mut SkipPolicy, from: u32, to: u32, epoch: u64) -> Vec<f64> {
        (from..=to)
            .filter_map(|second| policy.observe(obs(f64::from(second), epoch)))
            .collect()
    }

    #[test]
    fn playback_running_into_a_segment_skips_it_once() {
        let mut policy = policy_with("vid", &[(60.0, 90.0)]);
        assert_eq!(play(&mut policy, 0, 59, 1), Vec::<f64>::new());
        assert_eq!(policy.observe(obs(60.0, 1)), Some(90.0));
    }

    #[test]
    fn a_seek_that_lands_inside_a_segment_plays_it() {
        let mut policy = policy_with("vid", &[(60.0, 90.0)]);
        play(&mut policy, 0, 10, 1);
        assert_eq!(play(&mut policy, 70, 95, 2), Vec::<f64>::new());
    }

    #[test]
    fn a_report_that_predates_the_seek_does_not_fake_an_entry() {
        let mut policy = policy_with("vid", &[(60.0, 90.0)]);
        play(&mut policy, 0, 58, 1);
        assert_eq!(policy.observe(obs(59.0, 2)), None);
        assert_eq!(play(&mut policy, 68, 95, 2), Vec::<f64>::new());
    }

    #[test]
    fn a_short_seek_onto_a_segment_start_plays_it_despite_a_stale_report() {
        let mut policy = policy_with("vid", &[(60.0, 90.0)]);
        play(&mut policy, 0, 57, 1);
        assert_eq!(policy.observe(obs(58.0, 2)), None);
        assert_eq!(play(&mut policy, 60, 95, 2), Vec::<f64>::new());
    }

    /// Reports alone cannot tell a landing one second before a segment from a report that
    /// predates the seek, so this case plays the segment.
    #[test]
    fn a_seek_landing_one_report_before_a_segment_plays_it() {
        let mut policy = policy_with("vid", &[(60.0, 90.0)]);
        play(&mut policy, 0, 10, 1);
        assert_eq!(play(&mut policy, 59, 95, 2), Vec::<f64>::new());
    }

    #[test]
    fn a_short_seek_into_a_segment_after_two_stale_reports_plays_it() {
        let mut policy = policy_with("vid", &[(60.0, 90.0)]);
        play(&mut policy, 0, 56, 1);
        assert_eq!(policy.observe(obs(57.0, 2)), None);
        assert_eq!(policy.observe(obs(58.0, 2)), None);
        assert_eq!(play(&mut policy, 61, 95, 2), Vec::<f64>::new());
    }

    #[test]
    fn a_fresh_load_whose_first_report_is_a_few_seconds_in_still_skips_a_leading_segment() {
        let mut policy = policy_with("vid", &[(0.0, 20.0)]);
        assert_eq!(policy.observe(obs(3.0, 1)), Some(20.0));
    }

    #[test]
    fn a_seek_landing_two_reports_before_a_segment_skips_it() {
        let mut policy = policy_with("vid", &[(60.0, 90.0)]);
        play(&mut policy, 0, 10, 1);
        assert_eq!(play(&mut policy, 58, 60, 2), vec![90.0]);
    }

    #[test]
    fn a_load_that_starts_far_from_zero_plays_the_segment_it_lands_in() {
        let mut policy = policy_with("vid", &[(290.0, 320.0)]);
        assert_eq!(
            policy.observe(Observation {
                duration: Some(600.0),
                ..obs(300.0, 1)
            }),
            None
        );
    }

    #[test]
    fn segment_at_zero_skips_on_a_fresh_load() {
        let mut policy = policy_with("vid", &[(0.0, 20.0)]);
        assert_eq!(policy.observe(obs(0.0, 1)), Some(20.0));
    }

    #[test]
    fn a_fresh_load_does_not_skip_an_oversized_span_before_duration_is_known() {
        let mut policy = policy_with("vid", &[(0.0, 190.0)]);
        let at_zero = Observation {
            duration: None,
            ..obs(0.0, 1)
        };
        assert_eq!(policy.observe(at_zero), None);
        assert_eq!(
            policy.observe(Observation {
                duration: Some(0.0),
                ..obs(0.0, 1)
            }),
            None
        );
        assert_eq!(policy.observe(obs(1.0, 1)), None);
    }

    #[test]
    fn a_leading_segment_skips_once_duration_is_known() {
        let mut policy = policy_with("vid", &[(0.0, 20.0)]);
        assert_eq!(
            policy.observe(Observation {
                duration: None,
                ..obs(0.0, 1)
            }),
            None
        );
        assert_eq!(policy.observe(obs(1.0, 1)), Some(20.0));
    }

    #[test]
    fn segments_that_arrive_after_playback_started_still_skip() {
        let mut policy = SkipPolicy::default();
        policy.on_load(1);
        assert_eq!(policy.observe(obs(0.0, 1)), None);
        policy.set_segments("vid".to_owned(), sanitize(&[(0.0, 20.0)], None));
        assert_eq!(policy.observe(obs(1.0, 1)), Some(20.0));
    }

    #[test]
    fn stale_position_after_the_skip_is_ignored() {
        let mut policy = policy_with("vid", &[(60.0, 90.0), (95.0, 120.0)]);
        play(&mut policy, 0, 59, 1);
        assert_eq!(policy.observe(obs(60.0, 1)), Some(90.0));
        policy.skip_committed();
        assert_eq!(policy.observe(obs(61.0, 2)), None);
        assert_eq!(policy.observe(obs(90.0, 2)), None);
        assert_eq!(policy.pending_skip, None);
        assert_eq!(play(&mut policy, 91, 95, 2), vec![120.0]);
    }

    #[test]
    fn foreign_bump_clears_the_pending_skip() {
        let mut policy = policy_with("vid", &[(60.0, 90.0)]);
        play(&mut policy, 0, 59, 1);
        assert_eq!(policy.observe(obs(60.0, 1)), Some(90.0));
        policy.skip_committed();
        assert_eq!(policy.observe(obs(20.0, 3)), None);
        assert_eq!(policy.pending_skip, None);
    }

    #[test]
    fn tail_segment_targets_one_second_before_the_end() {
        let mut policy = policy_with("vid", &[(150.0, 200.0)]);
        assert_eq!(play(&mut policy, 0, 150, 1), vec![199.0]);
    }

    #[test]
    fn ninety_percent_and_short_segments_are_ignored() {
        let mut policy = policy_with("vid", &[(5.0, 190.0), (100.0, 101.5)]);
        assert_eq!(play(&mut policy, 0, 110, 1), Vec::<f64>::new());
    }

    #[test]
    fn id_mismatch_does_not_skip() {
        let mut policy = policy_with("other", &[(60.0, 90.0)]);
        assert_eq!(play(&mut policy, 0, 70, 1), Vec::<f64>::new());
    }

    #[test]
    fn a_blocked_report_does_not_skip_and_the_next_free_one_does() {
        let mut policy = policy_with("vid", &[(60.0, 90.0)]);
        play(&mut policy, 0, 59, 1);
        let blocked = Observation {
            blocked: true,
            ..obs(60.0, 1)
        };
        assert_eq!(policy.observe(blocked), None);
        assert_eq!(policy.observe(obs(61.0, 1)), Some(90.0));
    }

    #[test]
    fn less_than_half_a_second_left_is_not_worth_a_seek() {
        let mut policy = policy_with("vid", &[(60.0, 90.4)]);
        play(&mut policy, 0, 10, 1);
        assert_eq!(play(&mut policy, 86, 89, 2), Vec::<f64>::new());
        assert_eq!(policy.observe(obs(90.0, 2)), None);
    }
}
