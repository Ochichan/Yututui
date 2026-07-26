//! Pure mapping between exact OpenSubsonic rating observations and YuTuTui ratings.

use serde::{Deserialize, Serialize};

use crate::personal_state::Rating;

/// Exact values observed from an OpenSubsonic `Child`.
///
/// `user_rating` deliberately remains an `i64`: malformed values must survive a read/write
/// round-trip in the server shadow instead of being silently normalized.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawServerRating {
    pub user_rating: Option<i64>,
    pub starred: bool,
}

/// Map a server observation into YuTuTui's tri-state rating.
///
/// Ratings `1` and `5` are authoritative even when the star flag contradicts them. Ratings
/// `2..=4`, missing/zero ratings, and malformed values use the star flag.
pub fn map_server_rating(raw: RawServerRating) -> Rating {
    match raw.user_rating {
        Some(1) => Rating::Disliked,
        Some(5) => Rating::Liked,
        _ if raw.starred => Rating::Liked,
        _ => Rating::Neutral,
    }
}

/// Return the normalized server values written after an explicit YuTuTui rating change.
///
/// Callers still have to perform the two server mutations in order (`setRating`, then
/// `star`/`unstar`) and verify the result with a readback.
pub fn canonical_server_rating(rating: Rating) -> RawServerRating {
    match rating {
        Rating::Liked => RawServerRating {
            user_rating: Some(5),
            starred: true,
        },
        Rating::Disliked => RawServerRating {
            user_rating: Some(1),
            starred: false,
        },
        Rating::Neutral => RawServerRating {
            user_rating: Some(0),
            starred: false,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_supported_and_malformed_combination_has_the_fixed_mapping() {
        let cases = [
            (None, false, Rating::Neutral),
            (None, true, Rating::Liked),
            (Some(0), false, Rating::Neutral),
            (Some(0), true, Rating::Liked),
            (Some(1), false, Rating::Disliked),
            (Some(1), true, Rating::Disliked),
            (Some(2), false, Rating::Neutral),
            (Some(2), true, Rating::Liked),
            (Some(3), false, Rating::Neutral),
            (Some(3), true, Rating::Liked),
            (Some(4), false, Rating::Neutral),
            (Some(4), true, Rating::Liked),
            (Some(5), false, Rating::Liked),
            (Some(5), true, Rating::Liked),
            (Some(-1), false, Rating::Neutral),
            (Some(-1), true, Rating::Liked),
            (Some(6), false, Rating::Neutral),
            (Some(6), true, Rating::Liked),
        ];

        for (user_rating, starred, expected) in cases {
            assert_eq!(
                map_server_rating(RawServerRating {
                    user_rating,
                    starred,
                }),
                expected,
                "user_rating={user_rating:?}, starred={starred}"
            );
        }
    }

    #[test]
    fn explicit_local_ratings_have_one_canonical_server_representation() {
        assert_eq!(
            canonical_server_rating(Rating::Liked),
            RawServerRating {
                user_rating: Some(5),
                starred: true,
            }
        );
        assert_eq!(
            canonical_server_rating(Rating::Disliked),
            RawServerRating {
                user_rating: Some(1),
                starred: false,
            }
        );
        assert_eq!(
            canonical_server_rating(Rating::Neutral),
            RawServerRating {
                user_rating: Some(0),
                starred: false,
            }
        );
    }
}
