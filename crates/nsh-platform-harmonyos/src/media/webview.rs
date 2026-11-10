//! Embedded ArkWeb views (`<web-view>`), `nsh-asset://` custom protocol, and `window.nsh.postMessage`.

use std::{
    cell::{Cell, RefCell},
    rc::{Rc, Weak},
    sync::{Arc, OnceLock},
};

use nsh_core::{Command, EventKind, EventPayload, PayloadValue, WebProps, WebSource};
use nsh_engine::Access;
use ohos_arkui_binding::{
    component::{attribute::ArkUICommonAttribute, built_in_component::Custom},
    types::advanced::{LengthMetricUnit, NodeDirtyFlag},
};
use ohos_web_binding::{
    ArkWebResponse, CustomProtocol, CustomProtocolHandler, CustomProtocolOption, ResourceHandle,
    Web, WebProxy, WebProxyBuilder,
};
use rustc_hash::FxHashMap;

use crate::{
    app::session::Services,
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

struct Route {
    node: Weak<Node>,
    access: Arc<Access>,
    assets: Arc<dyn StreamAssets>,
    host: Arc<Services>,
    url_schemes: Vec<nsh_core::web_resource::WebUrlScheme>,
    proxy: RefCell<Option<WebProxy>>,
}

thread_local! {
    static ROUTES: RefCell<FxHashMap<u32, Rc<Route>>> = RefCell::default();
}

struct RouteRegistration(u32);
impl Drop for RouteRegistration {
    fn drop(&mut self) {
        ROUTES.with(|routes| {
            routes.borrow_mut().remove(&self.0);
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
    frame: Cell<Size>,
    props: RefCell<Option<WebProps>>,
    applied_source: RefCell<Option<WebSource>>,
    applied_document: RefCell<Option<DocumentSettings>>,
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
        let mut target = None;
        let mut route = None;
        if owner.host.check().is_ok() {
            let attachment = NativeAttachment::new(&native)?;
            let id = attachment.id();
            ROUTES.with(|routes| {
                routes.borrow_mut().insert(
                    id,
                    Rc::new(Route {
                        node: node.clone(),
                        access: owner.access.clone(),
                        assets: owner.assets.clone(),
                        host: owner.host.clone(),
                        url_schemes: Vec::new(),
                        proxy: RefCell::new(None),
                    }),
                );
            });
            route = Some(RouteRegistration(id));
            target = Some(attachment);
        }
        Ok(Self {
            ready: Cell::new(owner.host.check().is_ok()),
            node,
            native,
            target: RefCell::new(target),
            route: RefCell::new(route),
            frame: Cell::new(Size::default()),
            props: RefCell::new(None),
            applied_source: RefCell::new(None),
            applied_document: RefCell::new(None),
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
            self.route.borrow_mut().take();
            *self.applied_source.borrow_mut() = None;
            *self.applied_document.borrow_mut() = None;
            if let Some(target) = self.target.borrow_mut().take() {
                node.web_remove(target);
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
        *self.props.borrow_mut() = Some(props);
        if let Some(node) = self.node.upgrade() {
            self.sync(&node)?;
        }
        Ok(())
    }

    pub(crate) fn command(&self, command: Command) -> Result<()> {
        if self.closed.get() || !self.ready.get() {
            return Ok(());
        }
        let Some(node) = self.node.upgrade() else {
            return Ok(());
        };
        let Some(target_id) = self.target.borrow().as_ref().map(NativeAttachment::id) else {
            return Ok(());
        };
        let web_command = match command {
            Command::Reload => WebCommand {
                target_id,
                kind: "reload".into(),
                script: None,
                ..Default::default()
            },
            Command::GoBack => WebCommand {
                target_id,
                kind: "go-back".into(),
                script: None,
                ..Default::default()
            },
            Command::GoForward => WebCommand {
                target_id,
                kind: "go-forward".into(),
                script: None,
                ..Default::default()
            },
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
            Command::Blur => WebCommand {
                target_id,
                kind: "evaluate".into(),
                script: Some("document.activeElement?.blur();".into()),
                ..Default::default()
            },
            Command::InjectJavaScript(script) => WebCommand {
                target_id,
                kind: "evaluate".into(),
                script: Some(script),
                ..Default::default()
            },
            Command::PostMessage(data) => {
                let literal = serde_json::to_string(&data).expect("string literal");
                WebCommand {
                    target_id,
                    kind: "evaluate".into(),
                    script: Some(format!(
                        "window.dispatchEvent(new MessageEvent('message',{{data:{literal}}}));document.dispatchEvent(new MessageEvent('message',{{data:{literal}}}));"
                    )),
                    ..Default::default()
                }
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
        node.web_command(web_command);
        Ok(())
    }

    pub(crate) fn close(&self) -> Result<()> {
        if self.closed.replace(true) {
            return Ok(());
        }
        self.route.borrow_mut().take();
        if let Some(node) = self.node.upgrade()
            && let Some(target) = self.target.borrow_mut().take()
        {
            node.web_remove(target);
        } else if let Some(target) = self.target.borrow_mut().take() {
            target.invalidate()?;
        }
        Ok(())
    }

    fn replace_target(&self, node: &Node) -> Result<u32> {
        let attachment = NativeAttachment::new(&self.native)?;
        let id = attachment.id();
        let url_schemes = self
            .props
            .borrow()
            .as_ref()
            .map(|p| p.url_schemes.clone())
            .unwrap_or_default();
        ROUTES.with(|routes| {
            routes.borrow_mut().insert(
                id,
                Rc::new(Route {
                    node: self.node.clone(),
                    access: node.access.clone(),
                    assets: node.assets.clone(),
                    host: node.host.clone(),
                    url_schemes,
                    proxy: RefCell::new(None),
                }),
            );
        });
        *self.route.borrow_mut() = Some(RouteRegistration(id));
        *self.applied_source.borrow_mut() = None;
        *self.applied_document.borrow_mut() = None;
        if let Some(old) = self.target.replace(Some(attachment)) {
            node.web_remove(old);
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
        *self.applied_document.borrow_mut() = Some(document);

        let source_changed = self.applied_source.borrow().as_ref() != Some(&props.source);
        if source_changed {
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
        node.web_update(WebSettings {
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
            download_directory: node
                .host
                .data_directory()
                .join("downloads")
                .display()
                .to_string(),
        });
        Ok(())
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
        install_asset_protocol(
            native_tag,
            route.assets.clone(),
            route.host.clone(),
            &route.url_schemes,
        )?;
        let control = node.control.clone();
        let node_id = node.id;
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
            .build()
            .map_err(|error| {
                napi_ohos::Error::from_reason(format!("Unable to build WebView JS proxy: {error}"))
            })?;
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
    for scheme in url_schemes {
        let name = scheme.scheme();
        if !CustomProtocol::is_protocol_registered(name) {
            CustomProtocol::add_protocol_with_option(
                name,
                CustomProtocolOption::Standard
                    | CustomProtocolOption::Secure
                    | CustomProtocolOption::CorsEnabled
                    | CustomProtocolOption::FetchEnabled,
            );
            CustomProtocol::register();
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
        let _ = Web::new(native_tag.to_owned()).custom_protocol(name.to_owned(), scheme_handler);
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

pub(crate) fn download_allowed(target_id: u32, url: &str) -> napi_ohos::Result<bool> {
    NativeAttachment::dispatch(target_id, || {
        let Some(route) = ROUTES.with(|routes| routes.borrow().get(&target_id).cloned()) else {
            return false;
        };
        let Some(node) = route.node.upgrade() else {
            return false;
        };
        node.interaction.mask().contains(EventKind::Download)
            && (route.access.allows_webview(url)
                || (route.access.allows_webviews()
                    && (url.starts_with("data:") || url.starts_with("blob:"))))
    })
}

pub(crate) fn on_event(event: WebEvent) -> napi_ohos::Result<bool> {
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
            "download" => {
                if let Some(download) = event.download {
                    node.control.dispatch_event(
                        node.id,
                        EventKind::Download,
                        EventPayload::Map(vec![
                            ("state", PayloadValue::Str(download.state)),
                            ("id", PayloadValue::Str(download.id)),
                            ("url", PayloadValue::Str(event.url)),
                            ("filename", PayloadValue::Str(download.filename)),
                            ("path", PayloadValue::Str(download.path)),
                            ("bytes", PayloadValue::Number(download.bytes)),
                            (
                                "message",
                                PayloadValue::Str(event.message.unwrap_or_default()),
                            ),
                        ]),
                    );
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
