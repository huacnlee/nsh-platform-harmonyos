//! HarmonyOS HMS speech recognition backend for `nsh-extra`'s `speech` module.
//!
//! `nsh-extra::speech::set_backend` is process-wide, while HarmonyOS hosts
//! sessions per window (`Services`). This module keeps a globally keyed session
//! table partitioned by `host_session_id` so closing one window only tears down
//! speech sessions belonging to that window, while late callbacks after `abort`
//! or teardown are dropped silently.

#[cfg(any(test, feature = "speech"))]
mod registry {
    use rustc_hash::FxHashMap;

    /// Normalized speech error kinds matching `nsh_extra::speech::SpeechError`.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum SpeechErrorKind {
        NotAllowed,
        NoSpeech,
        AudioCapture,
        Network,
        LanguageNotSupported,
        Unavailable,
        Failed,
    }

    impl SpeechErrorKind {
        pub(crate) fn from_wire(name: &str) -> Self {
            match name {
                "not-allowed" => Self::NotAllowed,
                "no-speech" => Self::NoSpeech,
                "audio-capture" => Self::AudioCapture,
                "network" => Self::Network,
                "language-not-supported" => Self::LanguageNotSupported,
                "unavailable" => Self::Unavailable,
                _ => Self::Failed,
            }
        }
    }

    /// Wire event sent from `NativeSpeech.ets` (`nshSpeechEvent`).
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(crate) enum SpeechWireEvent {
        Start,
        Result { text: String, is_final: bool },
        End { error: Option<SpeechErrorKind> },
    }

    impl SpeechWireEvent {
        pub(crate) fn from_parts(
            phase: Option<&str>,
            text: Option<String>,
            is_final: Option<bool>,
            error: Option<&str>,
        ) -> Option<Self> {
            match phase {
                Some("start") => Some(Self::Start),
                Some("result") => Some(Self::Result {
                    text: text.unwrap_or_default(),
                    is_final: is_final.unwrap_or(false),
                }),
                Some("end") => Some(Self::End {
                    error: error
                        .filter(|s| !s.is_empty())
                        .map(SpeechErrorKind::from_wire),
                }),
                _ => None,
            }
        }
    }

    /// Platform-independent sink abstraction for host testing.
    pub(crate) trait SpeechSinkAdapter {
        fn on_start(&self);
        fn on_result(&self, text: String, is_final: bool);
        fn on_end(&self, error: Option<SpeechErrorKind>);
    }

    struct SessionEntry<S> {
        host_session_id: u32,
        sink: S,
    }

    /// Pure multi-session speech registry shared by OHOS runtime and host tests.
    pub(crate) struct SpeechRegistry<S> {
        next_key: u32,
        sessions: FxHashMap<u32, SessionEntry<S>>,
    }

    impl<S> Default for SpeechRegistry<S> {
        fn default() -> Self {
            Self {
                next_key: 1,
                sessions: FxHashMap::default(),
            }
        }
    }

    impl<S: SpeechSinkAdapter> SpeechRegistry<S> {
        pub(crate) fn insert(&mut self, host_session_id: u32, sink: S) -> u32 {
            let mut key = self.next_key;
            if key == 0 {
                key = 1;
            }
            while self.sessions.contains_key(&key) {
                key = key.wrapping_add(1).max(1);
            }
            self.next_key = key.wrapping_add(1).max(1);
            self.sessions.insert(
                key,
                SessionEntry {
                    host_session_id,
                    sink,
                },
            );
            key
        }

        pub(crate) fn deliver(&mut self, key: u32, event: SpeechWireEvent) -> bool {
            match event {
                SpeechWireEvent::Start => {
                    let Some(entry) = self.sessions.get(&key) else {
                        return false;
                    };
                    entry.sink.on_start();
                    true
                }
                SpeechWireEvent::Result { text, is_final } => {
                    let Some(entry) = self.sessions.get(&key) else {
                        return false;
                    };
                    entry.sink.on_result(text, is_final);
                    true
                }
                SpeechWireEvent::End { error } => {
                    let Some(entry) = self.sessions.remove(&key) else {
                        return false;
                    };
                    entry.sink.on_end(error);
                    true
                }
            }
        }

        /// Removes `key` immediately so any subsequent late callbacks from ArkTS
        /// are ignored. Returns `true` if the session was still active.
        pub(crate) fn abort(&mut self, key: u32) -> bool {
            self.sessions.remove(&key).is_some()
        }

        /// Ends only sessions owned by `host_session_id`, leaving sessions on other
        /// windows untouched.
        pub(crate) fn close_host_session(&mut self, host_session_id: u32) -> usize {
            let keys: Vec<u32> = self
                .sessions
                .iter()
                .filter_map(|(&key, entry)| {
                    (entry.host_session_id == host_session_id).then_some(key)
                })
                .collect();
            let count = keys.len();
            for key in keys {
                if let Some(entry) = self.sessions.remove(&key) {
                    entry.sink.on_end(Some(SpeechErrorKind::AudioCapture));
                }
            }
            count
        }

        #[cfg(test)]
        pub(crate) fn active_count(&self) -> usize {
            self.sessions.len()
        }
    }
}

