//! System UI requests are scoped to an engine generation. ArkTS owns their
//! controllers on the window thread; late completions hold only weak routes.
use std::collections::BTreeMap;
#[cfg(target_env = "ohos")]
use std::sync::{
    Mutex, Weak,
    atomic::{AtomicU32, Ordering},
};

#[cfg(target_env = "ohos")]
use nsh_core::Command;
use nsh_core::{EventKind, EventPayload, PayloadValue};

#[cfg(target_env = "ohos")]
use crate::app::{
    runtime::{Dispatcher, Message},
    session::{HostCommand, Services},
};

struct Request<R> {
    route: std::sync::Weak<R>,
    kind: EventKind,
    id: u32,
    stream: bool,
    answer: bool,
    paste: Option<(u32, bool, Option<u64>, u64)>,
}

impl<R> Clone for Request<R> {
    fn clone(&self) -> Self {
        Self {
            route: self.route.clone(),
            kind: self.kind,
            id: self.id,
            stream: self.stream,
            answer: self.answer,
            paste: self.paste,
        }
    }
}

impl<R> Request<R> {
    fn map_payload(&self, value: SystemEvent) -> Option<EventPayload> {
        let mut fields = Vec::new();
        if self.answer {
            fields.push(("id", PayloadValue::Number(f64::from(self.id))));
        }
        // Answer-less notification listeners still deliver press replies.
        value.append_fields(&mut fields);
        (!fields.is_empty()).then_some(EventPayload::Map(fields))
    }
}

fn insert_request<R>(
    requests: &mut BTreeMap<u32, Request<R>>,
    key: u32,
    request: Request<R>,
    live: impl Fn(&R) -> bool,
) -> Result<(), String> {
    requests.retain(|_, old| {
        !(request.stream
            && request.kind == EventKind::Notification
            && old.stream
            && old.kind == EventKind::Notification)
            && old.route.upgrade().is_some_and(|route| live(&route))
    });
    if requests.len() >= 256 {
        return Err("too many system requests".into());
    }
    requests.insert(key, request);
    Ok(())
}

#[cfg(target_env = "ohos")]
#[derive(Default)]
pub(crate) struct System {
    next: AtomicU32,
    requests: Mutex<BTreeMap<u32, Request<Dispatcher>>>,
    clipboard: crate::services::clipboard::Clipboard,
}

#[cfg(target_env = "ohos")]
impl System {
    /// Writes plain text to the system clipboard on the serialized worker
    /// without emitting a `ClipboardWrite` receipt to core.
    pub(crate) fn copy_clipboard(&self, text: String) -> napi_ohos::Result<()> {
        self.clipboard.copy_text(text)
    }

    pub(crate) fn command(
        &self,
        host: &std::sync::Arc<Services>,
        route: Weak<Dispatcher>,
        command: Command,
    ) -> napi_ohos::Result<()> {
        use nsh_core::notifications::NotificationCommand as N;
        match command {
            Command::SetClipboardItem(item) => {
                return self.clipboard.write_text(route, item.text, item.html);
            }
            Command::SetClipboard(text) => {
                return self.clipboard.write_text(route, text, None);
            }
            Command::SetClipboardImage(uri) => {
                return self.clipboard.write_image(uri);
            }
            Command::ReadClipboard { id, has_only } => {
                return self.clipboard.read_text(host.clone(), route, id, has_only);
            }
            Command::ReadClipboardImage { id, has_only } => {
                return self.clipboard.read_image(host.clone(), route, id, has_only);
            }
            Command::ReadClipboardFiles { id } => {
                return self.clipboard.read_files(host.clone(), route, id);
            }
            _ => {}
        }
        let (kind, id, stream, data) = match command {
            Command::Notification(command) => {
                let (id, stream, data) = match command {
                    N::Permission { id, ask } => (
                        id,
                        false,
                        SystemCommand {
                            op: "notification-permission".into(),
                            ask: Some(ask),
                            ..Default::default()
                        },
                    ),
                    N::Show { id, notification } => (
                        id,
                        false,
                        SystemCommand {
                            op: "notification-show".into(),
                            name: Some(notification.id),
                            title: Some(notification.title),
                            body: Some(notification.body),
                            silent: Some(notification.silent),
                            ..Default::default()
                        },
                    ),
                    N::Close(name) => (
                        0,
                        false,
                        SystemCommand {
                            op: "notification-close".into(),
                            name: Some(name),
                            ..Default::default()
                        },
                    ),
                    N::SetBadge(count) => (
                        0,
                        false,
                        SystemCommand {
                            op: "notification-badge".into(),
                            count: Some(count),
                            ..Default::default()
                        },
                    ),
                    N::Listen => (0, true, SystemCommand::notification_listener(command)),
                    _ => {
                        return Err(napi_ohos::Error::from_reason(
                            "unsupported notification command",
                        ));
                    }
                };
                (EventKind::Notification, id, stream, data)
            }
            Command::AuthSession { id, page } => {
                // One auth session runs at a time per route; cancel any prior pending entry
                // and deliver a cancellation reply so its JS Promise does not hang.
                let previous: Vec<(u32, Request<Dispatcher>)> = {
                    let mut requests = self.requests.lock().unwrap();
                    let keys: Vec<u32> = requests
                        .iter()
                        .filter_map(|(&key, r)| {
                            (r.kind == EventKind::AuthSession && Weak::ptr_eq(&r.route, &route))
                                .then_some(key)
                        })
                        .collect();
                    keys.into_iter()
                        .filter_map(|key| requests.remove(&key).map(|req| (key, req)))
                        .collect()
                };
                for (key, req) in previous {
                    Self::deliver(
                        req,
                        SystemEvent {
                            phase: Some("cancelled".into()),
                            ..Default::default()
                        },
                    );
                    let _ = host.send_host_command(HostCommand {
                        kind: "system".into(),
                        open_id: Some(key),
                        system: Some(SystemCommand {
                            op: "auth-session-close".into(),
                            ..Default::default()
                        }),
                        ..Default::default()
                    });
                }
                (
                    EventKind::AuthSession,
                    id,
                    false,
                    SystemCommand {
                        op: "auth-session-open".into(),
                        uri: Some(page.url),
                        name: Some(page.callback_scheme),
                        ephemeral: Some(page.ephemeral),
                        ..Default::default()
                    },
                )
            }
            Command::CloseAuthSession(id) => {
                let removed = {
                    let mut requests = self.requests.lock().unwrap();
                    let key = requests.iter().find_map(|(&key, r)| {
                        (r.kind == EventKind::AuthSession
                            && r.id == id
                            && Weak::ptr_eq(&r.route, &route))
                        .then_some(key)
                    });
                    key.and_then(|key| requests.remove(&key).map(|req| (key, req)))
                };
                if let Some((key, req)) = removed {
                    Self::deliver(
                        req,
                        SystemEvent {
                            phase: Some("cancelled".into()),
                            ..Default::default()
                        },
                    );
                    host.send_host_command(HostCommand {
                        kind: "system".into(),
                        open_id: Some(key),
                        system: Some(SystemCommand {
                            op: "auth-session-close".into(),
                            ..Default::default()
                        }),
                        ..Default::default()
                    })?;
                }
                return Ok(());
            }
            Command::Share { id, sheet } => (
                EventKind::Share,
                id,
                false,
                SystemCommand {
                    op: "share".into(),
                    text: sheet.text,
                    uri: sheet.url,
                    title: sheet.title,
                    paths: Some(sheet.files),
                    ..Default::default()
                },
            ),
            _ => return Err(napi_ohos::Error::from_reason("unsupported system command")),
        };
        let answer = !matches!(
            data.op.as_str(),
            "notification-close" | "notification-badge" | "notification-listen"
        );
        self.submit(
            host,
            Request {
                route,
                kind,
                id,
                stream,
                answer,
                paste: None,
            },
            data,
        )
    }

