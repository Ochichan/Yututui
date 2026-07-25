//! Bounded local-address polling for automatic sync reconnect wakeups.
//!
//! This watcher never probes an endpoint or handles credentials. It only compares addresses
//! reported by the operating system and turns an added usable address into one coalesced wakeup.

use std::collections::HashSet;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::runtime::Handle;
use tokio::sync::Notify;
use tokio::task::JoinHandle;

const POLL_INTERVAL: Duration = Duration::from_secs(2);
const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(2);

/// Owns the single polling task and coalesces its observations into owner-lane wakeups.
pub(crate) struct NetworkChangeWatch {
    task: JoinHandle<()>,
    activation: Arc<Activation>,
    wake: Arc<CoalescedWake>,
}

#[derive(Default)]
struct Activation {
    active: AtomicBool,
    changed: Notify,
}

impl Activation {
    fn set(&self, active: bool) {
        if self.active.swap(active, Ordering::AcqRel) != active {
            self.changed.notify_one();
        }
    }

    fn is_active(&self) -> bool {
        self.active.load(Ordering::Acquire)
    }

    async fn wait_for(&self, active: bool) {
        loop {
            let notified = self.changed.notified();
            if self.is_active() == active {
                return;
            }
            notified.await;
        }
    }
}

#[derive(Default)]
struct CoalescedWake {
    pending: AtomicBool,
    changed: Notify,
}

impl CoalescedWake {
    fn signal(&self) {
        if !self.pending.swap(true, Ordering::AcqRel) {
            self.changed.notify_one();
        }
    }

    async fn wait(&self) {
        loop {
            let notified = self.changed.notified();
            if self.pending.swap(false, Ordering::AcqRel) {
                return;
            }
            notified.await;
        }
    }

    fn clear(&self) {
        self.pending.store(false, Ordering::Release);
    }
}

#[derive(Default)]
struct AddressSetReducer {
    previous: Option<HashSet<IpAddr>>,
}

impl AddressSetReducer {
    fn observe(&mut self, current: HashSet<IpAddr>) -> bool {
        let reconnect = self
            .previous
            .as_ref()
            .is_some_and(|previous| current.iter().any(|address| !previous.contains(address)));
        self.previous = Some(current);
        reconnect
    }
}

impl NetworkChangeWatch {
    /// Start one parked task immediately, or leave the caller on its timed fallback.
    pub(crate) fn start() -> Option<Self> {
        let runtime = match Handle::try_current() {
            Ok(runtime) => runtime,
            Err(_) => {
                watcher_unavailable();
                return None;
            }
        };
        let activation = Arc::new(Activation::default());
        let wake = Arc::new(CoalescedWake::default());
        let task_activation = Arc::clone(&activation);
        let task_wake = Arc::clone(&wake);
        let task_runtime = runtime.clone();
        let task = runtime.spawn(async move {
            watch_networks(task_runtime, task_activation, task_wake).await;
        });
        Some(Self {
            task,
            activation,
            wake,
        })
    }

    /// Wait for the next coalesced usable-address addition.
    pub(crate) async fn changed(&self) {
        self.activation.set(true);
        self.wake.wait().await;
    }

    /// Stop address snapshots while automatic sync is not waiting on connectivity.
    pub(crate) fn park(&self) {
        self.activation.set(false);
        self.wake.clear();
    }

    /// A disabled watcher remains pending while the scheduler's timed fallback stays active.
    pub(crate) async fn changed_or_pending(watch: Option<&Self>) {
        let Some(watch) = watch else {
            std::future::pending::<()>().await;
            return;
        };
        watch.changed().await;
    }
}

impl Drop for NetworkChangeWatch {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn watch_networks(runtime: Handle, activation: Arc<Activation>, wake: Arc<CoalescedWake>) {
    loop {
        activation.wait_for(true).await;
        let Some((mut networks, initial)) =
            refresh_snapshot(&runtime, sysinfo::Networks::new()).await
        else {
            watcher_unavailable();
            return;
        };
        if !activation.is_active() {
            continue;
        }
        let mut reducer = AddressSetReducer::default();
        let initial_is_reconnect = reducer.observe(initial);
        debug_assert!(!initial_is_reconnect);
        let start = tokio::time::Instant::now() + POLL_INTERVAL;
        let mut interval = tokio::time::interval_at(start, POLL_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                biased;
                _ = activation.wait_for(false) => break,
                _ = interval.tick() => {
                    let Some((refreshed, current)) =
                        refresh_snapshot(&runtime, networks).await
                    else {
                        watcher_unavailable();
                        return;
                    };
                    networks = refreshed;
                    if !activation.is_active() {
                        break;
                    }
                    if reducer.observe(current) {
                        wake.signal();
                        if activation.is_active() {
                            activation.set(false);
                        } else {
                            wake.clear();
                        }
                        break;
                    }
                }
            }
        }
    }
}

async fn refresh_snapshot(
    runtime: &Handle,
    mut networks: sysinfo::Networks,
) -> Option<(sysinfo::Networks, HashSet<IpAddr>)> {
    let refresh = runtime.spawn_blocking(move || {
        networks.refresh(true);
        let mut addresses = HashSet::new();
        for network in networks.values() {
            extend_usable_addresses(
                &mut addresses,
                network.operational_state(),
                network.ip_networks().iter().map(|network| network.addr),
            );
        }
        (networks, addresses)
    });
    tokio::time::timeout(SNAPSHOT_TIMEOUT, refresh)
        .await
        .ok()?
        .ok()
}

