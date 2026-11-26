//! Default-route network reachability classification and native OHOS change monitoring.

use std::sync::{Mutex, Weak};

use nsh_core::platform::{Network, NetworkKind};

/// Abstracted link bearer kind mapped from OpenHarmony's `NetBearerType`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Bearer {
    Wifi,
    Cellular,
    Ethernet,
    Other,
}

/// Classifies OpenHarmony default network state into [`Network`].
pub(crate) fn classify_network(
    has_default: bool,
    capabilities: Option<(&[Bearer], bool)>,
    metered: Option<bool>,
) -> Network {
    if !has_default {
        return Network::new(NetworkKind::None);
    }
    let Some((bearers, not_metered_cap)) = capabilities else {
        return Network::new(NetworkKind::Unknown).expensive(metered.unwrap_or(false));
    };
    let kind = if bearers.contains(&Bearer::Ethernet) {
        NetworkKind::Wired
    } else if bearers.contains(&Bearer::Wifi) {
        NetworkKind::Wifi
    } else if bearers.contains(&Bearer::Cellular) {
        NetworkKind::Cellular
    } else if !bearers.is_empty() {
        NetworkKind::Other
    } else {
        NetworkKind::Unknown
    };
    let expensive = metered.unwrap_or(!not_metered_cap || kind == NetworkKind::Cellular);
    Network::new(kind).expensive(expensive)
}

/// Platform-independent subscriber / snapshot state machine shared by the OHOS
/// monitor and host regression tests.
///
/// Invariants:
/// - `last` always represents the latest [`Network`] snapshot already published
///   to all live existing subscribers.
/// - Subscribing a new window never silently overwrites `last`: if the probed
///   network differs from `last`, existing live subscribers receive the change
///   through the same serialized publication path before the new subscriber is
///   initialized.
/// - OS callback registration is serialized on a separate lock and never runs
///   while `state` is locked, avoiding self-deadlock on synchronous callbacks.
struct MonitorState<S> {
    last: Option<Network>,
    subscribers: Vec<Weak<S>>,
}

impl<S> Default for MonitorState<S> {
    fn default() -> Self {
        Self {
            last: None,
            subscribers: Vec::new(),
        }
    }
}

struct MonitorCore<S, H> {
    registration: Mutex<Option<H>>,
    state: Mutex<MonitorState<S>>,
}

impl<S, H> Default for MonitorCore<S, H> {
    fn default() -> Self {
        Self {
            registration: Mutex::new(None),
            state: Mutex::new(MonitorState::default()),
        }
    }
}

impl<S, H> MonitorCore<S, H> {
    /// Ensures the single process-wide system callback is registered without
    /// holding `self.state`.
    fn ensure_registered(&self, register: impl FnOnce() -> Result<H, String>) {
        let mut reg = self.registration.lock().unwrap_or_else(|e| e.into_inner());
        if reg.is_none() {
            match register() {
                Ok(handle) => *reg = Some(handle),
                Err(error) => {
                    log::warn!("OHOS default network callback registration failed: {error}");
                }
            }
        }
    }

    /// Subscribes `route`, whose runtime already saw `initial_network`.
    ///
    /// Probes and delivers under `self.state` so concurrent `notify()` calls
    /// cannot interleave out of order (`deliver` must be non-blocking and must
    /// not re-enter `MonitorCore`).
    fn subscribe(
        &self,
        route: Weak<S>,
        initial_network: Network,
        is_live: impl Fn(&S) -> bool,
        probe: impl FnOnce() -> Network,
        deliver: impl Fn(&S, Network),
    ) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state
            .subscribers
            .retain(|weak| weak.upgrade().is_some_and(|s| is_live(&s)));
        let current = probe();
        let had_subscribers = !state.subscribers.is_empty();
        if had_subscribers {
            if state.last != Some(current) {
                state.last = Some(current);
                for existing in state.subscribers.iter().filter_map(Weak::upgrade) {
                    deliver(&existing, current);
                }
            }
        } else {
            state.last = Some(current);
        }
        if let Some(new_sub) = route.upgrade()
            && is_live(&new_sub)
        {
            state.subscribers.push(route);
            if initial_network != current {
                deliver(&new_sub, current);
            }
        }
    }

    /// Publishes an OS network change callback to all live subscribers when
    /// the probed state differs from `last`.
    fn notify(
        &self,
        is_live: impl Fn(&S) -> bool,
        probe: impl FnOnce() -> Network,
        deliver: impl Fn(&S, Network),
    ) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state
            .subscribers
            .retain(|weak| weak.upgrade().is_some_and(|s| is_live(&s)));
        if state.subscribers.is_empty() {
            state.last = None;
            return;
        }
        let current = probe();
        if state.last == Some(current) {
            return;
        }
        state.last = Some(current);
        for subscriber in state.subscribers.iter().filter_map(Weak::upgrade) {
            deliver(&subscriber, current);
        }
    }
}

