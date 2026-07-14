//! Privacy-preserving rolling write accounting for the managed packet cache.

use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use super::long_form_seek::ROLLING_WRITE_BUDGET_BYTES;

const LEDGER_VERSION: u8 = 1;
const BUCKET_SECS: u64 = 60 * 60;
const WINDOW_SECS: u64 = 24 * BUCKET_SECS;
const LEDGER_MAX_BYTES: u64 = 64 * 1024;
pub(crate) const LEDGER_FILE: &str = "write-budget-v1.json";
/// Persist wear budget ahead of observed writes. The worker rounds whole-lifetime reservations to
/// this quantum, while the actor only consumes already-durable tokens in memory.
const PERSIST_RESERVATION_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Ledger {
    version: u8,
    buckets: Vec<Bucket>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Bucket {
    hour_start_unix: u64,
    bytes: u64,
}

/// One coarse, media-agnostic 24-hour ledger. Any read/write/clock failure exhausts admission
/// rather than silently losing wear accounting.
pub(crate) struct RollingWriteBudget {
    path: Option<PathBuf>,
    ledger: Ledger,
    failed_closed: bool,
    reservation_hour: Option<u64>,
    reservation_remaining: u64,
    pending: Option<PendingReservationState>,
    next_reservation_id: u64,
    #[cfg(test)]
    persist_count: u64,
}

#[derive(Clone)]
struct PendingReservationState {
    id: u64,
    ledger: Ledger,
    hour: u64,
    bytes: u64,
}

pub(crate) enum ReservationPlan {
    Covered,
    Persist(PendingReservation),
}

pub(crate) struct PendingReservation {
    id: u64,
    path: PathBuf,
    ledger: Ledger,
}

pub(crate) struct ReservationCompletion {
    id: u64,
    result: io::Result<()>,
}

impl PendingReservation {
    /// Execute only from a blocking worker. The actor receives [`ReservationCompletion`] and
    /// cannot enable disk writes until this atomic write has durably acknowledged.
    pub(crate) fn persist(self) -> ReservationCompletion {
        let result = crate::util::safe_fs::write_private_atomic_json(&self.path, &self.ledger);
        ReservationCompletion {
            id: self.id,
            result,
        }
    }

    pub(crate) fn completion_without_runtime(self) -> ReservationCompletion {
        ReservationCompletion {
            id: self.id,
            result: Err(io::Error::other(
                "write-budget persistence worker unavailable",
            )),
        }
    }

    #[cfg(test)]
    pub(crate) fn completion_for_test(&self, result: io::Result<()>) -> ReservationCompletion {
        ReservationCompletion {
            id: self.id,
            result,
        }
    }
}

impl RollingWriteBudget {
    pub(crate) fn open(cache_dir: Option<&Path>) -> Self {
        Self::open_at(cache_dir, unix_now())
    }

    fn open_at(cache_dir: Option<&Path>, now: io::Result<u64>) -> Self {
        let Some(path) = cache_dir.map(|dir| dir.join(LEDGER_FILE)) else {
            return Self::failed();
        };
        let Ok(now) = now else {
            return Self::failed_with_path(path);
        };
        let mut ledger =
            match crate::util::safe_fs::read_private_file_limited(&path, LEDGER_MAX_BYTES) {
                Ok(bytes) => match serde_json::from_slice::<Ledger>(&bytes) {
                    Ok(ledger) if ledger.version == LEDGER_VERSION => ledger,
                    Ok(_) | Err(_) => return Self::failed_with_path(path),
                },
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ledger {
                    version: LEDGER_VERSION,
                    buckets: Vec::new(),
                },
                Err(_) => return Self::failed_with_path(path),
            };
        if normalize(&mut ledger, now).is_err() {
            return Self::failed_with_path(path);
        }
        Self {
            path: Some(path),
            ledger,
            failed_closed: false,
            // A reopened process cannot prove how much of the last persisted reservation was
            // consumed before exit, so it conservatively reserves a fresh chunk on first write.
            reservation_hour: None,
            reservation_remaining: 0,
            pending: None,
            next_reservation_id: 0,
            #[cfg(test)]
            persist_count: 0,
        }
    }

    pub(crate) fn bytes(&self) -> u64 {
        if self.failed_closed {
            ROLLING_WRITE_BUDGET_BYTES
        } else {
            self.pending
                .as_ref()
                .map_or(&self.ledger, |pending| &pending.ledger)
                .buckets
                .iter()
                .fold(0u64, |total, bucket| total.saturating_add(bucket.bytes))
        }
    }

    /// Reserve enough durable accounting to cover the whole bounded writer lifetime. This only
    /// prepares an immutable worker payload; it performs no filesystem operation.
    pub(crate) fn prepare_reservation(&mut self, minimum: u64) -> io::Result<ReservationPlan> {
        self.prepare_reservation_at(minimum, unix_now())
    }

    fn prepare_reservation_at(
        &mut self,
        minimum: u64,
        now: io::Result<u64>,
    ) -> io::Result<ReservationPlan> {
        if self.failed_closed {
            return Err(io::Error::other("write budget failed closed"));
        }
        if self.pending.is_some() {
            return Err(io::Error::other("write budget reservation already pending"));
        }
        let Some(path) = self.path.clone() else {
            self.failed_closed = true;
            return Err(io::Error::other("write budget path unavailable"));
        };
        let Ok(now) = now else {
            self.failed_closed = true;
            return Err(io::Error::other("write budget clock unavailable"));
        };
        let mut ledger = self.ledger.clone();
        if normalize(&mut ledger, now).is_err() {
            self.failed_closed = true;
            return Err(io::Error::other("write budget ledger invalid"));
        }
        let hour = hour_start(now);
        if self.reservation_hour != Some(hour) {
            self.reservation_hour = Some(hour);
            self.reservation_remaining = 0;
        }
        if minimum <= self.reservation_remaining {
            return Ok(ReservationPlan::Covered);
        }
        let unreserved = minimum.saturating_sub(self.reservation_remaining);
        let chunks = u128::from(unreserved).div_ceil(u128::from(PERSIST_RESERVATION_BYTES));
        let reservation =
            u64::try_from(chunks * u128::from(PERSIST_RESERVATION_BYTES)).unwrap_or(u64::MAX);
        let accounted = ledger
            .buckets
            .iter()
            .fold(0u64, |total, bucket| total.saturating_add(bucket.bytes));
        if accounted.saturating_add(reservation) > ROLLING_WRITE_BUDGET_BYTES {
            return Err(io::Error::other("rolling write budget exhausted"));
        }
        if let Some(bucket) = ledger
            .buckets
            .iter_mut()
            .find(|bucket| bucket.hour_start_unix == hour)
        {
            bucket.bytes = bucket.bytes.saturating_add(reservation);
        } else {
            ledger.buckets.push(Bucket {
                hour_start_unix: hour,
                bytes: reservation,
            });
            ledger
                .buckets
                .sort_unstable_by_key(|bucket| bucket.hour_start_unix);
        }
        self.next_reservation_id = self.next_reservation_id.wrapping_add(1);
        let id = self.next_reservation_id;
        self.pending = Some(PendingReservationState {
            id,
            ledger: ledger.clone(),
            hour,
            bytes: reservation,
        });
        Ok(ReservationPlan::Persist(PendingReservation {
            id,
            path,
            ledger,
        }))
    }

    pub(crate) fn complete_reservation(&mut self, completion: ReservationCompletion) -> bool {
        let Some(pending) = self.pending.take() else {
            return false;
        };
        if pending.id != completion.id {
            self.failed_closed = true;
            return false;
        }
        if let Err(error) = completion.result {
            self.failed_closed = true;
            tracing::warn!(
                error_kind = ?error.kind(),
                "long-form write-budget ledger failed closed"
            );
            return false;
        }
        self.ledger = pending.ledger;
        self.reservation_hour = Some(pending.hour);
        self.reservation_remaining = self.reservation_remaining.saturating_add(pending.bytes);
        #[cfg(test)]
        {
            self.persist_count = self.persist_count.saturating_add(1);
        }
        true
    }

    /// Consume already-durable reservation tokens. This is the only ledger operation on the mpv
    /// observation path and deliberately performs neither serialization nor filesystem I/O.
    pub(crate) fn record_observed(&mut self, delta: u64) -> u64 {
        self.record_observed_at(delta, unix_now())
    }

    #[cfg(test)]
    pub(crate) fn persistence_count(&self) -> u64 {
        self.persist_count
    }

    fn record_observed_at(&mut self, delta: u64, now: io::Result<u64>) -> u64 {
        if delta == 0 || self.failed_closed {
            return self.bytes();
        }
        let Ok(now) = now else {
            self.failed_closed = true;
            return self.bytes();
        };
        if self.reservation_hour != Some(hour_start(now)) || delta > self.reservation_remaining {
            self.failed_closed = true;
            return self.bytes();
        }
        self.reservation_remaining -= delta;
        self.bytes()
    }

    fn failed() -> Self {
        Self {
            path: None,
            ledger: Ledger {
                version: LEDGER_VERSION,
                buckets: Vec::new(),
            },
            failed_closed: true,
            reservation_hour: None,
            reservation_remaining: 0,
            pending: None,
            next_reservation_id: 0,
            #[cfg(test)]
            persist_count: 0,
        }
    }

    fn failed_with_path(path: PathBuf) -> Self {
        Self {
            path: Some(path),
            ..Self::failed()
        }
    }
}

fn unix_now() -> io::Result<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .map_err(io::Error::other)
}