fn watcher_unavailable() {
    tracing::debug!("network-change watcher unavailable; automatic sync will use timed retry");
}

fn extend_usable_addresses(
    output: &mut HashSet<IpAddr>,
    state: sysinfo::InterfaceOperationalState,
    addresses: impl IntoIterator<Item = IpAddr>,
) {
    if potentially_operational(state) {
        output.extend(addresses.into_iter().filter(|address| usable_ip(*address)));
    }
}

fn potentially_operational(state: sysinfo::InterfaceOperationalState) -> bool {
    !matches!(
        state,
        sysinfo::InterfaceOperationalState::Down
            | sysinfo::InterfaceOperationalState::Testing
            | sysinfo::InterfaceOperationalState::NotPresent
            | sysinfo::InterfaceOperationalState::LowerLayerDown
    )
}

fn usable_ip(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => usable_ipv4(address),
        IpAddr::V6(address) => address
            .to_ipv4_mapped()
            .map_or_else(|| usable_ipv6(address), usable_ipv4),
    }
}

fn usable_ipv4(address: Ipv4Addr) -> bool {
    !address.is_unspecified()
        && !address.is_loopback()
        && !address.is_multicast()
        && !address.is_link_local()
        && !address.is_broadcast()
}

fn usable_ipv6(address: Ipv6Addr) -> bool {
    !address.is_unspecified()
        && !address.is_loopback()
        && !address.is_multicast()
        && !address.is_unicast_link_local()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addresses(values: &[&str]) -> HashSet<IpAddr> {
        values
            .iter()
            .map(|address| address.parse().unwrap())
            .collect()
    }

    #[test]
    fn initial_snapshot_is_baseline_only() {
        let mut reducer = AddressSetReducer::default();

        assert!(!reducer.observe(addresses(&["10.0.0.20", "fd00::20"])));
        assert!(!reducer.observe(addresses(&["10.0.0.20", "fd00::20"])));
    }

    #[test]
    fn added_address_signals_reconnect() {
        let mut reducer = AddressSetReducer::default();
        reducer.observe(addresses(&["10.0.0.20"]));

        assert!(reducer.observe(addresses(&["10.0.0.20", "192.168.1.20"])));
    }

    #[test]
    fn removals_do_not_signal_but_reappearance_does() {
        let mut reducer = AddressSetReducer::default();
        reducer.observe(addresses(&["10.0.0.20"]));

        assert!(!reducer.observe(HashSet::new()));
        assert!(reducer.observe(addresses(&["10.0.0.20"])));
    }

    #[test]
    fn down_to_up_with_the_same_address_signals_reconnect() {
        let address: IpAddr = "10.0.0.20".parse().unwrap();
        let snapshot = |state| {
            let mut output = HashSet::new();
            extend_usable_addresses(&mut output, state, [address]);
            output
        };
        let mut reducer = AddressSetReducer::default();

        assert!(!reducer.observe(snapshot(sysinfo::InterfaceOperationalState::Down)));
        assert!(reducer.observe(snapshot(sysinfo::InterfaceOperationalState::Up)));
    }

    #[test]
    fn operational_state_filter_matches_sysinfo_guidance() {
        for state in [
            sysinfo::InterfaceOperationalState::Up,
            sysinfo::InterfaceOperationalState::Dormant,
            sysinfo::InterfaceOperationalState::Unknown,
        ] {
            assert!(potentially_operational(state));
        }
        for state in [
            sysinfo::InterfaceOperationalState::Down,
            sysinfo::InterfaceOperationalState::Testing,
            sysinfo::InterfaceOperationalState::NotPresent,
            sysinfo::InterfaceOperationalState::LowerLayerDown,
        ] {
            assert!(!potentially_operational(state));
        }
    }

    #[test]
    fn unusable_addresses_are_filtered() {
        for address in [
            "0.0.0.0",
            "127.0.0.1",
            "169.254.1.1",
            "224.0.0.1",
            "255.255.255.255",
            "::",
            "::1",
            "fe80::1",
            "ff02::1",
            "::ffff:127.0.0.1",
        ] {
            assert!(
                !usable_ip(address.parse().unwrap()),
                "{address} must not wake automatic sync"
            );
        }
    }

    #[test]
    fn private_and_global_addresses_are_usable() {
        for address in [
            "10.0.0.20",
            "192.168.1.20",
            "1.1.1.1",
            "fd00::20",
            "2001:4860:4860::8888",
        ] {
            assert!(
                usable_ip(address.parse().unwrap()),
                "{address} must be eligible for a reconnect wakeup"
            );
        }
    }

    #[tokio::test]
    async fn clearing_a_ready_wake_discards_the_stale_edge() {
        let wake = CoalescedWake::default();
        wake.signal();
        wake.clear();

        let mut waiting = Box::pin(wake.wait());
        assert!(futures::poll!(waiting.as_mut()).is_pending());

        wake.signal();
        waiting.await;
    }
}