    pub(crate) fn open_app_settings(
        &self,
        host: &Services,
        route: Weak<Dispatcher>,
        id: u32,
    ) -> napi_ohos::Result<()> {
        self.submit(
            host,
            Request {
                route,
                kind: EventKind::OpenAppSettings,
                id,
                stream: false,
                answer: true,
                paste: None,
            },
            SystemCommand {
                op: "open-app-settings".into(),
                ..Default::default()
            },
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn paste_to_input(
        &self,
        host: &std::sync::Arc<Services>,
        route: Weak<Dispatcher>,
        node_id: u32,
        report_only: bool,
        paste_max_size: Option<u64>,
        expected_edit_gen: u64,
    ) -> napi_ohos::Result<()> {
        self.clipboard.paste_to_input(
            host.clone(),
            route,
            node_id,
            report_only,
            paste_max_size,
            expected_edit_gen,
        )
    }

    pub(crate) fn file_request(
        &self,
        host: &Services,
        route: Weak<Dispatcher>,
        id: u32,
        request: nsh_core::file_dialog::FileRequest,
    ) -> napi_ohos::Result<()> {
        use nsh_core::file_dialog::FileRequest as F;
        let fail = |message: String| {
            if let Some(route) = route.upgrade() {
                route.send(Message::HostEvent(
                    EventKind::FileRequest,
                    EventPayload::file_failed(id, message),
                ));
            }
            Ok(())
        };
        let data = match request {
            F::OpenPath(path) => {
                if let Err(err) = validate_existing_file_request_path(&path) {
                    return fail(err);
                }
                SystemCommand {
                    op: "file-open-path".into(),
                    uri: Some(path.to_string_lossy().into_owned()),
                    ..Default::default()
                }
            }
            F::OpenDialog(dialog) => {
                let mode = match dialog.kind {
                    nsh_core::file_dialog::OpenDialogKind::Folders => "folders",
                    nsh_core::file_dialog::OpenDialogKind::FilesAndFolders => "both",
                    _ => "files",
                };
                let filters = format_ohos_open_filters(&dialog.filters);
                SystemCommand {
                    op: "file-open-dialog".into(),
                    name: Some(mode.into()),
                    count: Some(if dialog.multiple { 500 } else { 1 }),
                    paths: (!filters.is_empty()).then_some(filters),
                    uri: dialog.directory.map(|p| p.to_string_lossy().into_owned()),
                    title: dialog.title,
                    ..Default::default()
                }
            }
            F::SaveDialog(dialog) => {
                let choices = format_ohos_save_choices(&dialog.filters);
                SystemCommand {
                    op: "file-save-dialog".into(),
                    name: dialog.file_name,
                    paths: (!choices.is_empty()).then_some(choices),
                    uri: dialog.directory.map(|p| p.to_string_lossy().into_owned()),
                    title: dialog.title,
                    ..Default::default()
                }
            }
            F::RevealPath(path) => {
                if let Err(err) = validate_existing_file_request_path(&path) {
                    return fail(err);
                }
                SystemCommand {
                    op: "file-reveal-path".into(),
                    uri: Some(path.to_string_lossy().into_owned()),
                    ..Default::default()
                }
            }
            F::MoveToTrash(path) => {
                if let Err(err) = validate_existing_file_request_path(&path) {
                    return fail(err);
                }
                SystemCommand {
                    op: "file-move-to-trash".into(),
                    uri: Some(path.to_string_lossy().into_owned()),
                    ..Default::default()
                }
            }
            _ => return fail("unsupported file request on HarmonyOS".into()),
        };
        self.submit(
            host,
            Request {
                route,
                kind: EventKind::FileRequest,
                id,
                stream: false,
                answer: true,
                paste: None,
            },
            data,
        )
    }

    pub(crate) fn submit_clipboard_fallback(
        &self,
        host: &Services,
        route: Weak<Dispatcher>,
        id: u32,
        op: &str,
        paste: Option<(u32, bool, Option<u64>, u64)>,
    ) -> napi_ohos::Result<()> {
        self.submit(
            host,
            Request {
                route,
                kind: EventKind::Clipboard,
                id,
                stream: false,
                answer: paste.is_none(),
                paste,
            },
            SystemCommand {
                op: op.into(),
                max_bytes: paste.and_then(|(_, _, max, _)| max).map(|m| m as f64),
                ..Default::default()
            },
        )
    }

    fn submit(
        &self,
        host: &Services,
        request: Request<Dispatcher>,
        data: SystemCommand,
    ) -> napi_ohos::Result<()> {
        let key = match self
            .next
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
        {
            Ok(previous) => previous + 1,
            Err(_) => {
                let message = "system request IDs exhausted";
                Self::deliver(
                    request,
                    SystemEvent::failure(message, "ERR_NSH_QUOTA_EXCEEDED"),
                );
                return Err(napi_ohos::Error::from_reason(message));
            }
        };
        let admitted = insert_request(
            &mut self.requests.lock().unwrap(),
            key,
            request.clone(),
            |d| d.live.load(Ordering::Acquire),
        );
        if let Err(message) = admitted {
            Self::deliver(
                request,
                SystemEvent::failure(&message, "ERR_NSH_QUOTA_EXCEEDED"),
            );
            return Err(napi_ohos::Error::from_reason(message));
        }
        if let Err(error) = host.send_host_command(HostCommand {
            kind: "system".into(),
            open_id: Some(key),
            system: Some(data),
            ..Default::default()
        }) {
            self.complete(
                key,
                SystemEvent::failure(&error.to_string(), "ERR_NSH_HOST_ERROR"),
            );
            return Err(error);
        }
        Ok(())
    }

    pub(crate) fn complete(&self, key: u32, value: SystemEvent) {
        let request = {
            let mut requests = self.requests.lock().unwrap();
            let Some(request) = requests.get(&key).cloned() else {
                return;
            };
            if !request.stream || value.phase.as_deref() == Some("end") {
                requests.remove(&key);
            }
            request
        };
        Self::deliver(request, value);
    }

    fn deliver(request: Request<Dispatcher>, value: SystemEvent) {
        let Some(route) = request
            .route
            .upgrade()
            .filter(|d| d.live.load(Ordering::Acquire))
        else {
            return;
        };
        if let Some((node_id, report_only, _paste_max_size, expected_edit_gen)) = request.paste {
            let text = value.text.unwrap_or_default();
            let html = value.html.filter(|s| !s.is_empty());
            let files = value.files.filter(|s| !s.is_empty());
            let image = value.uri.filter(|s| !s.is_empty());
            let image_too_large = value
                .too_large
                .filter(|v| v.is_finite() && *v > 0.0)
                .map(|v| v.round() as u64);
            if !text.is_empty()
                || html.is_some()
                || files.is_some()
                || image.is_some()
                || image_too_large.is_some()
            {
                route.send(Message::InputPaste {
                    id: node_id,
                    text,
                    html,
                    files,
                    image,
                    image_too_large,
                    report_only,
                    expected_edit_gen,
                });
            }
            return;
        }
        let payload = if request.kind == EventKind::AuthSession {
            auth_session_payload(request.id, value)
        } else if request.kind == EventKind::Share {
            if let Some(message) = value.error {
                nsh_core::share::ShareError::failed(message).payload(request.id)
            } else if value.phase.as_deref() == Some("shared") {
                nsh_core::share::ShareOutcome::shared(value.text).payload(request.id)
            } else {
                nsh_core::share::ShareOutcome::dismissed().payload(request.id)
            }
        } else if request.kind == EventKind::FileRequest {
            match value.error {
                Some(error) => EventPayload::file_failed(request.id, error),
                None => EventPayload::file_answer(
                    request.id,
                    parse_file_request_paths(value.files.as_deref()),
                ),
            }
        } else if request.kind == EventKind::Clipboard {
            if let Some(error) = value.error.as_deref() {
                log::warn!("OHOS clipboard request failed: {error}");
            }
            clipboard_payload(request.id, value)
        } else if request.kind == EventKind::OpenAppSettings {
            EventPayload::Map(vec![
                ("id", PayloadValue::Number(f64::from(request.id))),
                (
                    "opened",
                    PayloadValue::Bool(value.error.is_none() && value.opened.unwrap_or(false)),
                ),
            ])
        } else {
            if let Some(error) = value.error.as_deref() {
                route.send(Message::Error(format!("system service: {error}")));
            }
            let Some(payload) = request.map_payload(value) else {
                return;
            };
            payload
        };
        route.send(Message::HostEvent(request.kind, payload));
    }

    pub(crate) fn retire(&self, host: &Services, route: &Weak<Dispatcher>) {
        let retired: Vec<u32> = {
            let mut requests = self.requests.lock().unwrap();
            let keys = requests
                .iter()
                .filter_map(|(&key, r)| Weak::ptr_eq(&r.route, route).then_some(key))
                .collect::<Vec<_>>();
            for key in &keys {
                requests.remove(key);
            }
            keys
        };
        for key in retired {
            let _ = host.send_host_command(HostCommand {
                kind: "system-cancel".into(),
                open_id: Some(key),
                ..Default::default()
            });
        }
    }
}

/// Typed SDK request; fields cross N-API directly without a JSON envelope.
#[cfg_attr(target_env = "ohos", napi_derive_ohos::napi(object))]
#[derive(Clone, Debug, Default)]
pub(crate) struct SystemCommand {
    pub op: String,
    pub paths: Option<Vec<String>>,
    pub text: Option<String>,
    pub uri: Option<String>,
    pub name: Option<String>,
    pub title: Option<String>,
    pub body: Option<String>,
    pub silent: Option<bool>,
    pub count: Option<u32>,
    pub max_bytes: Option<f64>,
    pub ask: Option<bool>,
    pub lang: Option<String>,
    pub interim: Option<bool>,
    pub ephemeral: Option<bool>,
}

impl SystemCommand {
    #[cfg(target_env = "ohos")]
    fn notification_listener(command: nsh_core::notifications::NotificationCommand) -> Self {
        debug_assert!(matches!(
            command,
            nsh_core::notifications::NotificationCommand::Listen
        ));
        Self {
            op: "notification-listen".into(),
            ..Default::default()
        }
    }
}

/// Typed completion shared by clipboard, notification and system services.
#[cfg_attr(target_env = "ohos", napi_derive_ohos::napi(object))]
#[derive(Clone, Debug, Default)]
pub(crate) struct SystemEvent {
    pub permission: Option<String>,
    pub shown: Option<bool>,
    pub opened: Option<bool>,
    pub pressed: Option<String>,
    pub has: Option<bool>,
    pub denied: Option<bool>,
    pub text: Option<String>,
    pub html: Option<String>,
    pub uri: Option<String>,
    pub width: Option<f64>,
    pub height: Option<f64>,
    pub too_large: Option<f64>,
    pub files: Option<String>,
    pub error: Option<String>,
    pub code: Option<String>,
    pub phase: Option<String>,
    #[cfg_attr(target_env = "ohos", napi(js_name = "final"))]
    pub is_final: Option<bool>,
}

impl SystemEvent {
    #[cfg(target_env = "ohos")]
    fn failure(message: &str, code: &str) -> Self {
        Self {
            error: Some(message.into()),
            code: Some(code.into()),
            phase: Some("end".into()),
            ..Default::default()
        }
    }

    fn append_fields(self, fields: &mut Vec<(&'static str, PayloadValue)>) {
        for (name, value) in [
            ("permission", self.permission),
            ("pressed", self.pressed),
            ("text", self.text),
            ("html", self.html),
            ("uri", self.uri),
            ("files", self.files),
            ("error", self.error),
            ("code", self.code),
            ("phase", self.phase),
        ] {
            if let Some(value) = value {
                fields.push((name, PayloadValue::Str(value)));
            }
        }
        for (name, value) in [
            ("shown", self.shown),
            ("has", self.has),
            ("denied", self.denied),
            ("opened", self.opened),
        ] {
            if let Some(value) = value {
                fields.push((name, PayloadValue::Bool(value)));
            }
        }
        for (name, value) in [
            ("width", self.width),
            ("height", self.height),
            ("tooLarge", self.too_large),
        ] {
            if let Some(value) = value {
                fields.push((name, PayloadValue::Number(value)));
            }
        }
    }
}

#[cfg(unix)]
#[cfg_attr(target_env = "ohos", napi_derive_ohos::napi(object))]
pub(crate) struct PickedDescriptor {
    pub fd: i32,
    pub name: String,
}

#[cfg(unix)]
pub(crate) struct PickedHandle {
    descriptor: std::os::fd::OwnedFd,
    name: String,
}

#[cfg(unix)]
impl PickedDescriptor {
    pub(crate) fn retain(self) -> std::io::Result<PickedHandle> {
        if self.fd < 0 {
            return Err(std::io::Error::other("invalid picked descriptor"));
        }
        // SAFETY: the caller keeps `fd` alive until this synchronous call returns.
        let descriptor =
            unsafe { std::os::fd::BorrowedFd::borrow_raw(self.fd) }.try_clone_to_owned()?;
        Ok(PickedHandle {
            descriptor,
            name: self.name,
        })
    }
}

#[cfg(unix)]
const MAX_PICKED_IMAGE_BYTES: u64 = 64 << 20;

#[cfg(unix)]
const PICKED_READ_CHUNK: usize = 64 * 1024;

#[cfg(unix)]
fn read_bounded_file<R: std::io::Read>(
    reader: &mut R,
    name: String,
    limit: u64,
    modified: Option<std::time::SystemTime>,
    mut known_regular_size: impl FnMut(&R) -> Option<u64>,
) -> Option<nsh_core::picker::PickedFile> {
    if let Some(len) = known_regular_size(reader)
        && len > limit
    {
        log::warn!("OHOS picked {name} is too large ({len} > {limit} bytes)");
        let mut picked = nsh_core::picker::PickedFile::too_large(name, "", len);
        picked.last_modified = modified;
        return Some(picked);
    }

    let mut chunk = [0u8; PICKED_READ_CHUNK];
    let mut total: u64 = 0;
    let mut bytes: Option<Vec<u8>> = Some(Vec::new());
    let mut sniffed_mime: Option<&'static str> = None;

    loop {
        let n = match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => n,
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(err) => {
                log::warn!("OHOS picked {name} read failed: {err}");
                return None;
            }
        };
        let next_total = total.saturating_add(n as u64);
        if let Some(buf) = bytes.as_mut() {
            if next_total <= limit {
                buf.extend_from_slice(&chunk[..n]);
                total = next_total;
                continue;
            }
            sniffed_mime = if !buf.is_empty() {
                nsh_core::picker::sniff(buf).or_else(|| nsh_core::picker::sniff(&chunk[..n]))
            } else {
                nsh_core::picker::sniff(&chunk[..n])
            };
            bytes = None;
            total = next_total;
            if let Some(len) = known_regular_size(reader)
                && len > limit
            {
                total = len.max(total);
                break;
            }
        } else {
            total = next_total;
        }
    }

    let mut picked = match bytes {
        Some(data) => {
            let mut picked = nsh_core::picker::PickedFile::default();
            picked.name = name;
            picked.data = data.into();
            picked
        }
        None => nsh_core::picker::PickedFile::too_large(name, sniffed_mime.unwrap_or(""), total),
    };
    picked.last_modified = modified;
    Some(picked)
}

#[cfg(unix)]
fn read_picked_images_with(
    handles: Vec<PickedHandle>,
    request: &nsh_core::picker::PickRequest,
    mut prepare: impl FnMut(
        Vec<u8>,
        &str,
        &nsh_core::picker::PickRequest,
    ) -> Option<nsh_core::picker::PickedFile>,
) -> Vec<nsh_core::picker::PickedFile> {
    use std::{fs::File, io::Read as _};
    let limit = request.limit.clamp(1, nsh_core::picker::MAX_PICKED) as usize;
    let raw = handles
        .into_iter()
        .take(limit)
        .filter_map(|handle| {
            let file = File::from(handle.descriptor);
            let metadata = file.metadata().ok();
            if metadata
                .as_ref()
                .is_some_and(|m| m.is_file() && m.len() > MAX_PICKED_IMAGE_BYTES)
            {
                log::warn!(
                    "OHOS picked image {} is left out: larger than {MAX_PICKED_IMAGE_BYTES} bytes",
                    handle.name
                );
                return None;
            }
            let modified = metadata.and_then(|m| m.modified().ok());
            let mut bytes = Vec::new();
            if file
                .take(MAX_PICKED_IMAGE_BYTES + 1)
                .read_to_end(&mut bytes)
                .is_err()
                || bytes.len() as u64 > MAX_PICKED_IMAGE_BYTES
            {
                return None;
            }
            let mut picked = prepare(bytes, &handle.name, request)?;
            picked.last_modified = modified;
            Some(picked)
        })
        .collect();
    nsh_core::picker::finish(request, raw)
}

#[cfg(unix)]
fn read_picked_files(
    handles: Vec<PickedHandle>,
    request: &nsh_core::picker::FilePickRequest,
) -> Vec<nsh_core::picker::PickedFile> {
    use std::fs::File;
    let max_count = request.limit.clamp(1, nsh_core::picker::MAX_PICKED) as usize;
    let size_limit = request.size_limit();
    let raw = handles
        .into_iter()
        .take(max_count)
        .filter_map(|handle| {
            let mut file = File::from(handle.descriptor);
            let metadata = file.metadata().ok();
            let modified = metadata.as_ref().and_then(|m| m.modified().ok());
            let initial_regular_len = metadata.as_ref().filter(|m| m.is_file()).map(|m| m.len());
            let mut first_check = true;
            read_bounded_file(&mut file, handle.name, size_limit, modified, |file| {
                if first_check {
                    first_check = false;
                    initial_regular_len
                } else {
                    file.metadata()
                        .ok()
                        .filter(|m| m.is_file())
                        .map(|m| m.len())
                }
            })
        })
        .collect();
    nsh_core::picker::finish_files(request, raw)
}

#[cfg(unix)]
pub(crate) fn read_picked(
    handles: Vec<PickedHandle>,
    request: &nsh_core::picker::Pick,
) -> Vec<nsh_core::picker::PickedFile> {
    match request {
        nsh_core::picker::Pick::Images(r) => {
            #[cfg(target_env = "ohos")]
            {
                read_picked_images_with(handles, r, crate::extensions::prepare_image_owned)
            }
            #[cfg(not(target_env = "ohos"))]
            {
                read_picked_images_with(handles, r, |bytes, name, _| {
                    Some(nsh_core::picker::PickedFile::new(name, "", bytes.into()))
                })
            }
        }
        nsh_core::picker::Pick::Files(r) => read_picked_files(handles, r),
    }
}

/// Formats `FileFilter` entries for HarmonyOS `DocumentSelectOptions.fileSuffixFilters`
/// (`"Description|.ext1,.ext2"`).
pub(crate) fn format_ohos_open_filters(
    filters: &[nsh_core::file_dialog::FileFilter],
) -> Vec<String> {
    let mut out = Vec::new();
    for filter in filters {
        let suffixes: Vec<String> = filter
            .extensions
            .iter()
            .filter(|ext| !ext.is_empty())
            .map(|ext| format!(".{ext}"))
            .collect();
        if suffixes.is_empty() {
            continue;
        }
        let joined = suffixes.join(",");
        let name = filter.name.replace('|', " ");
        let trimmed = name.trim();
        if trimmed.is_empty() {
            out.push(joined);
        } else {
            out.push(format!("{trimmed}|{joined}"));
        }
    }
    out
}

/// Formats `FileFilter` entries for HarmonyOS `DocumentSaveOptions.fileSuffixChoices`
/// (one `"Description|.ext"` option per suffix).
pub(crate) fn format_ohos_save_choices(
    filters: &[nsh_core::file_dialog::FileFilter],
) -> Vec<String> {
    let mut out = Vec::new();
    for filter in filters {
        let name = filter.name.replace('|', " ");
        let trimmed = name.trim();
        for ext in &filter.extensions {
            if ext.is_empty() {
                continue;
            }
            if trimmed.is_empty() {
                out.push(format!(".{ext}"));
            } else {
                out.push(format!("{trimmed}|.{ext}"));
            }
        }
    }
    out
}

/// Validates that a file path passed to `open_path`, `reveal_path`, or `move_to_trash`
/// is absolute (or already a `file://` URI) and exists on disk when given as a filesystem path.
pub(crate) fn validate_existing_file_request_path(path: &std::path::Path) -> Result<(), String> {
    let raw = path.to_string_lossy();
    if raw.is_empty() {
        return Err("path must not be empty".into());
    }
    if raw.contains("://") {
        return Ok(());
    }
    if !path.is_absolute() {
        return Err(format!("path must be absolute: {}", path.display()));
    }
    if !path.exists() {
        return Err(format!("path does not exist: {}", path.display()));
    }
    Ok(())
}

/// Parses the JSON array of chosen paths returned by `NativeSystem.ets` file dialogs.
pub(crate) fn parse_file_request_paths(files: Option<&str>) -> Vec<std::path::PathBuf> {
    let Some(raw) = files.filter(|s| !s.is_empty()) else {
        return Vec::new();
    };
    let Ok(list) = serde_json::from_str::<Vec<String>>(raw) else {
        return Vec::new();
    };
    list.into_iter()
        .filter(|path| !path.is_empty())
        .map(std::path::PathBuf::from)
        .collect()
}

/// Converts a `NativeAuthSession` reply into the `EventKind::AuthSession` payload.
pub(crate) fn auth_session_payload(id: u32, value: SystemEvent) -> nsh_core::EventPayload {
    use nsh_core::auth_session::{AuthSessionError, AuthSessionOutcome};
    if let Some(message) = value.error.filter(|s| !s.is_empty()) {
        let mut error = AuthSessionError::failed(message);
        error.code = value.code.filter(|s| !s.is_empty());
        return error.payload(id);
    }
    if value.phase.as_deref() == Some("completed")
        && let Some(url) = value.uri.filter(|s| !s.is_empty())
    {
        return AuthSessionOutcome::Completed(url).payload(id);
    }
    AuthSessionOutcome::Cancelled.payload(id)
}

/// Converts a `NativeClipboard` reply into the `EventKind::Clipboard` payload.
/// User dismissal or read errors resolve as `denied: true` instead of surfacing
/// as an uncaught host error overlay.
pub(crate) fn clipboard_payload(id: u32, value: SystemEvent) -> nsh_core::EventPayload {
    if value.error.is_some() || value.denied == Some(true) {
        return EventPayload::Map(vec![
            ("id", PayloadValue::Number(f64::from(id))),
            ("has", PayloadValue::Bool(false)),
            ("denied", PayloadValue::Bool(true)),
        ]);
    }
    let mut fields = vec![("id", PayloadValue::Number(f64::from(id)))];
    value.append_fields(&mut fields);
    EventPayload::Map(fields)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answer_less_notification_listener_delivers_presses_and_skips_empty_replies() {
        let request = Request::<()> {
            route: std::sync::Weak::new(),
            kind: EventKind::Notification,
            id: 0,
            stream: true,
            answer: false,
            paste: None,
        };
        let payload = request.map_payload(SystemEvent {
            pressed: Some("reminder".into()),
            ..Default::default()
        });
        let Some(EventPayload::Map(fields)) = payload else {
            panic!("notification press was dropped");
        };
        assert!(matches!(
            fields.as_slice(),
            [("pressed", PayloadValue::Str(name))] if name == "reminder"
        ));
        assert!(request.map_payload(SystemEvent::default()).is_none());
        let payload = request.map_payload(SystemEvent {
            error: Some("failed".into()),
            ..Default::default()
        });
        assert!(matches!(payload, Some(EventPayload::Map(_))));
    }

    #[cfg(unix)]
    #[test]
    fn retained_descriptor_survives_sdk_close_and_keeps_original_name() {
        use std::{fs::File, os::fd::AsRawFd};
        let path = std::env::temp_dir().join(format!("nsh-picked-{}", std::process::id()));
        std::fs::write(&path, b"picked bytes").unwrap();
        let source = File::open(&path).unwrap();
        let handle = PickedDescriptor {
            fd: source.as_raw_fd(),
            name: "photo.jpg".into(),
        }
        .retain()
        .unwrap();
        drop(source);
        std::fs::remove_file(path).unwrap();
        let files = read_picked(
            vec![handle],
            &nsh_core::picker::Pick::Images(Default::default()),
        );
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].name, "photo.jpg");
        assert_eq!(&*files[0].data, b"picked bytes");
    }

