//! Default-route network reachability classification and native OHOS change monitoring.

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

#[cfg(target_env = "ohos")]
mod ohos {
    use std::sync::{Mutex, OnceLock, Weak};

    use nsh_core::platform::Network;
    use ohos_net_connection_binding::{
        CallbackId, NetBearerType, NetCap, NetConnCallback, get_default_net, has_default_net,
        is_default_net_metered,
    };

    use super::{Bearer, classify_network};
    use crate::app::runtime::{Dispatcher, Message};

    #[derive(Default)]
    struct MonitorState {
        callback_id: Option<CallbackId>,
        last: Option<Network>,
        subscribers: Vec<Weak<Dispatcher>>,
    }

    static MONITOR: OnceLock<Mutex<MonitorState>> = OnceLock::new();

    fn monitor() -> &'static Mutex<MonitorState> {
        MONITOR.get_or_init(|| Mutex::new(MonitorState::default()))
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
            let network = current_network();
            let recipients = {
                let mut state = monitor().lock().unwrap_or_else(|e| e.into_inner());
                state.subscribers.retain(|weak| {
                    weak.upgrade()
                        .is_some_and(|d| d.live.load(std::sync::atomic::Ordering::Acquire))
                });
                if state.last == Some(network) {
                    return;
                }
                state.last = Some(network);
                state
                    .subscribers
                    .iter()
                    .filter_map(Weak::upgrade)
                    .collect::<Vec<_>>()
            };
            for route in recipients {
                route.send(Message::HostNetwork(network));
            }
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

    pub(super) fn subscribe(route: Weak<Dispatcher>) {
        let mut state = monitor().lock().unwrap_or_else(|e| e.into_inner());
        state.subscribers.retain(|weak| {
            weak.upgrade()
                .is_some_and(|d| d.live.load(std::sync::atomic::Ordering::Acquire))
        });
        state.subscribers.push(route);
        if state.callback_id.is_none() {
            let callback = NetConnCallback {
                on_network_available: Some(on_handle),
                on_net_capabilities_change: Some(on_caps),
                on_net_lost: Some(on_handle),
                on_net_unavailable: Some(on_unavailable),
                ..Default::default()
            };
            match callback.register_default_net_conn_callback() {
                Ok(id) => {
                    state.callback_id = Some(id);
                }
                Err(error) => {
                    log::warn!("OHOS default network callback registration failed: {error}");
                }
            }
        }
        state.last = Some(current_network());
    }
}

#[cfg(target_env = "ohos")]
pub(crate) fn current_network() -> Network {
    ohos::current_network()
}

#[cfg(target_env = "ohos")]
pub(crate) fn subscribe(route: std::sync::Weak<crate::app::runtime::Dispatcher>) {
    ohos::subscribe(route);
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