fn hour_start(now: u64) -> u64 {
    now - now % BUCKET_SECS
}

fn normalize(ledger: &mut Ledger, now: u64) -> Result<(), ()> {
    if ledger.buckets.len() > 48 {
        return Err(());
    }
    let current_hour = hour_start(now);
    if ledger.buckets.iter().any(|bucket| {
        bucket.hour_start_unix % BUCKET_SECS != 0
            || bucket.hour_start_unix > current_hour.saturating_add(BUCKET_SECS)
    }) {
        return Err(());
    }
    let oldest = now.saturating_sub(WINDOW_SECS);
    ledger
        .buckets
        .retain(|bucket| bucket.hour_start_unix.saturating_add(BUCKET_SECS) > oldest);
    ledger
        .buckets
        .sort_unstable_by_key(|bucket| bucket.hour_start_unix);
    if ledger
        .buckets
        .windows(2)
        .any(|pair| pair[0].hour_start_unix == pair[1].hour_start_unix)
    {
        return Err(());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

    fn private_dir(label: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "ytt-long-form-budget-{label}-{}-{}",
            std::process::id(),
            NEXT_DIR.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&path);
        crate::util::safe_fs::ensure_private_dir(&path).unwrap();
        path
    }

    #[test]
    fn malformed_or_future_ledger_fails_closed() {
        let dir = private_dir("invalid");
        let path = dir.join(LEDGER_FILE);
        crate::util::safe_fs::write_private_atomic(
            &path,
            br#"{"version":1,"buckets":[{"hour_start_unix":999999999999,"bytes":1}]}"#,
        )
        .unwrap();
        let budget = RollingWriteBudget::open_at(Some(&dir), Ok(BUCKET_SECS * 100));
        assert_eq!(budget.bytes(), ROLLING_WRITE_BUDGET_BYTES);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn keeps_only_the_rolling_window_and_no_media_identity() {
        let dir = private_dir("window");
        let path = dir.join(LEDGER_FILE);
        let now = BUCKET_SECS * 100;
        let ledger = Ledger {
            version: LEDGER_VERSION,
            buckets: vec![
                Bucket {
                    hour_start_unix: hour_start(now - WINDOW_SECS - BUCKET_SECS),
                    bytes: 50,
                },
                Bucket {
                    hour_start_unix: hour_start(now - BUCKET_SECS),
                    bytes: 75,
                },
            ],
        };
        crate::util::safe_fs::write_private_atomic_json(&path, &ledger).unwrap();
        let budget = RollingWriteBudget::open_at(Some(&dir), Ok(now));
        assert_eq!(budget.bytes(), 75);
        let bytes = std::fs::read(path).unwrap();
        let text = String::from_utf8(bytes).unwrap();
        assert!(!text.contains("url"));
        assert!(!text.contains("video"));
        assert!(!text.contains("title"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn observation_consumes_durable_reservation_without_persistence_or_io() {
        let dir = private_dir("reservation");
        let now = BUCKET_SECS * 100;
        let mut budget = RollingWriteBudget::open_at(Some(&dir), Ok(now));
        let ledger_path = dir.join(LEDGER_FILE);

        let ReservationPlan::Persist(persistence) = budget
            .prepare_reservation_at(20 * 1024 * 1024, Ok(now))
            .unwrap()
        else {
            panic!("new budget needs a durable reservation");
        };
        assert!(
            !ledger_path.exists(),
            "preparation performs no filesystem IO"
        );
        assert_eq!(budget.persist_count, 0);
        assert_eq!(budget.bytes(), 2 * PERSIST_RESERVATION_BYTES);
        let completion = persistence.persist();
        assert!(budget.complete_reservation(completion));
        assert_eq!(budget.persist_count, 1);
        let persisted = std::fs::read(&ledger_path).unwrap();
        for _ in 0..1_000 {
            budget.record_observed_at(4_096, Ok(now));
        }
        assert_eq!(budget.persist_count, 1);
        assert_eq!(std::fs::read(&ledger_path).unwrap(), persisted);
        let accounted = budget.bytes();
        drop(budget);

        let mut reopened = RollingWriteBudget::open_at(Some(&dir), Ok(now));
        assert_eq!(reopened.bytes(), accounted);
        assert_eq!(reopened.reservation_remaining, 0);
        assert!(matches!(
            reopened.prepare_reservation_at(1, Ok(now)).unwrap(),
            ReservationPlan::Persist(_)
        ));
        assert_eq!(reopened.bytes(), accounted + PERSIST_RESERVATION_BYTES);
        assert_eq!(reopened.persist_count, 0);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn failed_reservation_fails_closed_and_cannot_admit_again() {
        let dir = private_dir("failed-reservation");
        let now = BUCKET_SECS * 100;
        let mut budget = RollingWriteBudget::open_at(Some(&dir), Ok(now));
        let ReservationPlan::Persist(persistence) =
            budget.prepare_reservation_at(1, Ok(now)).unwrap()
        else {
            panic!("new budget needs a durable reservation");
        };
        let failure =
            persistence.completion_for_test(Err(io::Error::other("injected persistence failure")));
        assert!(!budget.complete_reservation(failure));
        assert_eq!(budget.bytes(), ROLLING_WRITE_BUDGET_BYTES);
        assert!(budget.prepare_reservation_at(1, Ok(now)).is_err());
        let _ = std::fs::remove_dir_all(dir);
    }
}
