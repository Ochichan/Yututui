//! Pure standard aggregate-history planning and timestamp normalization.

use data_encoding::HEXLOWER;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use super::model::{OpenSubsonicItemRef, ServerSong};

/// Maximum number of low-confidence plays expanded by the pure planner.
///
/// The complete logical ordinal range remains in [`AggregatePlan::range`]. Durable bridge
/// bookkeeping expands the rest incrementally instead of allocating an unbounded event vector.
pub const MAX_AGGREGATE_DELTA: u64 = 100;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AggregateHistoryShadow {
    pub play_count: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub played_at_unix: Option<i64>,
    pub observed_at_unix: i64,
    /// Monotonic local generation for a server counter that moved backwards.
    ///
    /// This is local bookkeeping for exact-versus-aggregate reconciliation only. It must
    /// never participate in a portable event ID because devices can observe resets at
    /// different times.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub counter_epoch: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AggregatePlay {
    pub event_id: String,
    pub item: OpenSubsonicItemRef,
    pub played_at_unix: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AggregatePlayRange {
    pub item: OpenSubsonicItemRef,
    pub first_ordinal: u64,
    pub last_ordinal: u64,
    pub played_at_unix: i64,
    pub has_server_time: bool,
    pub has_server_ordinal: bool,
}

impl AggregatePlayRange {
    pub(crate) fn logical_len(&self) -> u64 {
        self.last_ordinal
            .saturating_sub(self.first_ordinal)
            .saturating_add(1)
    }

    pub(crate) fn play(&self, ordinal: u64) -> Option<AggregatePlay> {
        if !(self.first_ordinal..=self.last_ordinal).contains(&ordinal) {
            return None;
        }
        let event_id = if self.has_server_time {
            aggregate_event_id(
                &self.item,
                self.played_at_unix,
                self.has_server_ordinal.then_some(ordinal),
            )
        } else {
            timeless_aggregate_event_id(&self.item, ordinal)
        };
        Some(AggregatePlay {
            event_id,
            item: self.item.clone(),
            played_at_unix: self.played_at_unix,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AggregatePlan {
    pub next_shadow: AggregateHistoryShadow,
    /// Complete compact logical range, including ordinals not expanded in `plays`.
    pub(crate) range: Option<AggregatePlayRange>,
    /// Bounded first chunk for callers that only need a preview.
    pub plays: Vec<AggregatePlay>,
    pub baseline_reset: bool,
    /// Counter growth deliberately consumed as baseline-only evidence.
    ///
    /// Once a server counter has reset, a count-only ordinal can be indistinguishable from an
    /// ordinal emitted before the reset. The caller still consumes matching exact credits, but
    /// conservatively avoids creating a duplicate aggregate event.
    pub baseline_only_delta: u64,
}

/// First observation and counter decreases establish a baseline only. Increases use the exact
/// item and each server counter ordinal as a deterministic dedupe identity. A valid server
/// `playedAt` strengthens that identity and supplies the event time. Without one, the local
/// observation time is display/retention metadata only and never participates in the event ID.
pub fn plan_aggregate_history(
    previous: Option<&AggregateHistoryShadow>,
    song: &ServerSong,
    observed_at_unix: i64,
) -> AggregatePlan {
    let played_at_unix = song.played_at.as_deref().and_then(parse_rfc3339_unix);
    let Some(previous) = previous else {
        return AggregatePlan {
            next_shadow: AggregateHistoryShadow {
                play_count: song.play_count.unwrap_or(0),
                played_at_unix,
                observed_at_unix,
                counter_epoch: 0,
            },
            range: None,
            plays: Vec::new(),
            baseline_reset: false,
            baseline_only_delta: 0,
        };
    };
    let mut counter_epoch = previous.counter_epoch;
    let (play_count, baseline_reset) = match song.play_count {
        Some(play_count) if play_count < previous.play_count => {
            counter_epoch = counter_epoch.saturating_add(1);
            (play_count, true)
        }
        Some(play_count) => (play_count, false),
        None if played_at_unix.is_some_and(|played_at| {
            previous
                .played_at_unix
                .is_none_or(|prior| played_at > prior)
        }) =>
        {
            // A changed last-played timestamp is the only standard evidence available.
            // Advance a synthetic ordinal so a later playCount readback of the same play
            // does not create a second event.
            (previous.play_count.saturating_add(1), false)
        }
        None => (previous.play_count, false),
    };
    let next_shadow = AggregateHistoryShadow {
        play_count,
        played_at_unix: played_at_unix.or(previous.played_at_unix),
        observed_at_unix,
        counter_epoch,
    };
    if baseline_reset || play_count <= previous.play_count {
        return AggregatePlan {
            next_shadow,
            range: None,
            plays: Vec::new(),
            baseline_reset,
            baseline_only_delta: 0,
        };
    }

    let observed_delta = play_count.saturating_sub(previous.play_count);
    if played_at_unix.is_none() && counter_epoch > 0 {
        return AggregatePlan {
            next_shadow,
            range: None,
            plays: Vec::new(),
            baseline_reset: false,
            baseline_only_delta: observed_delta,
        };
    }
    let first = previous.play_count.saturating_add(1);
    let has_server_ordinal = song.play_count.is_some();
    let range = AggregatePlayRange {
        item: song.item.clone(),
        first_ordinal: first,
        last_ordinal: play_count,
        played_at_unix: played_at_unix.unwrap_or(observed_at_unix),
        has_server_time: played_at_unix.is_some(),
        has_server_ordinal,
    };
    let plays = (first..=play_count)
        .take(MAX_AGGREGATE_DELTA as usize)
        .filter_map(|ordinal| range.play(ordinal))
        .collect();
    AggregatePlan {
        next_shadow,
        range: Some(range),
        plays,
        baseline_reset: false,
        baseline_only_delta: 0,
    }
}

fn aggregate_event_id(
    item: &OpenSubsonicItemRef,
    played_at_unix: i64,
    server_ordinal: Option<u64>,
) -> String {
    let mut digest = Sha256::new();
    digest.update(b"yututui-open-subsonic-aggregate-play-v3\0");
    update_hash(&mut digest, item.backend_id().as_str());
    update_hash(&mut digest, item.account_scope_id().as_str());
    update_hash(&mut digest, item.item_id().as_str());
    digest.update(played_at_unix.to_be_bytes());
    match server_ordinal {
        Some(ordinal) => {
            digest.update([1]);
            digest.update(ordinal.to_be_bytes());
        }
        None => digest.update([0]),
    }
    format!("sub-aggregate-{}", HEXLOWER.encode(&digest.finalize()))
}

fn timeless_aggregate_event_id(item: &OpenSubsonicItemRef, server_ordinal: u64) -> String {
    let mut digest = Sha256::new();
    digest.update(b"yututui-open-subsonic-aggregate-count-only-v1\0");
    update_hash(&mut digest, item.backend_id().as_str());
    update_hash(&mut digest, item.account_scope_id().as_str());
    update_hash(&mut digest, item.item_id().as_str());
    digest.update(server_ordinal.to_be_bytes());
    format!("sub-aggregate-{}", HEXLOWER.encode(&digest.finalize()))
}

const fn is_zero(value: &u64) -> bool {
    *value == 0
}

fn update_hash(digest: &mut Sha256, value: &str) {
    digest.update((value.len() as u64).to_be_bytes());
    digest.update(value.as_bytes());
}

/// Strict RFC3339 parser for display/retention timestamps. It accepts UTC `Z` and numeric
/// offsets with optional fractional seconds. Malformed timestamps simply provide no evidence.
pub fn parse_rfc3339_unix(value: &str) -> Option<i64> {
    let bytes = value.as_bytes();
    if bytes.len() < 20
        || bytes.get(4) != Some(&b'-')
        || bytes.get(7) != Some(&b'-')
        || !matches!(bytes.get(10), Some(b'T' | b't' | b' '))
        || bytes.get(13) != Some(&b':')
        || bytes.get(16) != Some(&b':')
    {
        return None;
    }
    let year = decimal(&bytes[0..4])?;
    let month = decimal(&bytes[5..7])?;
    let day = decimal(&bytes[8..10])?;
    let hour = decimal(&bytes[11..13])?;
    let minute = decimal(&bytes[14..16])?;
    let second = decimal(&bytes[17..19])?;
    if year < 1970
        || !(1..=12).contains(&month)
        || day == 0
        || day > days_in_month(year, month)
        || hour > 23
        || minute > 59
        || second > 59
    {
        return None;
    }

    let mut cursor = 19;
    if bytes.get(cursor) == Some(&b'.') {
        cursor += 1;
        let start = cursor;
        while bytes.get(cursor).is_some_and(u8::is_ascii_digit) {
            cursor += 1;
        }
        if cursor == start {
            return None;
        }
    }
    let offset_seconds = match bytes.get(cursor) {
        Some(b'Z' | b'z') if cursor + 1 == bytes.len() => 0_i64,
        Some(sign @ (b'+' | b'-'))
            if cursor + 6 == bytes.len() && bytes.get(cursor + 3) == Some(&b':') =>
        {
            let offset_hour = decimal(&bytes[cursor + 1..cursor + 3])?;
            let offset_minute = decimal(&bytes[cursor + 4..cursor + 6])?;
            if offset_hour > 23 || offset_minute > 59 {
                return None;
            }
            let offset = i64::from(offset_hour * 3_600 + offset_minute * 60);
            if *sign == b'+' { offset } else { -offset }
        }
        _ => return None,
    };

    let years = year.checked_sub(1970)?;
    let leap_days = leap_years_before(year).checked_sub(leap_years_before(1970))?;
    let mut days = years.checked_mul(365)?.checked_add(leap_days)?;
    for prior_month in 1..month {
        days = days.checked_add(days_in_month(year, prior_month))?;
    }
    days = days.checked_add(day.checked_sub(1)?)?;
    let local = i64::from(
        days.checked_mul(86_400)?
            .checked_add(hour.checked_mul(3_600)?)?
            .checked_add(minute.checked_mul(60)?)?
            .checked_add(second)?,
    );
    local.checked_sub(offset_seconds)
}

fn decimal(bytes: &[u8]) -> Option<u32> {
    bytes.iter().try_fold(0_u32, |value, byte| {
        if !byte.is_ascii_digit() {
            return None;
        }
        value.checked_mul(10)?.checked_add(u32::from(byte - b'0'))
    })
}

fn days_in_month(year: u32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap_year(year) => 29,
        2 => 28,
        _ => 0,
    }
}

fn is_leap_year(year: u32) -> bool {
    year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400))
}

fn leap_years_before(year: u32) -> u32 {
    let prior = year.saturating_sub(1);
    prior / 4 - prior / 100 + prior / 400
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::open_subsonic::{AccountScopeId, BackendId, ItemId};

    fn song(play_count: u64, played_at: Option<&str>) -> ServerSong {
        ServerSong {
            item: OpenSubsonicItemRef::new(
                BackendId::new("backend").unwrap(),
                AccountScopeId::new("account").unwrap(),
                ItemId::new("item").unwrap(),
            ),
            title: "Track".to_owned(),
            artist: "Artist".to_owned(),
            artists: Vec::new(),
            album: None,
            album_id: None,
            album_artist: None,
            duration_secs: Some(180),
            track_number: None,
            disc_number: None,
            year: None,
            cover_art_id: None,
            content_type: None,
            suffix: None,
            starred: false,
            user_rating: None,
            play_count: Some(play_count),
            played_at: played_at.map(str::to_owned),
        }
    }

    #[test]
    fn timestamp_parser_handles_offsets_fraction_and_invalid_dates() {
        assert_eq!(parse_rfc3339_unix("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_rfc3339_unix("1970-01-01T09:00:00.123+09:00"), Some(0));
        assert_eq!(
            parse_rfc3339_unix("2024-02-29T00:00:00-01:00"),
            Some(1_709_168_400)
        );
        assert_eq!(parse_rfc3339_unix("2023-02-29T00:00:00Z"), None);
        assert_eq!(parse_rfc3339_unix("1970-01-01T00:00:60Z"), None);
    }

    #[test]
    fn baseline_reset_and_bounded_stable_increases() {
        let first = plan_aggregate_history(None, &song(10, None), 100);
        assert!(first.plays.is_empty());
        assert_eq!(first.baseline_only_delta, 0);
        let reset = plan_aggregate_history(Some(&first.next_shadow), &song(2, None), 200);
        assert!(reset.baseline_reset);
        assert!(reset.plays.is_empty());
        assert_eq!(reset.baseline_only_delta, 0);

        let previous = AggregateHistoryShadow {
            play_count: 1,
            played_at_unix: None,
            observed_at_unix: 1,
            counter_epoch: 0,
        };
        let plan = plan_aggregate_history(
            Some(&previous),
            &song(102, Some("1970-01-01T00:01:40Z")),
            200,
        );
        assert_eq!(plan.plays.len(), MAX_AGGREGATE_DELTA as usize);
        let range = plan.range.as_ref().unwrap();
        assert_eq!(range.first_ordinal, 2);
        assert_eq!(range.last_ordinal, 102);
        assert_eq!(range.logical_len(), 101);
        assert_eq!(
            plan.plays.last().unwrap().event_id,
            range.play(101).unwrap().event_id
        );
        assert_ne!(
            plan.plays.last().unwrap().event_id,
            range.play(102).unwrap().event_id,
            "the unexpanded logical tail must retain its own stable identity"
        );
        assert_eq!(plan.plays[0].played_at_unix, 100);
        assert_eq!(plan.baseline_only_delta, 0);
        let retry = plan_aggregate_history(
            Some(&previous),
            &song(102, Some("1970-01-01T00:01:40Z")),
            999,
        );
        assert_eq!(
            plan.plays
                .iter()
                .map(|play| &play.event_id)
                .collect::<Vec<_>>(),
            retry
                .plays
                .iter()
                .map(|play| &play.event_id)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn counter_reset_keeps_local_epoch_but_changes_identity_with_server_time() {
        let baseline = plan_aggregate_history(None, &song(0, Some("1970-01-01T00:00:10Z")), 10);
        let original = plan_aggregate_history(
            Some(&baseline.next_shadow),
            &song(1, Some("1970-01-01T00:00:20Z")),
            20,
        );
        assert_eq!(original.plays.len(), 1);

        let reset = plan_aggregate_history(
            Some(&original.next_shadow),
            &song(0, Some("1970-01-01T00:00:30Z")),
            30,
        );
        assert!(reset.baseline_reset);
        assert_eq!(reset.next_shadow.counter_epoch, 1);
        let after_reset = plan_aggregate_history(
            Some(&reset.next_shadow),
            &song(1, Some("1970-01-01T00:00:40Z")),
            40,
        );
        assert_eq!(after_reset.plays.len(), 1);
        assert_ne!(original.plays[0].event_id, after_reset.plays[0].event_id);

        let retry = plan_aggregate_history(
            Some(&reset.next_shadow),
            &song(1, Some("1970-01-01T00:00:40Z")),
            99,
        );
        assert_eq!(after_reset.plays[0].event_id, retry.plays[0].event_id);
    }

    #[test]
    fn missing_or_invalid_played_at_imports_portable_low_confidence_plays() {
        let previous = AggregateHistoryShadow {
            play_count: 5,
            played_at_unix: Some(100),
            observed_at_unix: 100,
            counter_epoch: 0,
        };
        let missing = plan_aggregate_history(Some(&previous), &song(7, None), 200);
        assert_eq!(missing.plays.len(), 2);
        assert_eq!(missing.baseline_only_delta, 0);
        assert!(missing.plays.iter().all(|play| play.played_at_unix == 200));
        assert_eq!(missing.next_shadow.play_count, 7);
        assert_eq!(missing.next_shadow.played_at_unix, Some(100));

        let invalid = plan_aggregate_history(
            Some(&missing.next_shadow),
            &song(9, Some("not-a-timestamp")),
            300,
        );
        assert_eq!(invalid.plays.len(), 2);
        assert_eq!(invalid.baseline_only_delta, 0);
        assert!(invalid.plays.iter().all(|play| play.played_at_unix == 300));
        assert_eq!(invalid.next_shadow.play_count, 9);
        assert_eq!(invalid.next_shadow.played_at_unix, Some(100));

        let verified = plan_aggregate_history(
            Some(&invalid.next_shadow),
            &song(10, Some("1970-01-01T00:06:40Z")),
            400,
        );
        assert_eq!(verified.plays.len(), 1);
        assert_eq!(verified.plays[0].played_at_unix, 400);
        assert_eq!(verified.baseline_only_delta, 0);
    }

    #[test]
    fn count_only_ids_are_retry_stable_and_post_reset_growth_is_conservative() {
        let previous = AggregateHistoryShadow {
            play_count: 1,
            played_at_unix: None,
            observed_at_unix: 100,
            counter_epoch: 0,
        };
        let observation = song(2, None);
        let first = plan_aggregate_history(Some(&previous), &observation, 200);
        let retry = plan_aggregate_history(Some(&previous), &observation, 999);
        assert_eq!(first.plays.len(), 1);
        assert_eq!(first.plays[0].event_id, retry.plays[0].event_id);
        assert_ne!(
            first.plays[0].played_at_unix, retry.plays[0].played_at_unix,
            "local observation time is display/retention metadata, not portable identity"
        );

        let reset = plan_aggregate_history(Some(&first.next_shadow), &song(0, None), 300);
        assert!(reset.baseline_reset);
        assert_eq!(reset.next_shadow.counter_epoch, 1);
        let reused_ordinals = plan_aggregate_history(Some(&reset.next_shadow), &song(2, None), 400);
        assert!(reused_ordinals.plays.is_empty());
        assert_eq!(reused_ordinals.baseline_only_delta, 2);
        let reset_retry = plan_aggregate_history(Some(&reset.next_shadow), &song(2, None), 999);
        assert!(reset_retry.plays.is_empty());
        assert_eq!(
            reset_retry.baseline_only_delta,
            reused_ordinals.baseline_only_delta
        );
        assert_eq!(
            reset_retry.next_shadow.play_count,
            reused_ordinals.next_shadow.play_count
        );
        assert_eq!(
            reset_retry.next_shadow.counter_epoch,
            reused_ordinals.next_shadow.counter_epoch
        );
    }

    #[test]
    fn two_devices_with_different_local_epochs_emit_the_same_ids() {
        let first_device = AggregateHistoryShadow {
            play_count: 7,
            played_at_unix: Some(100),
            observed_at_unix: 100,
            counter_epoch: 0,
        };
        let second_device = AggregateHistoryShadow {
            counter_epoch: 42,
            ..first_device.clone()
        };
        let observation = song(9, Some("1970-01-01T00:03:20Z"));
        let first = plan_aggregate_history(Some(&first_device), &observation, 200);
        let second = plan_aggregate_history(Some(&second_device), &observation, 201);

        assert_eq!(first.plays.len(), 2);
        assert_eq!(
            first
                .plays
                .iter()
                .map(|play| &play.event_id)
                .collect::<Vec<_>>(),
            second
                .plays
                .iter()
                .map(|play| &play.event_id)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn reset_observers_converge_despite_different_local_epochs() {
        let before_reset = AggregateHistoryShadow {
            play_count: 3,
            played_at_unix: Some(100),
            observed_at_unix: 100,
            counter_epoch: 0,
        };
        let other_before_reset = AggregateHistoryShadow {
            counter_epoch: 17,
            ..before_reset.clone()
        };
        let reset_observation = song(0, Some("1970-01-01T00:03:20Z"));
        let first_reset = plan_aggregate_history(Some(&before_reset), &reset_observation, 200);
        let second_reset =
            plan_aggregate_history(Some(&other_before_reset), &reset_observation, 201);
        assert_eq!(first_reset.next_shadow.counter_epoch, 1);
        assert_eq!(second_reset.next_shadow.counter_epoch, 18);

        let next_play = song(1, Some("1970-01-01T00:05:00Z"));
        let first = plan_aggregate_history(Some(&first_reset.next_shadow), &next_play, 300);
        let second = plan_aggregate_history(Some(&second_reset.next_shadow), &next_play, 301);
        assert_eq!(first.plays.len(), 1);
        assert_eq!(first.plays[0].event_id, second.plays[0].event_id);
    }

    #[test]
    fn established_and_new_devices_share_the_overlapping_server_ordinal() {
        let established_shadow = AggregateHistoryShadow {
            play_count: 10,
            played_at_unix: Some(100),
            observed_at_unix: 100,
            counter_epoch: 8,
        };
        let latest = song(12, Some("1970-01-01T00:03:20Z"));
        let established = plan_aggregate_history(Some(&established_shadow), &latest, 200);
        assert_eq!(established.plays.len(), 2);

        let new_device_baseline =
            plan_aggregate_history(None, &song(11, Some("1970-01-01T00:02:30Z")), 150);
        let new_device =
            plan_aggregate_history(Some(&new_device_baseline.next_shadow), &latest, 201);
        assert_eq!(new_device.plays.len(), 1);
        assert_eq!(established.plays[1].event_id, new_device.plays[0].event_id);
    }

    #[test]
    fn legacy_shadow_without_counter_epoch_migrates_to_convergent_ids() {
        let legacy: AggregateHistoryShadow = serde_json::from_value(serde_json::json!({
            "play_count": 7,
            "played_at_unix": 100,
            "observed_at_unix": 100
        }))
        .unwrap();
        assert_eq!(legacy.counter_epoch, 0);

        let migrated_elsewhere = AggregateHistoryShadow {
            counter_epoch: 99,
            ..legacy.clone()
        };
        let observation = song(8, Some("1970-01-01T00:03:20Z"));
        let legacy_plan = plan_aggregate_history(Some(&legacy), &observation, 200);
        let migrated_plan = plan_aggregate_history(Some(&migrated_elsewhere), &observation, 201);
        assert_eq!(
            legacy_plan.plays[0].event_id,
            migrated_plan.plays[0].event_id
        );
    }

    #[test]
    fn changed_played_timestamp_without_count_has_a_portable_identity() {
        let mut initial_song = song(0, Some("1970-01-01T00:01:40Z"));
        initial_song.play_count = None;
        let baseline = plan_aggregate_history(None, &initial_song, 100);
        assert!(baseline.plays.is_empty());

        let mut changed_song = song(0, Some("1970-01-01T00:03:20Z"));
        changed_song.play_count = None;
        let changed = plan_aggregate_history(Some(&baseline.next_shadow), &changed_song, 200);
        assert_eq!(changed.plays.len(), 1);
        assert_eq!(changed.plays[0].played_at_unix, 200);
        assert_eq!(changed.next_shadow.play_count, 1);

        let other_device_shadow = AggregateHistoryShadow {
            play_count: 50,
            played_at_unix: Some(100),
            observed_at_unix: 150,
            counter_epoch: 23,
        };
        let other_device = plan_aggregate_history(Some(&other_device_shadow), &changed_song, 201);
        assert_eq!(other_device.plays.len(), 1);
        assert_eq!(changed.plays[0].event_id, other_device.plays[0].event_id);

        let same = plan_aggregate_history(Some(&changed.next_shadow), &changed_song, 300);
        assert!(same.plays.is_empty());
        let count_readback =
            plan_aggregate_history(Some(&changed.next_shadow), &song(1, None), 301);
        assert!(
            count_readback.plays.is_empty(),
            "the later count echo must not duplicate the timestamp-only evidence"
        );
    }
}
