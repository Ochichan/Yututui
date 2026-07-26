//! Effective OpenSubsonic rating winners for the server projection bridge.

use std::collections::{BTreeMap, BTreeSet};

use super::legacy::rating_from_legacy;
use super::reducer::stamp_order;
use super::{
    CausalStamp, Operation, OperationOrigin, PersonalStateError, PersonalStateV2, PortableTrack,
    PortableTrackKey, Rating, project,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenSubsonicRatingWinner {
    pub operation_id: String,
    pub track: PortableTrack,
    pub rating: Rating,
    pub origin: OperationOrigin,
}

struct WinnerRegister {
    stamp: CausalStamp,
    operation_id: String,
    origin: OperationOrigin,
    track: PortableTrack,
    rating: Rating,
    from_baseline: bool,
}

impl WinnerRegister {
    fn explicit(envelope: &super::OperationEnvelope, track: PortableTrack, rating: Rating) -> Self {
        Self {
            stamp: envelope.stamp.clone(),
            operation_id: envelope.operation_id.clone(),
            origin: envelope.origin.clone(),
            track,
            rating,
            from_baseline: false,
        }
    }

    fn baseline(envelope: &super::OperationEnvelope, track: PortableTrack, rating: Rating) -> Self {
        Self {
            stamp: envelope.stamp.clone(),
            operation_id: envelope.operation_id.clone(),
            origin: envelope.origin.clone(),
            track,
            rating,
            from_baseline: true,
        }
    }

    fn accepts(&self, envelope: &super::OperationEnvelope) -> bool {
        self.from_baseline
            || stamp_order(
                &envelope.stamp,
                &envelope.operation_id,
                &self.stamp,
                &self.operation_id,
            )
            .is_gt()
    }
}

/// Return only effective, non-evicted exact server ratings together with the causal operation
/// that won. The bridge uses the operation ID as its durable echo/projection watermark.
pub fn open_subsonic_rating_winners(
    state: &PersonalStateV2,
) -> Result<Vec<OpenSubsonicRatingWinner>, PersonalStateError> {
    state.validate()?;
    let mut registers = BTreeMap::<PortableTrackKey, WinnerRegister>::new();
    for envelope in &state.operations {
        match &envelope.operation {
            Operation::LegacyBaseline { baseline } => {
                for (key, (track, rating)) in
                    rating_from_legacy(&baseline.favorites, &baseline.signals)
                {
                    registers
                        .entry(key)
                        .or_insert_with(|| WinnerRegister::baseline(envelope, track, rating));
                }
            }
            Operation::SetRating { track, rating } => match registers.get_mut(&track.key) {
                Some(current) if current.accepts(envelope) => {
                    *current = WinnerRegister::explicit(envelope, track.clone(), *rating);
                }
                None => {
                    registers.insert(
                        track.key.clone(),
                        WinnerRegister::explicit(envelope, track.clone(), *rating),
                    );
                }
                Some(_) => {}
            },
            _ => {}
        }
    }

    let projected = project(state)?;
    let liked = projected
        .legacy
        .favorites
        .iter()
        .map(|track| track.key.clone())
        .collect::<BTreeSet<_>>();
    let disliked = projected
        .legacy
        .signals
        .tracks
        .iter()
        .filter(|(_, signal)| signal.disliked)
        .map(|(key, _)| key.clone())
        .collect::<BTreeSet<_>>();

    Ok(registers
        .into_iter()
        .filter_map(|(key, winner)| {
            if !matches!(key, PortableTrackKey::OpenSubsonic { .. })
                || match winner.rating {
                    Rating::Liked => !liked.contains(&key),
                    Rating::Disliked => !disliked.contains(&key),
                    Rating::Neutral => false,
                }
            {
                return None;
            }
            Some(OpenSubsonicRatingWinner {
                operation_id: winner.operation_id,
                track: winner.track,
                rating: winner.rating,
                origin: winner.origin,
            })
        })
        .collect())
}