#[cfg(any(test, feature = "speech"))]
pub(crate) use registry::{SpeechErrorKind, SpeechRegistry, SpeechSinkAdapter, SpeechWireEvent};

#[cfg(target_env = "ohos")]
mod ohos {
    #[cfg(feature = "speech")]
    use std::sync::atomic::{AtomicBool, Ordering};

    #[cfg(feature = "speech")]
    static SPEECH_AVAILABLE: AtomicBool = AtomicBool::new(false);

    pub(super) fn on_session_created(speech_available: bool) {
        #[cfg(feature = "speech")]
        {
            if speech_available {
                SPEECH_AVAILABLE.store(true, Ordering::Release);
            }
            extra::register_once();
        }
        #[cfg(not(feature = "speech"))]
        let _ = speech_available;
    }

    pub(super) fn on_session_closed(host_session_id: u32) {
        #[cfg(feature = "speech")]
        extra::on_session_closed(host_session_id);
        #[cfg(not(feature = "speech"))]
        let _ = host_session_id;
    }

    pub(super) fn handle_event(key: u32, event: crate::services::system::SystemEvent) {
        #[cfg(feature = "speech")]
        extra::handle_event(key, event);
        #[cfg(not(feature = "speech"))]
        let _ = (key, event);
    }

    #[cfg(feature = "speech")]
    mod extra {
        use std::sync::{Arc, LazyLock, Mutex, Once, Weak, atomic::Ordering};

        use nsh_extra::speech::{
            SpeechBackend, SpeechControl, SpeechError, SpeechRequest, SpeechSink, set_backend,
        };

        use super::SPEECH_AVAILABLE;
        use crate::{
            app::session::{self, HostCommand, Services},
            services::{
                speech::{SpeechErrorKind, SpeechRegistry, SpeechSinkAdapter, SpeechWireEvent},
                system::SystemEvent,
            },
        };

        static REGISTER: Once = Once::new();
        static REGISTRY: LazyLock<Mutex<SpeechRegistry<SpeechSink>>> =
            LazyLock::new(Mutex::default);

        fn map_error(kind: SpeechErrorKind) -> SpeechError {
            match kind {
                SpeechErrorKind::NotAllowed => SpeechError::NotAllowed,
                SpeechErrorKind::NoSpeech => SpeechError::NoSpeech,
                SpeechErrorKind::AudioCapture => SpeechError::AudioCapture,
                SpeechErrorKind::Network => SpeechError::Network,
                SpeechErrorKind::LanguageNotSupported => SpeechError::LanguageNotSupported,
                SpeechErrorKind::Unavailable => SpeechError::Unavailable,
                SpeechErrorKind::Failed => SpeechError::Failed,
            }
        }

        impl SpeechSinkAdapter for SpeechSink {
            fn on_start(&self) {
                self.start();
            }

            fn on_result(&self, text: String, is_final: bool) {
                self.result(text, is_final);
            }