#[cfg(target_env = "ohos")]
mod ohos {
    use std::sync::{OnceLock, Weak, atomic::Ordering};

    use nsh_core::platform::Network;
    use ohos_net_connection_binding::{
        CallbackId, NetBearerType, NetCap, NetConnCallback, get_default_net, has_default_net,
        is_default_net_metered,
    };

    use super::{Bearer, MonitorCore, classify_network};
    use crate::app::runtime::{Dispatcher, Message};

    static MONITOR: OnceLock<MonitorCore<Dispatcher, CallbackId>> = OnceLock::new();

    fn monitor() -> &'static MonitorCore<Dispatcher, CallbackId> {
        MONITOR.get_or_init(MonitorCore::default)
    }

    fn is_live(dispatcher: &Dispatcher) -> bool {
        dispatcher.live.load(Ordering::Acquire)
    }

    fn deliver(dispatcher: &Dispatcher, network: Network) {
        dispatcher.send(Message::HostNetwork(network));
    }

    fn map_bearer(bearer: NetBearerType) -> Bearer {
        match bearer {
            NetBearerType::Wifi => Bearer::Wifi,
            NetBearerType::Cellular => Bearer::Cellular,
            NetBearerType::Ethernet => Bearer::Ethernet,
            NetBearerType::Bluetooth | NetBearerType::Vpn | NetBearerType::Unknown(_) => {
                Bearer::Other
            }
        }
    }

    pub(super) fn current_network() -> Network {
        let Ok(has_default) = has_default_net() else {
            return Network::default();
        };
        if !has_default {
            return classify_network(false, None, None);
        }
        let bearers_and_cap = get_default_net()
            .and_then(|handle| handle.net_capabilities())
            .ok()
            .map(|caps| {
                let not_metered = caps.net_caps.contains(&NetCap::NotMetered);
                let bearers: Vec<Bearer> = caps.bearer_types.into_iter().map(map_bearer).collect();
                (bearers, not_metered)
            });
        let metered = is_default_net_metered().ok();
        match bearers_and_cap {
            Some((bearers, not_metered)) => {
                classify_network(true, Some((&bearers, not_metered)), metered)
            }
            None => classify_network(true, None, metered),
        }
    }

    fn notify() {
        let _ = std::panic::catch_unwind(|| {
            monitor().notify(is_live, current_network, deliver);
        });
    }

    unsafe extern "C" fn on_handle<H>(_: *mut H) {
        notify();
    }

    unsafe extern "C" fn on_caps<H, C>(_: *mut H, _: *mut C) {
        notify();
    }

    unsafe extern "C" fn on_unavailable() {
        notify();
    }

    pub(super) fn subscribe(route: Weak<Dispatcher>, initial_network: Network) {
        let monitor = monitor();
        monitor.ensure_registered(|| {
            let callback = NetConnCallback {
                on_network_available: Some(on_handle),
                on_net_capabilities_change: Some(on_caps),
                on_net_lost: Some(on_handle),
                on_net_unavailable: Some(on_unavailable),
                ..Default::default()
            };
            callback
                .register_default_net_conn_callback()
                .map_err(|error| error.to_string())
        });
        monitor.subscribe(route, initial_network, is_live, current_network, deliver);
    }
}

#[cfg(target_env = "ohos")]
pub(crate) fn current_network() -> Network {
    ohos::current_network()
}

