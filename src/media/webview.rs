//! Embedded ArkWeb views (`<web-view>`), `nsh-asset://` custom protocol, and `window.nsh.postMessage`.

use std::{
    cell::{Cell, RefCell},
    rc::{Rc, Weak},
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
};

use nsh_core::{Command, EventKind, EventPayload, PayloadValue, WebProps, WebSource};
use nsh_engine::{Access, EngineControl};
use ohos_arkui_binding::{
    component::{attribute::ArkUICommonAttribute, built_in_component::Custom},
    types::advanced::{LengthMetricUnit, NodeDirtyFlag},
};
use ohos_web_binding::{
    ArkWebResponse, CustomProtocol, CustomProtocolHandler, CustomProtocolOption, ResourceHandle,
    Web, WebProxy, WebProxyBuilder,
};
use rustc_hash::{FxHashMap, FxHashSet};

use super::{
    web_downloads,
    web_evaluation::{EvalGuard, Evaluations, JsDone, ScriptHost, abort_payload},
};
use crate::{
    app::{runtime::Dispatcher, session::Services},
    interop::{
        attachment::NativeAttachment,
        sdk::{WebCommand, WebEvent, WebHeader, WebSettings},
    },
    render::{
        geometry::Size,
        node::{Error, NativeNode, Node, Result},
    },
    services::{assets::StreamAssets, executor::Executor},
};

const SCHEME: &str = "nsh-asset";
const BRIDGE: &str = "window.nsh = window.nsh || { postMessage: function (data) { window.__nshNative.postMessage(String(data)); } };";

static SCHEME_REGISTERED: OnceLock<()> = OnceLock::new();
static ARKWEB_INITIALIZED: AtomicBool = AtomicBool::new(false);

pub(crate) fn register_custom_scheme() {
    SCHEME_REGISTERED.get_or_init(|| {
        CustomProtocol::add_protocol_with_option(
            SCHEME,
            CustomProtocolOption::Standard
                | CustomProtocolOption::Secure
                | CustomProtocolOption::CorsEnabled
                | CustomProtocolOption::FetchEnabled,
        );
        CustomProtocol::register();
    });
}

fn register_url_schemes(schemes: &[nsh_core::web_resource::WebUrlScheme]) {
    let initialized = ARKWEB_INITIALIZED.load(Ordering::Acquire);
    let mut added = false;
    for scheme in schemes {
        let name = scheme.scheme();
        if !CustomProtocol::is_protocol_registered(name) {
            if initialized {
                log::warn!(
                    "OHOS WebView custom scheme `{name}` cannot be registered after ArkWeb initialization"
                );
                continue;
            }
            CustomProtocol::add_protocol_with_option(
                name,
                CustomProtocolOption::Standard
                    | CustomProtocolOption::Secure
                    | CustomProtocolOption::CorsEnabled
                    | CustomProtocolOption::FetchEnabled,
            );
            added = true;
        }
    }
    if added {
        CustomProtocol::register();
    }
}

pub(crate) fn on_eval_result(target_id: u32, key: &str, envelope: &str) {
    let evals = ROUTES.with(|routes| {
        routes
            .borrow()
            .get(&target_id)
            .map(|route| route.evaluations.clone())
    });
    if let Some(evals) = evals {
        evals.push_result(key, envelope);
    }
}

struct AssetReply(Option<ResourceHandle>);

// SAFETY: ArkWeb owns the underlying `ArkWeb_ResourceHandler` across threads until `finish()`
// is called once by the asset worker or `Drop`.
unsafe impl Send for AssetReply {}

impl AssetReply {
    fn respond(mut self, status: u16, mime_type: &str, body: &[u8]) {
        if let Some(handle) = self.0.take() {
            send_response(&handle, status, mime_type, body);
        }
    }
}

impl Drop for AssetReply {
    fn drop(&mut self) {
        if let Some(handle) = self.0.take() {
            send_response(&handle, 503, "text/plain; charset=utf-8", &[]);
        }
    }
}

fn send_response(handle: &ResourceHandle, status: u16, mime_type: &str, body: &[u8]) {
    let response = ArkWebResponse::new();
    response.set_header("content-type", mime_type, true);
    response.set_header("access-control-allow-origin", "*", true);
    response.set_status(i32::from(status));
    handle.receive_response(response);
    handle.receive_data(body);
    handle.finish();
}

type PendingJs = Box<dyn FnOnce(&Web)>;

const MAX_PENDING_JS: usize = 256;

struct PendingJob {
    guard: Option<EvalGuard>,
    run: PendingJs,
}

struct Route {
    node: Weak<Node>,
    dispatcher: std::sync::Weak<Dispatcher>,
    access: Arc<Access>,
    assets: Arc<dyn StreamAssets>,
    host: Arc<Services>,
    url_schemes: Vec<nsh_core::web_resource::WebUrlScheme>,
    defer_downloads: Cell<bool>,
    proxy: RefCell<Option<WebProxy>>,
    web: RefCell<Option<Web>>,
    controller_ready: Cell<bool>,
    pending_js: RefCell<std::collections::VecDeque<PendingJob>>,
    evaluations: Rc<Evaluations>,
}

/// Retains a web view's `(NodeId, EngineControl)` across view teardown while
/// ArkWeb still has active downloads whose stop confirmation `nsh-core` needs
/// before cleaning its private staging directories.
struct DownloadSink {
    node_id: nsh_core::NodeId,
    control: EngineControl,
    active_ids: FxHashSet<String>,
    closed: bool,
}

thread_local! {
    static ROUTES: RefCell<FxHashMap<u32, Rc<Route>>> = RefCell::default();
    static DOWNLOAD_SINKS: RefCell<FxHashMap<u32, DownloadSink>> = RefCell::default();
}