            fn on_end(&self, error: Option<SpeechErrorKind>) {
                self.end(error.map(map_error));
            }
        }

        struct NoopControl;
        impl SpeechControl for NoopControl {
            fn stop(&mut self) {}
            fn abort(&mut self) {}
        }

        struct HarmonySpeechControl {
            host: Weak<Services>,
            key: u32,
            stopped: bool,
            aborted: bool,
        }

        impl SpeechControl for HarmonySpeechControl {
            fn stop(&mut self) {
                if self.stopped || self.aborted {
                    return;
                }
                self.stopped = true;
                if let Some(host) = self.host.upgrade() {
                    let _ = host.send_host_command(HostCommand {
                        kind: "speech-stop".into(),
                        open_id: Some(self.key),
                        ..Default::default()
                    });
                }
            }

            fn abort(&mut self) {
                if self.aborted {
                    return;
                }
                self.aborted = true;
                let was_active = REGISTRY
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .abort(self.key);
                if was_active && let Some(host) = self.host.upgrade() {
                    let _ = host.send_host_command(HostCommand {
                        kind: "speech-abort".into(),
                        open_id: Some(self.key),
                        ..Default::default()
                    });
                }
            }
        }

        struct HarmonySpeechBackend;

        impl SpeechBackend for HarmonySpeechBackend {
            fn available(&self) -> bool {
                SPEECH_AVAILABLE.load(Ordering::Acquire)
                    && session::any_active_session(false).is_ok()
            }

            fn start(&self, request: SpeechRequest, events: SpeechSink) -> Box<dyn SpeechControl> {
                if !SPEECH_AVAILABLE.load(Ordering::Acquire) {
                    events.end(Some(SpeechError::Unavailable));
                    return Box::new(NoopControl);
                }
                let host = match session::any_active_session(true) {
                    Ok(host) => host,
                    Err(_) => {
                        events.end(Some(SpeechError::Unavailable));
                        return Box::new(NoopControl);
                    }
                };
                let key = REGISTRY
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(host.session_id(), events);
                let send_result = host.send_host_command(HostCommand {
                    kind: "speech-start".into(),
                    open_id: Some(key),
                    text: request.lang,
                    enabled: Some(request.interim),
                    ..Default::default()
                });
                if send_result.is_err() {
                    REGISTRY.lock().unwrap_or_else(|e| e.into_inner()).deliver(
                        key,
                        SpeechWireEvent::End {
                            error: Some(SpeechErrorKind::Unavailable),
                        },
                    );
                    return Box::new(NoopControl);
                }
                Box::new(HarmonySpeechControl {
                    host: Arc::downgrade(&host),
                    key,
                    stopped: false,
                    aborted: false,
                })
            }
        }

        pub(super) fn register_once() {
            REGISTER.call_once(|| {
                set_backend(Some(Arc::new(HarmonySpeechBackend)));
            });
        }

        pub(super) fn on_session_closed(host_session_id: u32) {
            REGISTRY
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .close_host_session(host_session_id);
        }

        pub(super) fn handle_event(key: u32, event: SystemEvent) {
            let Some(wire) = SpeechWireEvent::from_parts(
                event.phase.as_deref(),
                event.text,
                event.is_final,
                event.error.as_deref(),
            ) else {
                return;
            };
            REGISTRY
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .deliver(key, wire);
        }
    }
}

#[cfg(target_env = "ohos")]
pub(crate) fn on_session_created(speech_available: bool) {
    ohos::on_session_created(speech_available);
}

#[cfg(target_env = "ohos")]
pub(crate) fn on_session_closed(host_session_id: u32) {
    ohos::on_session_closed(host_session_id);
}

