//! Pure system-request queue, file dialog filter/path formatting, picked-file
//! reading, and AuthSession/Notification payload mapping.
use std::collections::BTreeMap;

use nsh_core::{EventKind, EventPayload, PayloadValue};

pub(crate) struct Request<R> {
    pub(crate) route: std::sync::Weak<R>,
    pub(crate) kind: EventKind,
    pub(crate) id: u32,
    pub(crate) stream: bool,
    pub(crate) answer: bool,
    pub(crate) paste: Option<(u32, bool)>,
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

/// Pure host-testable mirror of `SystemEvent` fields used for payload mapping.
#[derive(Clone, Debug, Default)]
pub(crate) struct SystemReply {
    pub(crate) permission: Option<String>,
    pub(crate) shown: Option<bool>,
    pub(crate) pressed: Option<String>,
    pub(crate) has: Option<bool>,
    pub(crate) text: Option<String>,
    pub(crate) html: Option<String>,
    pub(crate) uri: Option<String>,
    pub(crate) width: Option<f64>,
    pub(crate) height: Option<f64>,
    pub(crate) too_large: Option<f64>,
    pub(crate) files: Option<String>,
    pub(crate) error: Option<String>,
    pub(crate) code: Option<String>,
    pub(crate) phase: Option<String>,
    pub(crate) is_final: Option<bool>,
}

impl SystemReply {
    pub(crate) fn append_fields(self, fields: &mut Vec<(&'static str, PayloadValue)>) {
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

impl<R> Request<R> {
    pub(crate) fn map_payload(&self, value: SystemReply) -> Option<EventPayload> {
        let mut fields = Vec::new();
        if self.answer {
            fields.push(("id", PayloadValue::Number(f64::from(self.id))));
        }
        // Answer-less notification listeners still deliver press replies.
        value.append_fields(&mut fields);
        (!fields.is_empty()).then_some(EventPayload::Map(fields))
    }
}

pub(crate) fn insert_request<R>(
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

#[cfg(unix)]
pub(crate) struct PickedHandle {
    descriptor: std::os::fd::OwnedFd,
    name: String,
}

#[cfg(unix)]
impl PickedHandle {
    pub(crate) fn retain_raw(fd: i32, name: String) -> std::io::Result<Self> {
        if fd < 0 {
            return Err(std::io::Error::other("invalid picked descriptor"));
        }
        // SAFETY: the caller keeps `fd` alive until this synchronous call returns.
        let descriptor = unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) }.try_clone_to_owned()?;
        Ok(Self { descriptor, name })
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
pub(crate) fn auth_session_payload(id: u32, value: SystemReply) -> nsh_core::EventPayload {
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
        let payload = request.map_payload(SystemReply {
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
        assert!(request.map_payload(SystemReply::default()).is_none());
        let payload = request.map_payload(SystemReply {
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
        let handle = PickedHandle::retain_raw(source.as_raw_fd(), "photo.jpg".into()).unwrap();
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
        let handle =
            PickedHandle::retain_raw(source.as_raw_fd(), "folder/config.json".into()).unwrap();
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
            SystemReply {
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
            SystemReply {
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
            SystemReply {
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
            SystemReply {
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