fn eval_on_web(dispatcher: &std::sync::Weak<Dispatcher>, web: &Web, script: &str, done: JsDone) {
    let reply = Dispatcher::reply(dispatcher, move |raw: Option<Vec<u8>>| {
        done(raw.as_deref());
    });
    let cb_reply = reply.clone();
    if web
        .run_javascript(script, move |bytes| {
            cb_reply.send(bytes.map(<[u8]>::to_vec));
        })
        .is_err()
    {
        reply.send(None);
    }
}

impl ScriptHost for Route {
    fn evaluate(&self, script: &str, guard: EvalGuard, done: JsDone) {
        if !guard.is_alive() {
            return;
        }
        if self.controller_ready.get()
            && let Some(web) = self.web.borrow().as_ref()
        {
            guard.mark_dispatched();
            eval_on_web(&self.dispatcher, web, script, done);
        } else {
            let script = script.to_owned();
            let dispatcher = self.dispatcher.clone();
            let cb_guard = guard.clone();
            self.enqueue_job(
                Some(guard),
                Box::new(move |web| {
                    if cb_guard.is_alive() {
                        cb_guard.mark_dispatched();
                        eval_on_web(&dispatcher, web, &script, done);
                    }
                }),
            );
        }
    }

    fn execute(&self, script: &str) {
        if self.controller_ready.get()
            && let Some(web) = self.web.borrow().as_ref()
        {
            let _ = web.run_javascript(script, |_| {});
        } else {
            let script = script.to_owned();
            self.enqueue_job(
                None,
                Box::new(move |web| {
                    let _ = web.run_javascript(&script, |_| {});
                }),
            );
        }
    }
}

impl Route {
    fn enqueue_job(&self, guard: Option<EvalGuard>, run: PendingJs) {
        let mut queue = self.pending_js.borrow_mut();
        queue.retain(|job| job.guard.as_ref().is_none_or(EvalGuard::is_alive));
        if queue.len() >= MAX_PENDING_JS {
            if let Some(idx) = queue.iter().position(|job| job.guard.is_none()) {
                queue.remove(idx);
            } else {
                queue.pop_front();
            }
        }
        queue.push_back(PendingJob { guard, run });
    }

    fn run_detached(&self, script: &str) {
        self.execute(script);
    }

    fn on_ready(&self) {
        self.controller_ready.set(true);
        let queued = std::mem::take(&mut *self.pending_js.borrow_mut());
        if let Some(web) = self.web.borrow().as_ref() {
            for job in queued {
                if job.guard.as_ref().is_none_or(EvalGuard::is_alive) {
                    (job.run)(web);
                }
            }
        }
    }
}

struct RouteRegistration(u32);
impl Drop for RouteRegistration {
    fn drop(&mut self) {
        ROUTES.with(|routes| {
            routes.borrow_mut().remove(&self.0);
        });
        DOWNLOAD_SINKS.with(|sinks| {
            let mut sinks = sinks.borrow_mut();
            if let Some(sink) = sinks.get_mut(&self.0) {
                sink.closed = true;
                if sink.active_ids.is_empty() {
                    sinks.remove(&self.0);
                }
            }
        });
    }
}

#[derive(PartialEq)]
struct DocumentSettings {
    javascript: bool,
    shared_cookies: bool,
    script: Option<Arc<str>>,
    url_schemes: Vec<nsh_core::web_resource::WebUrlScheme>,
}

pub(crate) struct WebView {
    node: Weak<Node>,
    native: NativeNode,
    target: RefCell<Option<NativeAttachment>>,
    route: RefCell<Option<RouteRegistration>>,
    route_ref: RefCell<Option<Rc<Route>>>,
    evaluations: Rc<Evaluations>,
    frame: Cell<Size>,
    props: RefCell<Option<WebProps>>,
    applied_source: RefCell<Option<WebSource>>,
    applied_document: RefCell<Option<DocumentSettings>>,
    pending_settings: RefCell<Option<WebSettings>>,
    pending_commands: RefCell<Vec<WebCommand>>,
    arkts_mounted: Cell<bool>,
    waiting_transition: Cell<bool>,
    load_generation: Cell<u32>,
    ready: Cell<bool>,
    closed: Cell<bool>,
}

impl WebView {
    pub(crate) fn new(node: Weak<Node>) -> Result<Self> {
        let owner = node.upgrade().ok_or(Error::InvalidNode(0))?;
        let native = Custom::new()?.into_node();
        native
            .borrow()
            .set_length_metric_unit(LengthMetricUnit::Vp)?;
        owner.native.borrow_mut().insert_child(native.clone(), 0)?;
        let evaluations = {
            let control = owner.control.clone();
            let dispatcher = owner.dispatcher.clone();
            Evaluations::new(
                owner.id,
                Box::new(move |id, kind, payload| control.dispatch_event(id, kind, payload)),
                Box::new(move |delay, work| {
                    let _ = Dispatcher::after(&dispatcher, delay, work);
                }),
            )
        };
        let mut target = None;
        let mut route = None;
        let mut route_ref = None;
        if owner.host.check().is_ok() {
            let attachment = NativeAttachment::new(&native)?;
            let id = attachment.id();
            let entry = Rc::new(Route {
                node: node.clone(),
                dispatcher: owner.dispatcher.clone(),
                access: owner.access.clone(),
                assets: owner.assets.clone(),
                host: owner.host.clone(),
                url_schemes: Vec::new(),
                defer_downloads: Cell::new(false),
                proxy: RefCell::new(None),
                web: RefCell::new(None),
                controller_ready: Cell::new(false),
                pending_js: RefCell::default(),
                evaluations: evaluations.clone(),
            });
            ROUTES.with(|routes| {
                routes.borrow_mut().insert(id, entry.clone());
            });
            route = Some(RouteRegistration(id));
            route_ref = Some(entry);
            target = Some(attachment);
        }
        Ok(Self {
            ready: Cell::new(owner.host.check().is_ok()),
            node,
            native,
            target: RefCell::new(target),
            route: RefCell::new(route),
            route_ref: RefCell::new(route_ref),
            evaluations,
            frame: Cell::new(Size::default()),
            props: RefCell::new(None),
            applied_source: RefCell::new(None),
            applied_document: RefCell::new(None),
            pending_settings: RefCell::new(None),
            pending_commands: RefCell::new(Vec::new()),
            arkts_mounted: Cell::new(false),
            waiting_transition: Cell::new(false),
            load_generation: Cell::new(0),
            closed: Cell::new(false),
        })
    }