#[cfg(target_env = "ohos")]
pub(crate) fn handle_event(key: u32, event: crate::services::system::SystemEvent) {
    ohos::handle_event(key, event);
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;

    #[derive(Clone, Default)]
    struct MockSink(Arc<Mutex<Vec<SpeechWireEvent>>>);

    impl MockSink {
        fn events(&self) -> Vec<SpeechWireEvent> {
            self.0.lock().unwrap().clone()
        }
    }

    impl SpeechSinkAdapter for MockSink {
        fn on_start(&self) {
            self.0.lock().unwrap().push(SpeechWireEvent::Start);
        }

        fn on_result(&self, text: String, is_final: bool) {
            self.0
                .lock()
                .unwrap()
                .push(SpeechWireEvent::Result { text, is_final });
        }

        fn on_end(&self, error: Option<SpeechErrorKind>) {
            self.0.lock().unwrap().push(SpeechWireEvent::End { error });
        }
    }

    #[test]
    fn delivers_start_interim_final_end_in_order_and_drops_late_events() {
        let mut reg = SpeechRegistry::default();
        let sink = MockSink::default();
        let key = reg.insert(1, sink.clone());

        assert!(reg.deliver(key, SpeechWireEvent::Start));
        assert!(reg.deliver(
            key,
            SpeechWireEvent::Result {
                text: "hel".into(),
                is_final: false,
            }
        ));
        assert!(reg.deliver(
            key,
            SpeechWireEvent::Result {
                text: "hello".into(),
                is_final: true,
            }
        ));
        assert!(reg.deliver(key, SpeechWireEvent::End { error: None }));
        // Late callback after normal end is ignored.
        assert!(!reg.deliver(
            key,
            SpeechWireEvent::Result {
                text: "late".into(),
                is_final: true,
            }
        ));
        assert_eq!(reg.active_count(), 0);
        assert_eq!(
            sink.events(),
            vec![
                SpeechWireEvent::Start,
                SpeechWireEvent::Result {
                    text: "hel".into(),
                    is_final: false,
                },
                SpeechWireEvent::Result {
                    text: "hello".into(),
                    is_final: true,
                },
                SpeechWireEvent::End { error: None },
            ]
        );
    }

    #[test]
    fn abort_is_idempotent_and_isolates_host_sessions() {
        let mut reg = SpeechRegistry::default();
        let sink_a = MockSink::default();
        let sink_b = MockSink::default();
        let key_a = reg.insert(10, sink_a.clone());
        let key_b = reg.insert(20, sink_b.clone());

        // Closing host session 10 ends only session A, not session B.
        assert_eq!(reg.close_host_session(10), 1);
        assert_eq!(
            sink_a.events(),
            vec![SpeechWireEvent::End {
                error: Some(SpeechErrorKind::AudioCapture)
            }]
        );
        assert!(sink_b.events().is_empty());
        assert!(reg.deliver(key_b, SpeechWireEvent::Start));
        assert!(!reg.deliver(key_a, SpeechWireEvent::Start));

        // Repeated abort on B is idempotent and suppresses late events.
        assert!(reg.abort(key_b));
        assert!(!reg.abort(key_b));
        assert!(!reg.deliver(key_b, SpeechWireEvent::End { error: None }));
        assert_eq!(reg.active_count(), 0);
    }

    #[test]
    fn parses_wire_phases_and_error_codes() {
        assert_eq!(
            SpeechWireEvent::from_parts(Some("end"), None, None, Some("not-allowed")),
            Some(SpeechWireEvent::End {
                error: Some(SpeechErrorKind::NotAllowed)
            })
        );
        assert_eq!(
            SpeechWireEvent::from_parts(Some("end"), None, None, Some("language-not-supported")),
            Some(SpeechWireEvent::End {
                error: Some(SpeechErrorKind::LanguageNotSupported)
            })
        );
        assert_eq!(
            SpeechWireEvent::from_parts(Some("end"), None, None, Some("unknown-code")),
            Some(SpeechWireEvent::End {
                error: Some(SpeechErrorKind::Failed)
            })
        );
        assert_eq!(
            SpeechWireEvent::from_parts(Some("end"), None, None, None),
            Some(SpeechWireEvent::End { error: None })
        );
        assert_eq!(
            SpeechWireEvent::from_parts(Some("result"), Some("hi".into()), Some(true), None),
            Some(SpeechWireEvent::Result {
                text: "hi".into(),
                is_final: true
            })
        );
    }
}