#[cfg(target_env = "ohos")]
pub(crate) fn subscribe(
    route: std::sync::Weak<crate::app::runtime::Dispatcher>,
    initial_network: Network,
) {
    ohos::subscribe(route, initial_network);
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    };

    use super::*;

    struct FakeSub {
        live: AtomicBool,
        events: Mutex<Vec<Network>>,
    }

    impl FakeSub {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                live: AtomicBool::new(true),
                events: Mutex::new(Vec::new()),
            })
        }

        fn events(&self) -> Vec<Network> {
            self.events.lock().unwrap().clone()
        }
    }

    fn live(s: &FakeSub) -> bool {
        s.live.load(Ordering::Acquire)
    }

    fn send(s: &FakeSub, net: Network) {
        s.events.lock().unwrap().push(net);
    }

    #[test]
    fn offline_returns_none_network() {
        let net = classify_network(false, Some((&[Bearer::Wifi], true)), Some(false));
        assert_eq!(net, Network::new(NetworkKind::None));
        assert!(!net.online());
    }

    #[test]
    fn capabilities_prioritize_wired_then_wifi_then_cellular() {
        assert_eq!(
            classify_network(true, Some((&[Bearer::Wifi, Bearer::Ethernet], true)), None),
            Network::new(NetworkKind::Wired).expensive(false)
        );
        assert_eq!(
            classify_network(true, Some((&[Bearer::Cellular, Bearer::Wifi], true)), None),
            Network::new(NetworkKind::Wifi).expensive(false)
        );
        assert_eq!(
            classify_network(true, Some((&[Bearer::Cellular], true)), None),
            Network::new(NetworkKind::Cellular).expensive(true)
        );
        assert_eq!(
            classify_network(true, Some((&[Bearer::Other], false)), None),
            Network::new(NetworkKind::Other).expensive(true)
        );
        assert_eq!(
            classify_network(true, Some((&[Bearer::Wifi], false)), Some(false)),
            Network::new(NetworkKind::Wifi).expensive(false)
        );
    }

    #[test]
    fn missing_capabilities_fall_back_to_unknown() {
        assert_eq!(
            classify_network(true, None, Some(true)),
            Network::new(NetworkKind::Unknown).expensive(true)
        );
        assert_eq!(
            classify_network(true, Some((&[], true)), None),
            Network::new(NetworkKind::Unknown).expensive(false)
        );
    }

    #[test]
    fn new_subscriber_does_not_swallow_pending_change_for_existing_window() {
        let a = Network::new(NetworkKind::Wifi);
        let b = Network::new(NetworkKind::Cellular).expensive(true);
        let current = Mutex::new(a);
        let probe = || *current.lock().unwrap();

        let monitor: MonitorCore<FakeSub, ()> = MonitorCore::default();
        let w1 = FakeSub::new();
        monitor.subscribe(Arc::downgrade(&w1), a, live, probe, send);
        assert!(w1.events().is_empty());

        // Network changes A -> B before the OS callback fires; W2 subscribes first.
        *current.lock().unwrap() = b;
        let w2 = FakeSub::new();
        monitor.subscribe(Arc::downgrade(&w2), b, live, probe, send);

        // Delayed OS callback for B arrives afterwards.
        monitor.notify(live, probe, send);

        // W1 received B exactly once; W2 already started with B and got no duplicate.
        assert_eq!(w1.events(), vec![b]);
        assert!(w2.events().is_empty());

        // Rapid B -> A -> B transitions reach both windows in exact order.
        *current.lock().unwrap() = a;
        monitor.notify(live, probe, send);
        // Duplicate A callback is deduplicated.
        monitor.notify(live, probe, send);
        *current.lock().unwrap() = b;
        monitor.notify(live, probe, send);
        assert_eq!(w1.events(), vec![b, a, b]);
        assert_eq!(w2.events(), vec![a, b]);
    }

    #[test]
    fn synchronous_callback_during_registration_does_not_deadlock() {
        let a = Network::new(NetworkKind::Wifi);
        let b = Network::new(NetworkKind::None);
        let current = Mutex::new(a);
        let probe = || *current.lock().unwrap();
        let monitor: MonitorCore<FakeSub, ()> = MonitorCore::default();

        // Simulate OS invoking `notify()` synchronously inside register().
        monitor.ensure_registered(|| {
            monitor.notify(live, probe, send);
            Ok(())
        });

        let w1 = FakeSub::new();
        // If W1 captured A before subscribing and network moved to B right before
        // subscribe, W1 receives B cleanly.
        *current.lock().unwrap() = b;
        monitor.subscribe(Arc::downgrade(&w1), a, live, probe, send);
        assert_eq!(w1.events(), vec![b]);

        // Unsubscribing / dropping last window clears last so a fresh window
        // starts clean.
        w1.live.store(false, Ordering::Release);
        monitor.notify(live, probe, send);
        let w2 = FakeSub::new();
        monitor.subscribe(Arc::downgrade(&w2), b, live, probe, send);
        assert!(w2.events().is_empty());
    }
}