    pub(crate) fn native(&self) -> &NativeNode {
        &self.native
    }

    pub(crate) fn frame(&self, size: Size) -> Result<()> {
        self.frame.set(size);
        let native = self.native.borrow();
        native.width(size.width)?;
        native.height(size.height)?;
        native.mark_dirty(NodeDirtyFlag::NeedMeasure)?;
        Ok(())
    }

    pub(crate) fn context(&self, ready: bool) -> Result<()> {
        if self.closed.get() || self.ready.replace(ready) == ready {
            return Ok(());
        }
        let Some(node) = self.node.upgrade() else {
            return Ok(());
        };
        if !ready {
            self.evaluations.dispose();
            self.route_ref.borrow_mut().take();
            self.route.borrow_mut().take();
            *self.applied_source.borrow_mut() = None;
            *self.applied_document.borrow_mut() = None;
            *self.pending_settings.borrow_mut() = None;
            self.fail_pending_requests(&node, "WebView context lost");
            let was_mounted = self.arkts_mounted.replace(false);
            self.waiting_transition.set(false);
            if let Some(target) = self.target.borrow_mut().take() {
                if was_mounted {
                    node.web_remove(target);
                } else {
                    target.invalidate()?;
                }
            }
            return Ok(());
        }
        self.replace_target(&node)?;
        self.sync(&node)
    }

    pub(crate) fn configure(&self, props: WebProps) -> Result<()> {
        if self.closed.get() {
            return Ok(());
        }
        let defer_downloads = props.defer_downloads;
        *self.props.borrow_mut() = Some(props);
        if let Some(route) = self.route_ref.borrow().as_ref() {
            route.defer_downloads.set(defer_downloads);
        }
        if let Some(node) = self.node.upgrade() {
            self.sync(&node)?;
        }
        Ok(())
    }

