//! ArkWeb's part of `WebView`'s `evaluate`: each request's script runs wrapped
//! by `nsh_core::platform::web_evaluation`, a returned Promise is polled until
//! it settles, and the reply goes back to the core as `EventKind::WebEvaluation`.
//!
//! Mirroring `nsh_platform_apple::web_evaluation::Evaluations`, callbacks and
//! delayed polls hold only weak references (`Weak<Evaluations>` / `Weak<dyn ScriptHost>`),
//! while membership in `pending` plus a monotonic `serial` make late callbacks
//! harmless after cancellation, navigation, renderer loss, or disposal.

use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    rc::{Rc, Weak},
    time::Duration,
};

use nsh_core::{
    EventKind, EventPayload, NodeId, PayloadValue, element::codes,
    platform::web_evaluation as script,
};

const POLL_INTERVAL: Duration = Duration::from_millis(25);
const MAX_ENVELOPE_BYTES: usize = script::MAX_RESULT_BYTES + 1024;
const MAX_RAW_BYTES: usize = MAX_ENVELOPE_BYTES * 6 + 2;

pub(crate) type Reply = Box<dyn Fn(NodeId, EventKind, EventPayload)>;
pub(crate) type Schedule = Box<dyn Fn(Duration, Box<dyn FnOnce()>)>;
pub(crate) type JsDone = Box<dyn FnOnce(Option<&[u8]>)>;

pub(crate) trait ScriptHost {
    fn evaluate(&self, script: &str, done: JsDone);

    fn execute(&self, script: &str) {
        self.evaluate(script, Box::new(|_| {}));
    }
}

struct Pending {
    serial: u64,
    poll: String,
    cleanup: String,
}

pub(crate) struct Evaluations {
    id: NodeId,
    send: Reply,
    after: Schedule,
    web: RefCell<Option<Weak<dyn ScriptHost>>>,
    pending: RefCell<HashMap<u32, Pending>>,
    serial: Cell<u64>,
    process_gone: Cell<bool>,
}

impl Evaluations {
    pub(crate) fn new(id: NodeId, send: Reply, after: Schedule) -> Rc<Self> {
        Rc::new(Self {
            id,
            send,
            after,
            web: RefCell::new(None),
            pending: RefCell::new(HashMap::new()),
            serial: Cell::new(0),
            process_gone: Cell::new(false),
        })
    }

    pub(crate) fn set_web(&self, web: Option<&Rc<dyn ScriptHost>>) {
        *self.web.borrow_mut() = web.map(Rc::downgrade);
    }

    pub(crate) fn start(self: &Rc<Self>, request: u32, script_source: &str, scripts: bool) {
        if self.web().is_none() {
            self.error(request, codes::WEBVIEW_ABORTED, "the web view went away");
            return;
        }
        if !scripts {
            self.error(request, codes::WEBVIEW_SCRIPT, "the page runs no scripts");
            return;
        }
        self.cancel(request);
        let serial = self.serial.get().wrapping_add(1);
        self.serial.set(serial);
        let key = format!("__nsh_evaluation_{}_{}_{}", self.id, request, serial);
        self.pending.borrow_mut().insert(
            request,
            Pending {
                serial,
                poll: script::poll_script(&key),
                cleanup: script::cleanup_script(&key),
            },
        );
        self.run(request, serial, &script::start_script(script_source, &key));
    }

    pub(crate) fn cancel(&self, request: u32) {
        if let Some(entry) = self.pending.borrow_mut().remove(&request) {
            self.cleanup(entry);
        }
    }

    pub(crate) fn navigated(&self) {
        self.process_gone.set(false);
        self.cancel_all(codes::WEBVIEW_ABORTED, "the document navigated");
    }

    pub(crate) fn process_terminated(&self) {
        self.process_gone.set(true);
        self.cancel_all(codes::WEBVIEW_ABORTED, "the web content process ended");
    }

    pub(crate) fn scripts_disabled(&self) {
        self.cancel_all(codes::WEBVIEW_SCRIPT, "the page runs no scripts");
    }

    pub(crate) fn dispose(&self) {
        self.cancel_all(codes::WEBVIEW_ABORTED, "the web view went away");
        self.set_web(None);
    }

