//! System UI requests are scoped to an engine generation. ArkTS owns their
//! controllers on the window thread; late completions hold only weak routes.
use std::{
    collections::BTreeMap,
    sync::{
        Mutex, Weak,
        atomic::{AtomicU32, Ordering},
    },
};

use nsh_core::{Command, EventKind, EventPayload};

use crate::app::{
    runtime::{Dispatcher, Message},
    session::{HostCommand, Services},
};
pub(crate) use crate::services::system_spec::{PickedHandle, read_picked};
use crate::services::system_spec::{
    Request, SystemReply, auth_session_payload, format_ohos_open_filters, format_ohos_save_choices,
    insert_request, parse_file_request_paths, validate_existing_file_request_path,
};

#[derive(Default)]
pub(crate) struct System {
    next: AtomicU32,
    requests: Mutex<BTreeMap<u32, Request<Dispatcher>>>,
    clipboard: crate::services::clipboard::Clipboard,
}

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
        match request {
            F::OpenPath(path) => {
                if let Err(err) = validate_existing_file_request_path(&path) {
                    return fail(err);
                }
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
                    SystemCommand {
                        op: "file-open-path".into(),
                        uri: Some(path.to_string_lossy().into_owned()),
                        ..Default::default()
                    },
                )
            }
            F::OpenDialog(dialog) => {
                let mode = match dialog.kind {
                    nsh_core::file_dialog::OpenDialogKind::Folders => "folders",
                    nsh_core::file_dialog::OpenDialogKind::FilesAndFolders => "both",
                    _ => "files",
                };
                let filters = format_ohos_open_filters(&dialog.filters);
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
                    SystemCommand {
                        op: "file-open-dialog".into(),
                        name: Some(mode.into()),
                        count: Some(if dialog.multiple { 500 } else { 1 }),
                        paths: (!filters.is_empty()).then_some(filters),
                        uri: dialog.directory.map(|p| p.to_string_lossy().into_owned()),
                        title: dialog.title,
                        ..Default::default()
                    },
                )
            }
            F::SaveDialog(dialog) => {
                let choices = format_ohos_save_choices(&dialog.filters);
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
                    SystemCommand {
                        op: "file-save-dialog".into(),
                        name: dialog.file_name,
                        paths: (!choices.is_empty()).then_some(choices),
                        uri: dialog.directory.map(|p| p.to_string_lossy().into_owned()),
                        title: dialog.title,
                        ..Default::default()
                    },
                )
            }
            F::RevealPath(path) => {
                if let Err(err) = validate_existing_file_request_path(&path) {
                    return fail(err);
                }
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
                    SystemCommand {
                        op: "file-reveal-path".into(),
                        uri: Some(path.to_string_lossy().into_owned()),
                        ..Default::default()
                    },
                )
            }
            F::MoveToTrash(path) => {
                if let Err(err) = validate_existing_file_request_path(&path) {
                    return fail(err);
                }
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
                    SystemCommand {
                        op: "file-move-to-trash".into(),
                        uri: Some(path.to_string_lossy().into_owned()),
                        ..Default::default()
                    },
                )
            }
            _ => fail("unsupported file request on HarmonyOS".into()),
        }
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
        let reply = value.into_reply();
        if let Some((node_id, report_only)) = request.paste {
            let text = reply.text.unwrap_or_default();
            let html = reply.html.filter(|s| !s.is_empty());
            let files = reply.files.filter(|s| !s.is_empty());
            let image = reply.uri.filter(|s| !s.is_empty());
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
            auth_session_payload(request.id, reply)
        } else if request.kind == EventKind::Share {
            if let Some(message) = reply.error {
                nsh_core::share::ShareError::failed(message).payload(request.id)
            } else if reply.phase.as_deref() == Some("shared") {
                nsh_core::share::ShareOutcome::shared(reply.text).payload(request.id)
            } else {
                nsh_core::share::ShareOutcome::dismissed().payload(request.id)
            }
        } else if request.kind == EventKind::FileRequest {
            match reply.error {
                Some(error) => EventPayload::file_failed(request.id, error),
                None => EventPayload::file_answer(
                    request.id,
                    parse_file_request_paths(reply.files.as_deref()),
                ),
            }
        } else if request.kind == EventKind::Speech {
            use nsh_core::speech::{SpeechError, SpeechEvent as E};
            let event = match reply.phase.as_deref() {
                Some("start") => E::Start,
                Some("result") => E::Result {
                    text: reply.text.clone().unwrap_or_default(),
                    is_final: reply.is_final.unwrap_or(false),
                },
                Some("end") => E::End {
                    error: reply.error.as_deref().and_then(SpeechError::from_name),
                },
                _ => return,
            };
            EventPayload::Speech {
                id: request.id,
                event,
            }
        } else {
            if let Some(error) = reply.error.as_deref() {
                route.send(Message::Error(format!("system service: {error}")));
            }
            let Some(payload) = request.map_payload(reply) else {
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
#[napi_derive_ohos::napi(object)]
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
#[napi_derive_ohos::napi(object)]
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
    #[napi(js_name = "final")]
    pub is_final: Option<bool>,
}

impl SystemEvent {
    fn failure(message: &str, code: &str) -> Self {
        Self {
            error: Some(message.into()),
            code: Some(code.into()),
            phase: Some("end".into()),
            ..Default::default()
        }
    }

    fn into_reply(self) -> SystemReply {
        SystemReply {
            permission: self.permission,
            shown: self.shown,
            pressed: self.pressed,
            has: self.has,
            text: self.text,
            html: self.html,
            uri: self.uri,
            width: self.width,
            height: self.height,
            too_large: self.too_large,
            files: self.files,
            error: self.error,
            code: self.code,
            phase: self.phase,
            is_final: self.is_final,
        }
    }
}

#[napi_derive_ohos::napi(object)]
pub(crate) struct PickedDescriptor {
    pub fd: i32,
    pub name: String,
}

impl PickedDescriptor {
    pub(crate) fn retain(self) -> std::io::Result<PickedHandle> {
        PickedHandle::retain_raw(self.fd, self.name)
    }
}