    pub(crate) fn command(&self, command: Command) -> Result<()> {
        let Some(node) = self.node.upgrade() else {
            return Ok(());
        };
        if let Command::Evaluate { request, script } = command {
            if self.closed.get() || !self.ready.get() || self.applied_document.borrow().is_none() {
                abort_evaluation(&node.control, node.id, request);
                return Ok(());
            }
            let js_enabled = self
                .props
                .borrow()
                .as_ref()
                .is_none_or(|p| p.javascript_enabled);
            self.evaluations.start(request, &script, js_enabled);
            return Ok(());
        }
        if let Command::CancelEvaluate { request } = command {
            self.evaluations.cancel(request);
            return Ok(());
        }
        if let Command::WebViewRequest(request) = command {
            let reply_error = |message: &str| {
                node.control.dispatch_event(
                    node.id,
                    EventKind::WebViewReply,
                    nsh_core::webview::reply(request.id, Err(message.to_owned())),
                );
            };
            if self.closed.get() || !self.ready.get() {
                reply_error("WebView is not ready");
                return Ok(());
            }
            let Some(target_id) = self.target.borrow().as_ref().map(NativeAttachment::id) else {
                reply_error("WebView target is unavailable");
                return Ok(());
            };
            let web_command = match request.operation {
                nsh_core::webview::WebViewOperation::StartDownload { id, staging } => {
                    let path = match web_downloads::validate_staging_path(&staging) {
                        Ok(path) => path,
                        Err(error) => {
                            reply_error(&error);
                            return Ok(());
                        }
                    };
                    WebCommand {
                        target_id,
                        kind: "download-start".into(),
                        request_id: Some(request.id.to_string()),
                        download_id: Some(id),
                        path: Some(path),
                        ..Default::default()
                    }
                }
                nsh_core::webview::WebViewOperation::CancelDownload { id } => WebCommand {
                    target_id,
                    kind: "download-cancel".into(),
                    request_id: Some(request.id.to_string()),
                    download_id: Some(id),
                    ..Default::default()
                },
                _ => {
                    reply_error("Unsupported WebView operation");
                    return Ok(());
                }
            };
            if self.arkts_mounted.get() {
                node.web_command(web_command);
            } else {
                self.pending_commands.borrow_mut().push(web_command);
            }
            return Ok(());
        }
        if self.closed.get() || !self.ready.get() {
            return Ok(());
        }
        let (Some(target_id), Some(route)) = (
            self.target.borrow().as_ref().map(NativeAttachment::id),
            self.route_ref.borrow().clone(),
        ) else {
            return Ok(());
        };
        let web_command = match command {
            Command::Reload => {
                self.evaluations.navigated();
                WebCommand {
                    target_id,
                    kind: "reload".into(),
                    script: None,
                    ..Default::default()
                }
            }
            Command::GoBack => {
                self.evaluations.navigated();
                WebCommand {
                    target_id,
                    kind: "go-back".into(),
                    script: None,
                    ..Default::default()
                }
            }
            Command::GoForward => {
                self.evaluations.navigated();
                WebCommand {
                    target_id,
                    kind: "go-forward".into(),
                    script: None,
                    ..Default::default()
                }
            }
            Command::StopLoading => WebCommand {
                target_id,
                kind: "stop-loading".into(),
                script: None,
                ..Default::default()
            },
            Command::Focus => WebCommand {
                target_id,
                kind: "focus".into(),
                script: None,
                ..Default::default()
            },
            Command::Blur => {
                route.run_detached("document.activeElement?.blur();");
                WebCommand {
                    target_id,
                    kind: "blur".into(),
                    script: None,
                    ..Default::default()
                }
            }
            Command::InjectJavaScript(script) => {
                route.run_detached(&script);
                return Ok(());
            }
            Command::PostMessage(data) => {
                let literal = serde_json::to_string(&data).expect("string literal");
                route.run_detached(&format!(
                    "window.dispatchEvent(new MessageEvent('message',{{data:{literal}}}));document.dispatchEvent(new MessageEvent('message',{{data:{literal}}}));"
                ));
                return Ok(());
            }
            Command::Load(load) => {
                let schemes = self
                    .props
                    .borrow()
                    .as_ref()
                    .map(|p| p.url_schemes.clone())
                    .unwrap_or_default();
                if !node.access.allows_webviews()
                    || (!load.src.starts_with("asset:/")
                        && !has_custom_scheme(&load.src, &schemes)
                        && !node.access.allows_webview(&load.src))
                {
                    node.control.dispatch_event(
                        node.id,
                        EventKind::Error,
                        EventPayload::Map(vec![
                            (
                                "message",
                                PayloadValue::Str("WebView source is not granted".into()),
                            ),
                            (
                                "code",
                                PayloadValue::Str(nsh_core::element::codes::ACCESS_DENIED.into()),
                            ),
                        ]),
                    );
                    return Ok(());
                }
                self.evaluations.navigated();
                WebCommand {
                    target_id,
                    kind: "load".into(),
                    url: Some(asset_url(&load.src)),
                    headers: Some(
                        load.headers
                            .into_iter()
                            .map(|(header_key, header_value)| WebHeader {
                                header_key,
                                header_value,
                            })
                            .collect(),
                    ),
                    ..Default::default()
                }
            }
            Command::SendInput(input) => {
                use nsh_core::{WebInput, WebKey};
                let mut command = WebCommand {
                    target_id,
                    ..Default::default()
                };
                match input {
                    WebInput::Click { x, y } => {
                        command.kind = "input-click".into();
                        command.x = Some(f64::from(x.0));
                        command.y = Some(f64::from(y.0));
                    }
                    WebInput::Text(text) => {
                        command.kind = "input-text".into();
                        command.script = Some(text);
                    }
                    WebInput::Key(key) => {
                        command.kind = "input-key".into();
                        command.script = Some(
                            match key {
                                WebKey::Enter => "Enter",
                                WebKey::Tab => "Tab",
                                WebKey::Escape => "Escape",
                                WebKey::Backspace => "Backspace",
                                _ => return Err(Error::Unsupported("WebView input key")),
                            }
                            .into(),
                        );
                    }
                    _ => return Err(Error::Unsupported("WebView input")),
                }
                command
            }
            _ => return Err(Error::Unsupported("WebView command")),
        };
        if self.arkts_mounted.get() {
            node.web_command(web_command);
        } else {
            self.pending_commands.borrow_mut().push(web_command);
        }
        Ok(())
    }

    fn fail_pending_requests(&self, node: &Node, reason: &str) {
        for command in self.pending_commands.borrow_mut().drain(..) {
            if let Some(id) = web_downloads::parse_request_id(command.request_id.as_deref()) {
                node.control.dispatch_event(
                    node.id,
                    EventKind::WebViewReply,
                    nsh_core::webview::reply(id, Err(reason.to_owned())),
                );
            }
        }
    }

    pub(crate) fn close(&self) -> Result<()> {
        if self.closed.replace(true) {
            return Ok(());
        }
        self.evaluations.dispose();
        self.route_ref.borrow_mut().take();
        self.route.borrow_mut().take();
        *self.pending_settings.borrow_mut() = None;
        if let Some(node) = self.node.upgrade() {
            self.fail_pending_requests(&node, "WebView closed");
        } else {
            self.pending_commands.borrow_mut().clear();
        }
        let was_mounted = self.arkts_mounted.replace(false);
        self.waiting_transition.set(false);
        if let Some(node) = self.node.upgrade()
            && let Some(target) = self.target.borrow_mut().take()
        {
            if was_mounted {
                node.web_remove(target);
            } else {
                target.invalidate()?;
            }
        } else if let Some(target) = self.target.borrow_mut().take() {
            target.invalidate()?;
        }
        Ok(())
    }

    fn replace_target(&self, node: &Node) -> Result<u32> {
        self.evaluations.set_web(None);
        self.evaluations.navigated();
        let attachment = NativeAttachment::new(&self.native)?;
        let id = attachment.id();
        let (url_schemes, defer_downloads) = self
            .props
            .borrow()
            .as_ref()
            .map(|p| (p.url_schemes.clone(), p.defer_downloads))
            .unwrap_or_default();
        let entry = Rc::new(Route {
            node: self.node.clone(),
            dispatcher: node.dispatcher.clone(),
            access: node.access.clone(),
            assets: node.assets.clone(),
            host: node.host.clone(),
            url_schemes,
            defer_downloads: Cell::new(defer_downloads),
            proxy: RefCell::new(None),
            web: RefCell::new(None),
            controller_ready: Cell::new(false),
            pending_js: RefCell::default(),
            evaluations: self.evaluations.clone(),
        });
        ROUTES.with(|routes| {
            routes.borrow_mut().insert(id, entry.clone());
        });
        *self.route.borrow_mut() = Some(RouteRegistration(id));
        *self.route_ref.borrow_mut() = Some(entry);
        *self.applied_source.borrow_mut() = None;
        *self.applied_document.borrow_mut() = None;
        *self.pending_settings.borrow_mut() = None;
        self.fail_pending_requests(node, "WebView recreated");
        let was_mounted = self.arkts_mounted.replace(false);
        self.waiting_transition.set(false);
        if let Some(old) = self.target.replace(Some(attachment)) {
            if was_mounted {
                node.web_remove(old);
            } else {
                old.invalidate()?;
            }
        }
        Ok(id)
    }