    fn reply(&self, request: u32, data: String) {
        (self.send)(
            self.id,
            EventKind::WebEvaluation,
            evaluation_payload(request, data),
        );
    }

    fn error(&self, request: u32, code: &str, message: &str) {
        self.reply(request, error_json(code, message));
    }

    fn web(&self) -> Option<Rc<dyn ScriptHost>> {
        if self.process_gone.get() {
            return None;
        }
        self.web.borrow().as_ref().and_then(Weak::upgrade)
    }

    fn active(&self, request: u32, serial: u64) -> bool {
        self.pending
            .borrow()
            .get(&request)
            .is_some_and(|entry| entry.serial == serial)
    }

    fn cleanup(&self, entry: Pending) {
        if let Some(web) = self.web() {
            web.execute(&entry.cleanup);
        }
    }

    fn cancel_all(&self, code: &str, message: &str) {
        let mut pending: Vec<_> = std::mem::take(&mut *self.pending.borrow_mut())
            .into_iter()
            .collect();
        pending.sort_unstable_by_key(|(request, _)| *request);
        for (request, entry) in pending {
            self.cleanup(entry);
            self.error(request, code, message);
        }
    }

    fn finish(&self, request: u32, serial: u64, data: String) {
        if !self.active(request, serial) {
            return;
        }
        self.pending.borrow_mut().remove(&request);
        self.reply(request, data);
    }

    fn fail(&self, request: u32, serial: u64, code: &str, message: &str) {
        if !self.active(request, serial) {
            return;
        }
        self.cancel(request);
        self.error(request, code, message);
    }

    fn run(self: &Rc<Self>, request: u32, serial: u64, source: &str) {
        if !self.active(request, serial) {
            return;
        }
        let Some(web) = self.web() else {
            self.fail(
                request,
                serial,
                codes::WEBVIEW_ABORTED,
                "the web view went away",
            );
            return;
        };
        let state = Rc::downgrade(self);
        web.evaluate(
            source,
            Box::new(move |raw| {
                let Some(state) = state.upgrade() else {
                    return;
                };
                if !state.active(request, serial) {
                    return;
                }
                let Some(bytes) = raw else {
                    state.fail(
                        request,
                        serial,
                        codes::WEBVIEW_SCRIPT,
                        "the page could not run the script",
                    );
                    return;
                };
                if bytes.len() > MAX_RAW_BYTES {
                    state.fail(
                        request,
                        serial,
                        codes::QUOTA_EXCEEDED,
                        "the script's result is too large",
                    );
                    return;
                }
                let Some(text) = std::str::from_utf8(bytes)
                    .ok()
                    .and_then(decode_arkweb_envelope)
                else {
                    state.fail(
                        request,
                        serial,
                        codes::WEBVIEW_SCRIPT,
                        "the page gave no answer",
                    );
                    return;
                };
                if text.len() > MAX_ENVELOPE_BYTES {
                    state.fail(
                        request,
                        serial,
                        codes::QUOTA_EXCEEDED,
                        "the script's result is too large",
                    );
                    return;
                }
                let status = serde_json::from_str::<serde_json::Value>(&text)
                    .ok()
                    .and_then(|value| {
                        value
                            .get("status")
                            .and_then(|status| status.as_str())
                            .map(str::to_owned)
                    });
                match status.as_deref() {
                    Some("pending") => {
                        let weak = Rc::downgrade(&state);
                        (state.after)(
                            POLL_INTERVAL,
                            Box::new(move || {
                                let Some(state) = weak.upgrade() else {
                                    return;
                                };
                                let poll = state
                                    .pending
                                    .borrow()
                                    .get(&request)
                                    .filter(|entry| entry.serial == serial)
                                    .map(|entry| entry.poll.clone());
                                if let Some(poll) = poll {
                                    state.run(request, serial, &poll);
                                }
                            }),
                        );
                    }
                    Some("ok" | "error") => state.finish(request, serial, text),
                    _ => state.fail(
                        request,
                        serial,
                        codes::WEBVIEW_SCRIPT,
                        "the page gave no answer",
                    ),
                }
            }),
        );
    }
}

pub(crate) fn abort_payload(request: u32) -> EventPayload {
    evaluation_payload(
        request,
        error_json(codes::WEBVIEW_ABORTED, "the web view went away"),
    )
}

