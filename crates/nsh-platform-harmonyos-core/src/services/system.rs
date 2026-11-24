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
    paste: Option<(u32, bool)>,
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
    pub(crate) fn command(
        &self,
        host: &std::sync::Arc<Services>,
        route: Weak<Dispatcher>,
        command: Command,
    ) -> napi_ohos::Result<()> {
        use nsh_core::{notifications::NotificationCommand as N, speech::SpeechCommand as S};
        match command {
            Command::SetClipboardItem(item) => {
                return self.clipboard.write_text(item.text, item.html);
            }
            Command::SetClipboard(text) => {
                return self.clipboard.write_text(text, None);
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
            Command::Speech(command) => {
                let (id, data) = match command {
                    S::Start { id, request } => (
                        id,
                        SystemCommand {
                            op: "speech-start".into(),
                            lang: request.lang,
                            interim: Some(request.interim),
                            ..Default::default()
                        },
                    ),
                    S::Stop { id } | S::Abort { id } => {
                        let key = self.requests.lock().unwrap().iter().find_map(|(&key, r)| {
                            (r.kind == EventKind::Speech
                                && r.id == id
                                && Weak::ptr_eq(&r.route, &route))
                            .then_some(key)
                        });
                        if let Some(key) = key {
                            let abort = matches!(command, S::Abort { .. });
                            if abort {
                                self.requests.lock().unwrap().remove(&key);
                            }
                            host.send_host_command(HostCommand {
                                kind: "system".into(),
                                open_id: Some(key),
                                system: Some(SystemCommand {
                                    op: if abort { "speech-abort" } else { "speech-stop" }.into(),
                                    ..Default::default()
                                }),
                                ..Default::default()
                            })?;
                        }
                        return Ok(());
                    }
                    _ => return Err(napi_ohos::Error::from_reason("unsupported speech command")),
                };
                (EventKind::Speech, id, true, data)
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

    pub(crate) fn paste_to_input(
        &self,
        host: &std::sync::Arc<Services>,
        route: Weak<Dispatcher>,
        node_id: u32,
        report_only: bool,
    ) -> napi_ohos::Result<()> {
        self.clipboard
            .paste_to_input(host.clone(), route, node_id, report_only)
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
        paste: Option<(u32, bool)>,
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
        if let Some((node_id, report_only)) = request.paste {
            let text = value.text.unwrap_or_default();
            let html = value.html.filter(|s| !s.is_empty());
            let files = value.files.filter(|s| !s.is_empty());
            let image = value.uri.filter(|s| !s.is_empty());
            if !text.is_empty() || html.is_some() || files.is_some() || image.is_some() {
                route.send(Message::InputPaste {
                    id: node_id,
                    text,
                    html,
                    files,
                    image,
                    report_only,
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
        } else if request.kind == EventKind::Speech {
            use nsh_core::speech::{SpeechError, SpeechEvent as E};
            let event = match value.phase.as_deref() {
                Some("start") => E::Start,
                Some("result") => E::Result {
                    text: value.text.clone().unwrap_or_default(),
                    is_final: value.is_final.unwrap_or(false),
                },
                Some("end") => E::End {
                    error: value.error.as_deref().and_then(SpeechError::from_name),
                },
                _ => return,
            };
            EventPayload::Speech {
                id: request.id,
                event,
            }
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

/// Typed completion shared by clipboard, notification and speech services.
#[cfg_attr(target_env = "ohos", napi_derive_ohos::napi(object))]
#[derive(Clone, Debug, Default)]
pub(crate) struct SystemEvent {
    pub permission: Option<String>,
    pub shown: Option<bool>,
    pub pressed: Option<String>,
    pub has: Option<bool>,
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
        for (name, value) in [("shown", self.shown), ("has", self.has)] {
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
pub(crate) fn read_picked(
    handles: Vec<PickedHandle>,
    request: &nsh_core::picker::Pick,
) -> Vec<nsh_core::picker::PickedFile> {
    use std::{fs::File, io::Read as _};
    let (limit, max_bytes) = match request {
        nsh_core::picker::Pick::Images(r) => (r.limit, 64 << 20),
        nsh_core::picker::Pick::Files(r) => (r.limit, nsh_core::picker::MAX_PICKED_FILE_BYTES),
    };
    let raw = handles
        .into_iter()
        .take(limit.clamp(1, nsh_core::picker::MAX_PICKED) as usize)
        .filter_map(|handle| {
            let file = File::from(handle.descriptor);
            let metadata = file.metadata().ok();
            if metadata.as_ref().is_some_and(|m| m.len() > max_bytes) {
                log::warn!(
                    "OHOS picked {} is left out: larger than {max_bytes} bytes",
                    handle.name
                );
                return None;
            }
            let modified = metadata.and_then(|m| m.modified().ok());
            let mut bytes = Vec::new();
            if file.take(max_bytes + 1).read_to_end(&mut bytes).is_err()
                || bytes.len() as u64 > max_bytes
            {
                return None;
            }
            let mut picked = match request {
                nsh_core::picker::Pick::Images(_) => {
                    nsh_core::picker::PickedFile::new(handle.name, "", bytes.into())
                }
                nsh_core::picker::Pick::Files(_) => {
                    let mut picked = nsh_core::picker::PickedFile::default();
                    picked.name = handle.name;
                    picked.data = bytes.into();
                    picked
                }
            };
            picked.last_modified = modified;
            Some(picked)
        })
        .collect();
    request.finish(raw)
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
            (1, EventKind::Speech, true),
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
}