    fn sync(&self, node: &Node) -> Result<()> {
        if self.closed.get() || !self.ready.get() {
            return Ok(());
        }
        let Some(props) = self.props.borrow().clone() else {
            return Ok(());
        };
        if !props.url_schemes.is_empty() {
            register_url_schemes(&props.url_schemes);
        }
        if !node.access.allows_webviews()
            || matches!(
                &props.source,
                WebSource::Uri(uri)
                    if !uri.starts_with("asset:/")
                        && !has_custom_scheme(uri, &props.url_schemes)
                        && !node.access.allows_webview(uri)
            )
        {
            node.control.dispatch_event(
                node.id,
                EventKind::Error,
                EventPayload::Map(vec![
                    (
                        "message",
                        PayloadValue::Str("WebView source is not granted".into()),
                    ),
                    (
                        "code",
                        PayloadValue::Str(nsh_core::element::codes::ACCESS_DENIED.into()),
                    ),
                ]),
            );
            return Ok(());
        }
        if !props.javascript_enabled {
            self.evaluations.scripts_disabled();
        }
        let shared_cookies = !props.incognito;
        let document = DocumentSettings {
            javascript: props.javascript_enabled,
            shared_cookies,
            script: props.injected_js_before.clone(),
            url_schemes: props.url_schemes.clone(),
        };
        let document_changed = self
            .applied_document
            .borrow()
            .as_ref()
            .is_some_and(|old| old != &document);
        let new_document = document_changed
            || self.target.borrow().is_none()
            || (self.applied_document.borrow().is_none() && !props.url_schemes.is_empty());
        // The initial native target can already exist when the first props
        // arrive. Its private store still needs clearing before navigation.
        let first_document = self.applied_document.borrow().is_none();
        let target_id = if new_document {
            self.replace_target(node)?
        } else {
            self.target.borrow().as_ref().unwrap().id()
        };
        if let Some(route) = self.route_ref.borrow().as_ref() {
            let host: Rc<dyn ScriptHost> = route.clone();
            self.evaluations.set_web(Some(&host));
        }
        *self.applied_document.borrow_mut() = Some(document);

        let source_changed = self.applied_source.borrow().as_ref() != Some(&props.source);
        if source_changed {
            self.evaluations.navigated();
            *self.applied_source.borrow_mut() = Some(props.source.clone());
            self.load_generation
                .set(self.load_generation.get().wrapping_add(1).max(1));
        }
        let (url, html, html_base_url) = match &props.source {
            WebSource::None => (Some("about:blank".into()), None, None),
            WebSource::Uri(uri) => (Some(asset_url(uri)), None, None),
            WebSource::Html { html, base_url } => (
                None,
                Some(html.to_string()),
                base_url.as_deref().map(asset_url),
            ),
        };
        let document_start_script = props
            .injected_js_before
            .as_ref()
            .map_or_else(|| BRIDGE.to_owned(), |script| format!("{BRIDGE}\n{script}"));
        let settings = WebSettings {
            target_id,
            shared_cookies,
            clear_cookies: !shared_cookies && (first_document || new_document),
            url,
            html,
            html_base_url,
            load_generation: self.load_generation.get().max(1),
            java_script_enabled: props.javascript_enabled,
            scroll_enabled: props.scrollable,
            user_agent: props.user_agent.as_deref().map(str::to_owned),
            injected_javascript: props.injected_js.as_deref().map(str::to_owned),
            document_start_script,
            url_schemes: props
                .url_schemes
                .iter()
                .map(|s| s.scheme().to_owned())
                .collect(),
        };
        if self.arkts_mounted.get() {
            node.web_update(settings);
        } else {
            *self.pending_settings.borrow_mut() = Some(settings);
            if node.flushing_batch.get() {
                node.deferred_webs.borrow_mut().push(self.node.clone());
            } else {
                self.flush_mount_gate(node)?;
            }
        }
        Ok(())
    }

    pub(crate) fn flush_mount_gate(&self, node: &Node) -> Result<()> {
        if self.closed.get()
            || !self.ready.get()
            || !node.active.get()
            || node.pending_delete.get()
            || self.arkts_mounted.get()
            || self.waiting_transition.get()
            || self.pending_settings.borrow().is_none()
        {
            return Ok(());
        }
        // Defer ArkWeb creation until any enclosing Screen/Modal entrance
        // transition settles so `CreateNWeb` never stalls push/open animation.
        let mut cursor = node.parent_node.borrow().upgrade();
        while let Some(ancestor) = cursor {
            if let Some(screen) = ancestor.screen.borrow().as_ref().cloned()
                && screen.is_entering()
            {
                self.waiting_transition.set(true);
                let weak = self.node.clone();
                screen.when_settled(move |entered| {
                    crate::render::node::callback(|| {
                        if let Some(node) = weak.upgrade()
                            && let Some(web) = node.web.borrow().as_ref()
                        {
                            web.on_transition_settled(&node, entered);
                        }
                        Ok(())
                    });
                });
                return Ok(());
            }
            if let Some(modal) = ancestor.modal.borrow().as_ref().cloned()
                && modal.is_entering()
            {
                self.waiting_transition.set(true);
                let weak = self.node.clone();
                modal.when_settled(move |entered| {
                    crate::render::node::callback(|| {
                        if let Some(node) = weak.upgrade()
                            && let Some(web) = node.web.borrow().as_ref()
                        {
                            web.on_transition_settled(&node, entered);
                        }
                        Ok(())
                    });
                });
                return Ok(());
            }
            cursor = ancestor.parent_node.borrow().upgrade();
        }
        self.emit_arkts_mount(node);
        Ok(())
    }