fn evaluation_payload(request: u32, data: String) -> EventPayload {
    EventPayload::Map(vec![
        ("id", PayloadValue::Number(f64::from(request))),
        ("data", PayloadValue::Str(data)),
    ])
}

fn error_json(code: &str, message: &str) -> String {
    serde_json::json!({
        "status": "error",
        "code": code,
        "message": message,
    })
    .to_string()
}

/// ArkWeb JSON-quotes string results from `runJavaScript`; unwrap that one outer
/// layer while leaving already-unquoted strings intact.
fn decode_arkweb_envelope(raw: &str) -> Option<String> {
    if raw.starts_with('"') {
        serde_json::from_str::<String>(raw).ok()
    } else {
        Some(raw.to_owned())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use super::*;

    type JsCall = (String, JsDone);
    type ScheduledTimer = (Duration, Box<dyn FnOnce()>);

    #[derive(Default)]
    struct MockWeb {
        calls: RefCell<VecDeque<JsCall>>,
    }

    impl MockWeb {
        fn pop_script(&self) -> JsCall {
            self.calls
                .borrow_mut()
                .pop_front()
                .expect("expected queued run_javascript call")
        }
    }

    impl ScriptHost for MockWeb {
        fn evaluate(&self, script: &str, done: JsDone) {
            self.calls.borrow_mut().push_back((script.to_owned(), done));
        }
    }

    struct Harness {
        evals: Rc<Evaluations>,
        web: Rc<MockWeb>,
        replies: Rc<RefCell<Vec<(u32, serde_json::Value)>>>,
        timers: Rc<RefCell<VecDeque<ScheduledTimer>>>,
    }

    impl Harness {
        fn new() -> Self {
            let replies = Rc::new(RefCell::new(Vec::new()));
            let timers = Rc::new(RefCell::new(VecDeque::new()));
            let replies_out = replies.clone();
            let timers_out = timers.clone();
            let evals = Evaluations::new(
                4,
                Box::new(move |id, kind, payload| {
                    assert_eq!(id, 4);
                    assert_eq!(kind, EventKind::WebEvaluation);
                    let EventPayload::Map(entries) = payload else {
                        panic!("expected map payload");
                    };
                    let mut req = None;
                    let mut data = None;
                    for (k, v) in entries {
                        match (k, v) {
                            ("id", PayloadValue::Number(n)) => req = Some(n as u32),
                            ("data", PayloadValue::Str(s)) => {
                                data = Some(serde_json::from_str(&s).unwrap());
                            }
                            _ => {}
                        }
                    }
                    replies_out.borrow_mut().push((req.unwrap(), data.unwrap()));
                }),
                Box::new(move |delay, work| {
                    timers_out.borrow_mut().push_back((delay, work));
                }),
            );
            let web = Rc::new(MockWeb::default());
            let host: Rc<dyn ScriptHost> = web.clone();
            evals.set_web(Some(&host));
            Self {
                evals,
                web,
                replies,
                timers,
            }
        }

        fn take_replies(&self) -> Vec<(u32, serde_json::Value)> {
            std::mem::take(&mut *self.replies.borrow_mut())
        }
    }

    #[test]
    fn sync_and_promise_poll_lifecycle_and_arkweb_unquoting() {
        let h = Harness::new();
        h.evals.start(1, "Promise.resolve('鸿蒙')", true);
        let (start_js, done1) = h.web.pop_script();
        assert!(start_js.contains("__nsh_evaluation_4_1_1"));

        // Pending envelope (ArkWeb JSON-quoted string) schedules a 25ms poll.
        done1(Some(br#""{\"status\":\"pending\"}""#));
        assert!(h.take_replies().is_empty());
        let (delay, poll_timer) = h.timers.borrow_mut().pop_front().unwrap();
        assert_eq!(delay, Duration::from_millis(25));

        poll_timer();
        let (poll_js, done2) = h.web.pop_script();
        assert!(poll_js.contains("__nsh_evaluation_4_1_1"));

        let envelope = r#"{"status":"ok","value":"\"鸿蒙\""}"#;
        let quoted = serde_json::to_string(envelope).unwrap();
        done2(Some(quoted.as_bytes()));
        assert!(h.web.calls.borrow().is_empty());

        let replies = h.take_replies();
        assert_eq!(replies.len(), 1);
        assert_eq!(replies[0].0, 1);
        assert_eq!(replies[0].1["status"], "ok");
        assert_eq!(replies[0].1["value"], "\"鸿蒙\"");
    }

    #[test]
    fn cancel_reused_id_and_stale_callbacks_are_isolated() {
        let h = Harness::new();
        h.evals.start(9, "new Promise(() => {})", true);
        let (_, stale_done) = h.web.pop_script();

        // Reusing request id 9 cancels the first serial (running cleanup) and starts serial 2.
        h.evals.start(9, "2 + 2", true);
        let (cleanup_js, _) = h.web.pop_script();
        assert!(cleanup_js.starts_with("delete globalThis["));
        let (_, second_done) = h.web.pop_script();

        // Late completion from serial 1 must be ignored.
        stale_done(Some(br#""{\"status\":\"ok\",\"value\":1}""#));
        assert!(h.take_replies().is_empty());

        // Cancelling request 9 forgets serial 2 without replying.
        h.evals.cancel(9);
        let (cleanup2, _) = h.web.pop_script();
        assert!(cleanup2.starts_with("delete globalThis["));
        second_done(Some(br#""{\"status\":\"ok\",\"value\":4}""#));
        assert!(h.take_replies().is_empty());
    }

    #[test]
    fn navigation_script_disable_process_loss_and_dispose_transitions() {
        let h = Harness::new();

        // Disabled scripts reject immediately and abort active requests with WEBVIEW_SCRIPT.
        h.evals.start(1, "1", false);
        let replies = h.take_replies();
        assert_eq!(replies[0].1["code"], codes::WEBVIEW_SCRIPT);

        h.evals.start(2, "new Promise(() => {})", true);
        let _ = h.web.pop_script();
        h.evals.scripts_disabled();
        let (cleanup, _) = h.web.pop_script();
        assert!(cleanup.starts_with("delete globalThis["));
        assert_eq!(h.take_replies()[0].1["code"], codes::WEBVIEW_SCRIPT);

        // Renderer termination aborts without page cleanup and blocks until navigation.
        h.evals.start(3, "new Promise(() => {})", true);
        let _ = h.web.pop_script();
        h.evals.process_terminated();
        assert!(h.web.calls.borrow().is_empty());
        assert_eq!(h.take_replies()[0].1["code"], codes::WEBVIEW_ABORTED);

        h.evals.start(4, "1", true);
        assert_eq!(h.take_replies()[0].1["code"], codes::WEBVIEW_ABORTED);

        // Navigation clears `process_gone` and aborts active requests on future navigations.
        h.evals.navigated();
        h.evals.start(5, "new Promise(() => {})", true);
        let _ = h.web.pop_script();
        h.evals.navigated();
        let _ = h.web.pop_script();
        assert_eq!(h.take_replies()[0].1["code"], codes::WEBVIEW_ABORTED);

        // Quota and invalid answers fail cleanly and run cleanup.
        h.evals.start(6, "1", true);
        let (_, done6) = h.web.pop_script();
        done6(Some(b"null"));
        let _ = h.web.pop_script();
        assert_eq!(h.take_replies()[0].1["code"], codes::WEBVIEW_SCRIPT);

        h.evals.start(7, "'x'", true);
        let (_, done7) = h.web.pop_script();
        let huge = format!(
            r#"{{"status":"ok","value":"{}"}}"#,
            "x".repeat(MAX_ENVELOPE_BYTES)
        );
        done7(Some(huge.as_bytes()));
        let _ = h.web.pop_script();
        assert_eq!(h.take_replies()[0].1["code"], codes::QUOTA_EXCEEDED);

        // Disposal aborts pending work and detaches the host.
        h.evals.start(8, "new Promise(() => {})", true);
        let _ = h.web.pop_script();
        h.evals.dispose();
        let _ = h.web.pop_script();
        assert_eq!(h.take_replies()[0].1["code"], codes::WEBVIEW_ABORTED);
        h.evals.start(9, "1", true);
        assert_eq!(h.take_replies()[0].1["code"], codes::WEBVIEW_ABORTED);

        let EventPayload::Map(fields) = abort_payload(99) else {
            panic!("expected map");
        };
        assert_eq!(fields.len(), 2);
    }
}