    #[cfg(unix)]
    #[test]
    fn picked_files_preserve_arbitrary_names_and_infer_mime() {
        use std::{fs::File, os::fd::AsRawFd};
        let path = std::env::temp_dir().join(format!("nsh-picked-doc-{}", std::process::id()));
        std::fs::write(&path, b"{\"ok\":true}").unwrap();
        let source = File::open(&path).unwrap();
        let handle = PickedDescriptor {
            fd: source.as_raw_fd(),
            name: "folder/config.json".into(),
        }
        .retain()
        .unwrap();
        drop(source);
        std::fs::remove_file(path).unwrap();
        let files = read_picked(
            vec![handle],
            &nsh_core::picker::Pick::Files(Default::default()),
        );
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].name, "config.json");
        assert_eq!(files[0].mime, "application/json");
        assert_eq!(&*files[0].data, b"{\"ok\":true}");
    }

    #[test]
    fn three_hundred_notification_listens_replace_the_previous_route() {
        let owner = std::sync::Arc::new(());
        let mut requests = BTreeMap::new();
        for key in 1..=300 {
            insert_request(
                &mut requests,
                key,
                Request {
                    route: std::sync::Arc::downgrade(&owner),
                    kind: EventKind::Notification,
                    id: 0,
                    stream: true,
                    answer: false,
                    paste: None,
                },
                |_| true,
            )
            .unwrap();
            assert_eq!(requests.len(), 1);
            assert!(requests.contains_key(&key));
        }
    }

    #[test]
    fn replacing_listener_preserves_other_pending_requests() {
        let owner = std::sync::Arc::new(());
        let mut requests = BTreeMap::new();
        for (key, kind, stream) in [
            (1, EventKind::AuthSession, true),
            (2, EventKind::Notification, false),
            (3, EventKind::Notification, true),
            (4, EventKind::Notification, true),
        ] {
            insert_request(
                &mut requests,
                key,
                Request {
                    route: std::sync::Arc::downgrade(&owner),
                    kind,
                    id: key,
                    stream,
                    answer: true,
                    paste: None,
                },
                |_| true,
            )
            .unwrap();
        }
        assert_eq!(requests.keys().copied().collect::<Vec<_>>(), vec![1, 2, 4]);
    }

    #[test]
    fn file_dialog_filters_and_paths_format_for_ohos_picker() {
        use nsh_core::file_dialog::FileFilter;
        let filters = [
            FileFilter::new("Markdown", ["md", "markdown"]),
            FileFilter::new("", ["txt"]),
        ];
        assert_eq!(
            format_ohos_open_filters(&filters),
            vec!["Markdown|.md,.markdown", ".txt"]
        );
        assert_eq!(
            format_ohos_save_choices(&filters),
            vec!["Markdown|.md", "Markdown|.markdown", ".txt"]
        );
        assert_eq!(
            parse_file_request_paths(Some(r#"["/storage/docs/a.md","/storage/docs/b.txt"]"#)),
            vec![
                std::path::PathBuf::from("/storage/docs/a.md"),
                std::path::PathBuf::from("/storage/docs/b.txt"),
            ]
        );
        assert!(parse_file_request_paths(Some("[]")).is_empty());
        assert!(parse_file_request_paths(None).is_empty());
    }

    #[test]
    fn file_request_path_validation_rejects_relative_and_missing_paths() {
        let temp_dir = std::env::temp_dir();
        assert!(validate_existing_file_request_path(&temp_dir).is_ok());
        assert!(
            validate_existing_file_request_path(std::path::Path::new(
                "file://docs/storage/Users/currentUser/Documents/a.txt"
            ))
            .is_ok()
        );
        assert!(validate_existing_file_request_path(std::path::Path::new("note.md")).is_err());
        assert!(
            validate_existing_file_request_path(std::path::Path::new(
                "/nonexistent-nsh-path-12345/note.md"
            ))
            .is_err()
        );
    }

    #[test]
    fn auth_session_payload_maps_completed_cancelled_and_error_outcomes() {
        let completed = auth_session_payload(
            7,
            SystemEvent {
                phase: Some("completed".into()),
                uri: Some("myapp://callback?code=abc".into()),
                ..Default::default()
            },
        );
        let EventPayload::Map(fields) = completed else {
            panic!("expected Map payload");
        };
        assert!(fields.contains(&("id", PayloadValue::Number(7.0))));
        assert!(fields.contains(&("url", PayloadValue::Str("myapp://callback?code=abc".into()))));

        let cancelled = auth_session_payload(
            8,
            SystemEvent {
                phase: Some("cancelled".into()),
                ..Default::default()
            },
        );
        let EventPayload::Map(fields) = cancelled else {
            panic!("expected Map payload");
        };
        assert!(fields.contains(&("id", PayloadValue::Number(8.0))));
        assert!(fields.contains(&("action", PayloadValue::Str("cancelled".into()))));

        let failed = auth_session_payload(
            9,
            SystemEvent {
                error: Some("dialog failed".into()),
                ..Default::default()
            },
        );
        let EventPayload::Map(fields) = failed else {
            panic!("expected Map payload");
        };
        assert!(fields.contains(&("id", PayloadValue::Number(9.0))));
        assert!(fields.contains(&("error", PayloadValue::Str("dialog failed".into()))));
        assert!(!fields.iter().any(|(key, _)| *key == "code"));

        let unavailable = auth_session_payload(
            10,
            SystemEvent {
                error: Some("no browser".into()),
                code: Some("ERR_FEATURE_UNAVAILABLE_ON_PLATFORM".into()),
                ..Default::default()
            },
        );
        let EventPayload::Map(fields) = unavailable else {
            panic!("expected Map payload");
        };
        assert!(fields.contains(&("id", PayloadValue::Number(10.0))));
        assert!(fields.contains(&("error", PayloadValue::Str("no browser".into()))));
        assert!(fields.contains(&(
            "code",
            PayloadValue::Str("ERR_FEATURE_UNAVAILABLE_ON_PLATFORM".into())
        )));
    }

    #[test]
    fn clipboard_payload_maps_cancelled_and_error_replies_to_denied() {
        let cancelled = clipboard_payload(
            11,
            SystemEvent {
                has: Some(false),
                denied: Some(true),
                ..Default::default()
            },
        );
        let EventPayload::Map(fields) = cancelled else {
            panic!("expected Map payload");
        };
        assert_eq!(
            fields,
            vec![
                ("id", PayloadValue::Number(11.0)),
                ("has", PayloadValue::Bool(false)),
                ("denied", PayloadValue::Bool(true)),
            ]
        );

        let failed = clipboard_payload(
            12,
            SystemEvent {
                error: Some("paste cancelled".into()),
                ..Default::default()
            },
        );
        let EventPayload::Map(fields) = failed else {
            panic!("expected Map payload");
        };
        assert_eq!(
            fields,
            vec![
                ("id", PayloadValue::Number(12.0)),
                ("has", PayloadValue::Bool(false)),
                ("denied", PayloadValue::Bool(true)),
            ]
        );

        let ok = clipboard_payload(
            13,
            SystemEvent {
                has: Some(true),
                text: Some("hello".into()),
                ..Default::default()
            },
        );
        let EventPayload::Map(fields) = ok else {
            panic!("expected Map payload");
        };
        assert!(fields.contains(&("id", PayloadValue::Number(13.0))));
        assert!(fields.contains(&("has", PayloadValue::Bool(true))));
        assert!(fields.contains(&("text", PayloadValue::Str("hello".into()))));
        assert!(!fields.iter().any(|(key, _)| *key == "denied"));
    }

    #[cfg(unix)]
    #[test]
    fn file_picker_enforces_size_limit_returns_too_large_and_preserves_order() {
        use std::{fs::File, os::fd::AsRawFd};
        let dir = std::env::temp_dir().join(format!("nsh-picked-limits-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let make_handle = |file_name: &str, len: u64, sparse: bool| -> PickedHandle {
            let path = dir.join(file_name);
            let file = File::create(&path).unwrap();
            if sparse {
                file.set_len(len).unwrap();
            } else {
                std::fs::write(&path, vec![b'a'; len as usize]).unwrap();
            }
            let opened = File::open(&path).unwrap();
            PickedDescriptor {
                fd: opened.as_raw_fd(),
                name: file_name.to_owned(),
            }
            .retain()
            .unwrap()
        };

        // Limit = 16 bytes: 15 -> Read, 16 -> Read, 17 -> TooLarge, 512 MiB + 1 sparse -> TooLarge.
        let mut req = nsh_core::picker::FilePickRequest::default();
        req.limit = 10;
        req.max_size = Some(16);
        let handles = vec![
            make_handle("below.txt", 15, false),
            make_handle("exact.txt", 16, false),
            make_handle("over.json", 17, false),
            make_handle(
                "huge.bin",
                nsh_core::picker::MAX_PICKED_FILE_BYTES + 1,
                true,
            ),
        ];
        let picked = read_picked(handles, &nsh_core::picker::Pick::Files(req));
        assert_eq!(picked.len(), 4);
        assert_eq!(picked[0].name, "below.txt");
        assert_eq!(picked[0].status, nsh_core::picker::PickedStatus::Read);
        assert_eq!(picked[0].data.len(), 15);
        assert_eq!(picked[1].name, "exact.txt");
        assert_eq!(picked[1].status, nsh_core::picker::PickedStatus::Read);
        assert_eq!(picked[1].data.len(), 16);
        assert_eq!(picked[2].name, "over.json");
        assert_eq!(picked[2].status, nsh_core::picker::PickedStatus::TooLarge);
        assert!(picked[2].data.is_empty());
        assert_eq!(picked[2].len, 17);
        assert_eq!(picked[2].mime, "application/json");
        assert!(picked[2].last_modified.is_some());
        assert_eq!(picked[3].name, "huge.bin");
        assert_eq!(picked[3].status, nsh_core::picker::PickedStatus::TooLarge);
        assert!(picked[3].data.is_empty());
        assert_eq!(picked[3].len, nsh_core::picker::MAX_PICKED_FILE_BYTES + 1);

        // Default limit (512 MiB): sparse 512 MiB + 1 file is returned as TooLarge, never dropped.
        let huge = make_handle(
            "sparse-over-hard-limit.pdf",
            nsh_core::picker::MAX_PICKED_FILE_BYTES + 1,
            true,
        );
        let picked_default = read_picked(
            vec![huge],
            &nsh_core::picker::Pick::Files(Default::default()),
        );
        assert_eq!(picked_default.len(), 1);
        assert_eq!(
            picked_default[0].status,
            nsh_core::picker::PickedStatus::TooLarge
        );
        assert_eq!(picked_default[0].mime, "application/pdf");
        assert_eq!(
            picked_default[0].len,
            nsh_core::picker::MAX_PICKED_FILE_BYTES + 1
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn known_oversized_file_performs_zero_reads_and_unknown_stream_bounds_buffer() {
        struct CountingReader {
            data: Vec<u8>,
            pos: usize,
            reads: usize,
        }
        impl std::io::Read for CountingReader {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                self.reads += 1;
                let n = buf.len().min(self.data.len() - self.pos);
                buf[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
                self.pos += n;
                Ok(n)
            }
        }

        // Known-size regular file larger than limit must execute 0 read() calls.
        let mut known = CountingReader {
            data: vec![b'x'; 200],
            pos: 0,
            reads: 0,
        };
        let oversized = read_bounded_file(&mut known, "big.bin".into(), 64, None, |_| Some(200))
            .expect("oversized entry");
        assert_eq!(known.reads, 0);
        assert_eq!(oversized.status, nsh_core::picker::PickedStatus::TooLarge);
        assert_eq!(oversized.len, 200);
        assert!(oversized.data.is_empty());

        // Unknown-size stream (e.g. pipe) exceeding limit releases bytes and counts exact length.
        let mut png_header = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
        png_header.resize(256, 0);
        let mut stream = CountingReader {
            data: png_header,
            pos: 0,
            reads: 0,
        };
        let streamed_over =
            read_bounded_file(&mut stream, "unnamed".into(), 64, None, |_| None).unwrap();
        assert!(stream.reads > 0);
        assert_eq!(
            streamed_over.status,
            nsh_core::picker::PickedStatus::TooLarge
        );
        assert_eq!(streamed_over.len, 256);
        assert_eq!(streamed_over.mime, "image/png");
        assert!(streamed_over.data.is_empty());

        // Zero size limit accepts empty files and marks 1-byte files tooLarge.
        let mut empty = CountingReader {
            data: Vec::new(),
            pos: 0,
            reads: 0,
        };
        let ok_empty =
            read_bounded_file(&mut empty, "empty.txt".into(), 0, None, |_| None).unwrap();
        assert_eq!(ok_empty.status, nsh_core::picker::PickedStatus::Read);
        assert!(ok_empty.data.is_empty());
        let mut one_byte = CountingReader {
            data: vec![b'1'],
            pos: 0,
            reads: 0,
        };
        let over_zero =
            read_bounded_file(&mut one_byte, "one.txt".into(), 0, None, |_| None).unwrap();
        assert_eq!(over_zero.status, nsh_core::picker::PickedStatus::TooLarge);
        assert_eq!(over_zero.len, 1);
    }

    #[cfg(unix)]
    #[test]
    fn image_picker_passes_request_to_preparer_and_preserves_modified_and_order() {
        use nsh_core::geometry::{DevicePixels, Percentage};
        use std::{fs::File, os::fd::AsRawFd};
        let dir = std::env::temp_dir().join(format!("nsh-picked-img-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let make = |name: &str, bytes: &[u8]| {
            let p = dir.join(name);
            std::fs::write(&p, bytes).unwrap();
            let f = File::open(&p).unwrap();
            PickedDescriptor {
                fd: f.as_raw_fd(),
                name: name.to_owned(),
            }
            .retain()
            .unwrap()
        };
        let mut req = nsh_core::picker::PickRequest::default();
        req.limit = 4;
        req.max_dimension = Some(DevicePixels(4));
        req.quality = Some(Percentage(0.8));
        let handles = vec![make("a.png", b"png-a"), make("b.heic", b"heic-b")];
        let mut seen = Vec::new();
        let out = read_picked_images_with(handles, &req, |bytes, name, passed| {
            seen.push((
                name.to_owned(),
                bytes.len(),
                passed.max_dimension,
                passed.quality,
            ));
            let mut f = nsh_core::picker::PickedFile::new(
                nsh_core::picker::file_name(name, "image/jpeg"),
                "image/jpeg",
                b"reencoded".to_vec().into(),
            );
            f.size = nsh_core::Size::new(DevicePixels(4), DevicePixels(1));
            Some(f)
        });
        assert_eq!(seen.len(), 2);
        assert_eq!(
            seen[0],
            (
                "a.png".into(),
                5,
                Some(DevicePixels(4)),
                Some(Percentage(0.8))
            )
        );
        assert_eq!(
            seen[1],
            (
                "b.heic".into(),
                6,
                Some(DevicePixels(4)),
                Some(Percentage(0.8))
            )
        );
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].name, "a.jpg");
        assert_eq!(out[1].name, "b.jpg");
        assert_eq!(out[0].size.width.0, 4);
        assert_eq!(out[0].size.height.0, 1);
        assert!(out[0].last_modified.is_some());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