    fn on_transition_settled(&self, node: &Node, entered: bool) {
        self.waiting_transition.set(false);
        if !entered
            || self.closed.get()
            || !self.ready.get()
            || !node.active.get()
            || node.pending_delete.get()
            || self.arkts_mounted.get()
        {
            *self.pending_settings.borrow_mut() = None;
            self.fail_pending_requests(node, "WebView mount cancelled");
            return;
        }
        self.emit_arkts_mount(node);
    }

    fn emit_arkts_mount(&self, node: &Node) {
        let Some(settings) = self.pending_settings.borrow_mut().take() else {
            return;
        };
        let target_id = settings.target_id;
        self.arkts_mounted.set(true);
        node.web_update(settings);
        let commands = std::mem::take(&mut *self.pending_commands.borrow_mut());
        for command in commands {
            if command.target_id == target_id {
                node.web_command(command);
            }
        }
    }
}

pub(crate) fn on_controller_attached(target_id: u32, native_tag: &str) -> napi_ohos::Result<bool> {
    NativeAttachment::dispatch(target_id, || {
        let Some(route) = ROUTES.with(|routes| routes.borrow().get(&target_id).cloned()) else {
            return Ok(false);
        };
        let Some(node) = route.node.upgrade() else {
            return Ok(false);
        };
        if native_tag.contains('\0') {
            return Err(napi_ohos::Error::from_reason("Invalid WebView tag"));
        }
        install_asset_protocol(
            native_tag,
            route.assets.clone(),
            route.host.clone(),
            &route.url_schemes,
        )?;
        let control = node.control.clone();
        let node_id = node.id;
        let eval_dispatcher = route.dispatcher.clone();
        let proxy = WebProxyBuilder::new(native_tag.to_owned(), "__nshNative".to_owned())
            .add_method("postMessage", move |_tag, args| {
                if let Some(data) = args.first() {
                    control.dispatch_event(
                        node_id,
                        EventKind::Message,
                        EventPayload::Map(vec![("data", PayloadValue::Str(data.clone()))]),
                    );
                }
            })
            .add_method("evalResult", move |_tag, args| {
                if let (Some(key), Some(envelope)) = (args.first(), args.get(1))
                    && let Some(dispatcher) = eval_dispatcher.upgrade()
                {
                    dispatcher.send(crate::app::runtime::Message::WebEvalResult {
                        target_id,
                        key: key.clone(),
                        envelope: envelope.clone(),
                    });
                }
            })
            .build()
            .map_err(|error| {
                napi_ohos::Error::from_reason(format!("Unable to build WebView JS proxy: {error}"))
            })?;
        *route.web.borrow_mut() = Some(Web::new(native_tag.to_owned()));
        *route.proxy.borrow_mut() = Some(proxy);
        Ok(true)
    })?
}

fn install_asset_protocol(
    native_tag: &str,
    assets: Arc<dyn StreamAssets>,
    host: Arc<Services>,
    url_schemes: &[nsh_core::web_resource::WebUrlScheme],
) -> napi_ohos::Result<()> {
    let handler = CustomProtocolHandler::new();
    handler.on_request_start(move |request, request_handle| {
        let url = request.url();
        let name = asset_name_from_url(&url);
        let assets = assets.clone();
        let permit = host.reserve_load();
        let responder = AssetReply(Some(request_handle));
        let job = move || {
            let result = permit.map_err(|e| e.to_string()).and_then(|_permit| {
                name.as_deref()
                    .ok_or_else(|| "invalid asset URL".to_owned())
                    .and_then(|name| assets.read(&format!("asset:/{name}")))
            });
            let mime = match std::path::Path::new(name.as_deref().unwrap_or_default())
                .extension()
                .and_then(|x| x.to_str())
            {
                Some("html" | "htm") => "text/html",
                Some("js" | "mjs") => "text/javascript",
                Some("css") => "text/css",
                Some("svg") => "image/svg+xml",
                Some("png") => "image/png",
                Some("jpg" | "jpeg") => "image/jpeg",
                Some("woff2") => "font/woff2",
                _ => "application/octet-stream",
            };
            let (status, bytes) = match result {
                Ok(bytes) => (200, bytes),
                Err(error) => (404, error.into_bytes()),
            };
            responder.respond(status, mime, &bytes);
        };
        if let Err(error) = Executor::assets().and_then(|pool| pool.submit(job)) {
            log::error!("OHOS WebView asset worker: {error}");
        }
        true
    });
    let attached = Web::new(native_tag.to_owned())
        .custom_protocol(SCHEME.to_owned(), handler)
        .map_err(|error| {
            napi_ohos::Error::from_reason(format!(
                "Failed to bind WebView custom protocol: {error}"
            ))
        })?;
    if !attached {
        return Err(napi_ohos::Error::from_reason(
            "ArkWeb rejected the custom protocol handler for this WebView tag",
        ));
    }
    ARKWEB_INITIALIZED.store(true, Ordering::Release);
    for scheme in url_schemes {
        let name = scheme.scheme();
        if !CustomProtocol::is_protocol_registered(name) {
            log::warn!(
                "OHOS WebView custom scheme `{name}` was not registered before ArkWeb initialization"
            );
        }
        let scheme_handler = CustomProtocolHandler::new();
        let scheme_clone = scheme.clone();
        scheme_handler.on_request_start(move |request, request_handle| {
            let url = request.url();
            let method = request.method();
            let responder = AssetReply(Some(request_handle));
            let (req, _cancel) = nsh_core::web_resource::WebResourceRequest::new(
                url,
                if method.is_empty() {
                    "GET".to_owned()
                } else {
                    method
                },
                move |resource| match resource {
                    Some(res) => responder.respond(200, res.mime_type_or_default(), &res.bytes),
                    None => responder.respond(404, "text/plain; charset=utf-8", &[]),
                },
            );
            scheme_clone.handle(req);
            true
        });
        match Web::new(native_tag.to_owned()).custom_protocol(name.to_owned(), scheme_handler) {
            Ok(true) => {}
            Ok(false) => {
                log::warn!(
                    "ArkWeb rejected custom protocol handler for scheme `{name}` on tag `{native_tag}`"
                );
            }
            Err(error) => {
                log::warn!(
                    "Failed to bind ArkWeb custom protocol `{name}` on tag `{native_tag}`: {error}"
                );
            }
        }
    }
    Ok(())
}

