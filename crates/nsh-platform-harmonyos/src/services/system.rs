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
use nsh_core::EventKind;
#[cfg(target_env = "ohos")]
use nsh_core::{EventPayload, PayloadValue};

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
}
#[cfg(target_env = "ohos")]
#[derive(Default)]
pub(crate) struct System {
    next: AtomicU32,
    requests: Mutex<BTreeMap<u32, Request<Dispatcher>>>,
}
#[cfg(target_env = "ohos")]
impl System {
    pub(crate) fn command(
        &self,
        host: &Services,
        route: Weak<Dispatcher>,
        command: Command,
    ) -> napi_ohos::Result<()> {
        use nsh_core::{notifications::NotificationCommand as N, speech::SpeechCommand as S};
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
            Command::SetClipboard(text) => (
                EventKind::Clipboard,
                0,
                false,
                SystemCommand {
                    op: "clipboard-write".into(),
                    text: Some(text),
                    ..Default::default()
                },
            ),
            Command::SetClipboardImage(uri) => (
                EventKind::Clipboard,
                0,
                false,
                SystemCommand {
                    op: "clipboard-write-image".into(),
                    uri: Some(uri),
                    ..Default::default()
                },
            ),
            Command::ReadClipboard { id, has_only } => (
                EventKind::Clipboard,
                id,
                false,
                SystemCommand {
                    op: "clipboard-read".into(),
                    has_only: Some(has_only),
                    ..Default::default()
                },
            ),
            Command::ReadClipboardImage { id, has_only } => (
                EventKind::Clipboard,
                id,
                false,
                SystemCommand {
                    op: "clipboard-image".into(),
                    has_only: Some(has_only),
                    ..Default::default()
                },
            ),
            Command::ReadClipboardFiles { id } => (
                EventKind::Clipboard,
                id,
                false,
                SystemCommand {
                    op: "clipboard-files".into(),
                    ..Default::default()
                },
            ),
            _ => return Err(napi_ohos::Error::from_reason("unsupported system command")),
        };
        let answer = !matches!(
            data.op.as_str(),
            "clipboard-write"
                | "clipboard-write-image"
                | "notification-close"
                | "notification-badge"
                | "notification-listen"
        );
        let request = Request {
            route,
            kind,
            id,
            stream,
            answer,
        };
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
        let payload = if request.kind == EventKind::Speech {
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
            let mut fields = Vec::new();
            if request.answer {
                fields.push(("id", PayloadValue::Number(f64::from(request.id))));
            }
            value.append_fields(&mut fields);
            if fields.is_empty() {
                return;
            }
            EventPayload::Map(fields)
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

impl<R> Clone for Request<R> {
    fn clone(&self) -> Self {
        Self {
            route: self.route.clone(),
            kind: self.kind,
            id: self.id,
            stream: self.stream,
            answer: self.answer,
        }
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

/// Typed SDK request; fields cross N-API directly without a JSON envelope.
#[cfg_attr(target_env = "ohos", napi_derive_ohos::napi(object))]
#[derive(Clone, Debug, Default)]
pub(crate) struct SystemCommand {
    pub op: String,
    pub text: Option<String>,
    pub uri: Option<String>,
    pub has_only: Option<bool>,
    pub name: Option<String>,
    pub title: Option<String>,
    pub body: Option<String>,
    pub count: Option<u32>,
    pub ask: Option<bool>,
    pub lang: Option<String>,
    pub interim: Option<bool>,
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
#[cfg_attr(target_env = "ohos", napi_derive_ohos::napi(object))]
#[derive(Clone, Debug, Default)]
pub(crate) struct SystemEvent {
    pub permission: Option<String>,
    pub shown: Option<bool>,
    pub pressed: Option<String>,
    pub has: Option<bool>,
    pub text: Option<String>,
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
    #[cfg(target_env = "ohos")]
    fn append_fields(self, fields: &mut Vec<(&'static str, PayloadValue)>) {
        for (name, value) in [
            ("permission", self.permission),
            ("pressed", self.pressed),
            ("text", self.text),
            ("uri", self.uri),
            ("files", self.files),
            ("error", self.error),
            ("code", self.code),
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
        // SAFETY: the SDK keeps its opened file alive until this synchronous call returns.
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

#[cfg(test)]
mod tests {
    use super::*;
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
            let command = SystemCommand::notification_listener(
                nsh_core::notifications::NotificationCommand::Listen,
            );
            assert_eq!(command.op, "notification-listen");
            insert_request(
                &mut requests,
                key,
                Request {
                    route: std::sync::Arc::downgrade(&owner),
                    kind: EventKind::Notification,
                    id: 0,
                    stream: true,
                    answer: false,
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
                },
                |_| true,
            )
            .unwrap();
        }
        assert_eq!(requests.keys().copied().collect::<Vec<_>>(), vec![1, 2, 4]);
    }
}