pub(crate) fn on_navigate(target_id: u32, url: &str) -> napi_ohos::Result<bool> {
    NativeAttachment::dispatch(target_id, || {
        let Some(route) = ROUTES.with(|routes| routes.borrow().get(&target_id).cloned()) else {
            return true;
        };
        let Some(node) = route.node.upgrade() else {
            return true;
        };
        if node
            .dispatcher
            .upgrade()
            .is_none_or(|d| !d.live.load(std::sync::atomic::Ordering::Acquire))
        {
            return true;
        }
        !allows_navigation(&route.access, url, &route.url_schemes)
    })
}

pub(crate) fn download_allowed(target_id: u32, _url: &str) -> napi_ohos::Result<bool> {
    NativeAttachment::dispatch(target_id, || {
        let Some(route) = ROUTES.with(|routes| routes.borrow().get(&target_id).cloned()) else {
            return false;
        };
        let Some(node) = route.node.upgrade() else {
            return false;
        };
        // Offer the transfer to `nsh-core` whenever the app listens or a Rust
        // builder asked to defer downloads. `nsh-core` enforces `webview.hosts`
        // so disallowed URLs still log a denial and report `ERR_ACCESS_DENIED`.
        let mask = node.interaction.mask();
        route.defer_downloads.get()
            || mask.contains(EventKind::Download)
            || mask.contains(EventKind::DownloadPending)
    })
}

fn dispatch_download_lifecycle(event: WebEvent) -> bool {
    let target_id = event.target_id;
    let route_target = ROUTES.with(|routes| {
        routes
            .borrow()
            .get(&target_id)
            .and_then(|route| route.node.upgrade())
            .map(|node| (node.id, node.control.clone()))
    });
    let target = route_target.or_else(|| {
        DOWNLOAD_SINKS.with(|sinks| {
            sinks
                .borrow()
                .get(&target_id)
                .map(|sink| (sink.node_id, sink.control.clone()))
        })
    });
    let Some((node_id, control)) = target else {
        return false;
    };
    let Some(download) = event.download else {
        return true;
    };
    match event.phase.as_str() {
        "download-end" => {
            let download_id = download.id.clone();
            if let Some(payload) = web_downloads::build_end_payload(
                download.id,
                &download.state,
                download.bytes,
                download.error_code,
                event.code,
                event.message,
            ) {
                control.dispatch_event(node_id, EventKind::DownloadEnd, payload);
            }
            finish_download_sink(target_id, &download_id);
        }
        "web-reply" => {
            let ok = download.ok.unwrap_or(false);
            let cancel_done = ok && download.state == "cancel-ok" && !download.id.is_empty();
            let download_id = download.id;
            if let Some(payload) = web_downloads::build_reply_payload(
                download.request_id.as_deref(),
                ok,
                event.message,
            ) {
                control.dispatch_event(node_id, EventKind::WebViewReply, payload);
            }
            if cancel_done {
                finish_download_sink(target_id, &download_id);
            }
        }
        _ => return false,
    }
    true
}

fn finish_download_sink(target_id: u32, download_id: &str) {
    DOWNLOAD_SINKS.with(|sinks| {
        let mut sinks = sinks.borrow_mut();
        if let Some(sink) = sinks.get_mut(&target_id) {
            sink.active_ids.remove(download_id);
            if sink.closed && sink.active_ids.is_empty() {
                sinks.remove(&target_id);
            }
        }
    });
}

pub(crate) fn on_event(event: WebEvent) -> napi_ohos::Result<bool> {
    if matches!(event.phase.as_str(), "download-end" | "web-reply") {
        return Ok(dispatch_download_lifecycle(event));
    }
    NativeAttachment::dispatch(event.target_id, || {
        let Some(route) = ROUTES.with(|routes| routes.borrow().get(&event.target_id).cloned())
        else {
            return false;
        };
        let Some(node) = route.node.upgrade() else {
            return false;
        };
        let Some(dispatcher) = node
            .dispatcher
            .upgrade()
            .filter(|d| d.live.load(std::sync::atomic::Ordering::Acquire))
        else {
            return false;
        };
        let state = || {
            EventPayload::Map(vec![
                ("url", PayloadValue::Str(event.url.clone())),
                ("title", PayloadValue::Str(event.title.clone())),
                ("loading", PayloadValue::Bool(event.loading)),
                ("canGoBack", PayloadValue::Bool(event.can_go_back)),
                ("canGoForward", PayloadValue::Bool(event.can_go_forward)),
            ])
        };
        match event.phase.as_str() {
            "ready" => {
                route.on_ready();
            }
            "render-exited" => {
                route.evaluations.process_terminated();
                node.control.dispatch_event(
                    node.id,
                    EventKind::Error,
                    EventPayload::Map(vec![
                        (
                            "message",
                            PayloadValue::Str("The web content process terminated".into()),
                        ),
                        (
                            "code",
                            PayloadValue::Str("ERR_NSH_WEBVIEW_TERMINATED".into()),
                        ),
                    ]),
                );
            }
            "focus" | "blur" => {
                let focus = event.phase == "focus";
                let mut keyboard = dispatcher.keyboard.lock().unwrap();
                if focus {
                    keyboard.focused = Some(node.id);
                } else if keyboard.focused == Some(node.id) {
                    keyboard.focused = None;
                }
                drop(keyboard);
                if focus {
                    node.host.activate(node.dispatcher.clone());
                }
                node.control.dispatch_event(
                    node.id,
                    if focus {
                        EventKind::Focus
                    } else {
                        EventKind::Blur
                    },
                    EventPayload::None,
                );
            }
            "download-offer" => {
                if let Some(download) = event.download {
                    let download_id = download.id.clone();
                    DOWNLOAD_SINKS.with(|sinks| {
                        let mut sinks = sinks.borrow_mut();
                        let sink = sinks
                            .entry(event.target_id)
                            .or_insert_with(|| DownloadSink {
                                node_id: node.id,
                                control: node.control.clone(),
                                active_ids: FxHashSet::default(),
                                closed: false,
                            });
                        sink.closed = false;
                        sink.active_ids.insert(download_id);
                    });
                    let payload = web_downloads::build_offer_payload(
                        download.id,
                        event.url,
                        &download.filename,
                        download.mime_type,
                        download.bytes,
                        download.method,
                    );
                    node.control
                        .dispatch_event(node.id, EventKind::DownloadOffer, payload);
                }
            }
            "download-progress" => {
                if let Some(download) = event.download {
                    let payload =
                        web_downloads::build_progress_payload(download.id, download.bytes);
                    node.control
                        .dispatch_event(node.id, EventKind::DownloadProgress, payload);
                }
            }
            "error" => {
                node.control.dispatch_event(
                    node.id,
                    EventKind::Error,
                    EventPayload::Map(vec![
                        (
                            "message",
                            PayloadValue::Str(event.message.clone().unwrap_or_default()),
                        ),
                        (
                            "code",
                            PayloadValue::Number(f64::from(event.code.unwrap_or_default())),
                        ),
                        ("url", PayloadValue::Str(event.url.clone())),
                    ]),
                );
                node.control
                    .dispatch_event(node.id, EventKind::LoadEnd, state());
            }
            "start" => {
                route.evaluations.navigated();
                node.control
                    .dispatch_event(node.id, EventKind::LoadStart, state());
                node.control
                    .dispatch_event(node.id, EventKind::NavigationStateChange, state());
            }
            "finish" => {
                node.control
                    .dispatch_event(node.id, EventKind::Load, state());
                node.control
                    .dispatch_event(node.id, EventKind::LoadEnd, state());
                node.control
                    .dispatch_event(node.id, EventKind::NavigationStateChange, state());
            }
            _ => {
                node.control
                    .dispatch_event(node.id, EventKind::NavigationStateChange, state());
            }
        }
        true
    })
}

pub(crate) fn abort_evaluation(control: &EngineControl, node_id: nsh_core::NodeId, request: u32) {
    control.dispatch_event(node_id, EventKind::WebEvaluation, abort_payload(request));
}

fn asset_url(uri: &str) -> String {
    uri.strip_prefix("asset:/")
        .map_or_else(|| uri.to_owned(), |path| format!("{SCHEME}://app/{path}"))
}

fn asset_name_from_url(url: &str) -> Option<String> {
    let rest = url
        .trim()
        .strip_prefix("nsh-asset://")
        .or_else(|| url.trim().strip_prefix("NSH-ASSET://"))?;
    let path = rest
        .strip_prefix("app/")
        .or_else(|| rest.strip_prefix("localhost/"))
        .unwrap_or(rest);
    let path = path.strip_prefix('/').unwrap_or(path);
    let path = path.split(['?', '#']).next().unwrap_or("");
    if path.is_empty() {
        return None;
    }
    percent_encoding::percent_decode_str(path)
        .decode_utf8()
        .ok()
        .map(|s| s.into_owned())
}

fn has_custom_scheme(url: &str, schemes: &[nsh_core::web_resource::WebUrlScheme]) -> bool {
    let Some((scheme, _)) = url.split_once(':') else {
        return false;
    };
    schemes
        .iter()
        .any(|s| s.scheme().eq_ignore_ascii_case(scheme))
}

fn allows_navigation(
    access: &Access,
    url: &str,
    schemes: &[nsh_core::web_resource::WebUrlScheme],
) -> bool {
    // ArkWeb represents loadData's already-authorized inline HTML as a data
    // URL in onLoadIntercept. It is not an additional network capability.
    (access.allows_webviews()
        && (url.is_empty()
            || url == "about:srcdoc"
            || url.starts_with("data:text/html;")
            || url.starts_with("nsh-asset://app/")
            || has_custom_scheme(url, schemes)))
        || access.allows_webview(url)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asset_url_maps_asset_scheme_to_nsh_asset_app_origin() {
        assert_eq!(
            asset_url("asset:/docs/index.html"),
            "nsh-asset://app/docs/index.html"
        );
        assert_eq!(asset_url("https://example.com"), "https://example.com");
    }

    #[test]
    fn asset_name_from_url_decodes_url_path() {
        assert_eq!(
            asset_name_from_url("nsh-asset://app/docs/hello%20world.html?x=1#top").as_deref(),
            Some("docs/hello world.html")
        );
    }
}
