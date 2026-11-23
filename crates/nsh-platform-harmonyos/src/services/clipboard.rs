//! Native system clipboard via `ohos-pasteboard-binding`, `ohos-udmf-binding`,
//! and `ohos-image-native-binding`.
//!
//! Writes (`SetClipboard`, `SetClipboardItem`, `SetClipboardImage`), `has_only`
//! probes, and direct reads run on a serialized per-session worker thread without
//! crossing to ArkTS. Only when OHOS requires the security-component `PasteButton`
//! prompt (permission denied or empty read while `has_data()` is true) does a read
//! fall back to `NativeClipboard.ets`.

use std::sync::{Arc, Mutex, Weak, atomic::Ordering, mpsc};

use base64::Engine as _;
use nsh_core::{EventKind, EventPayload, PayloadValue};
use ohos_image_native_binding::{
    DecodingOptions, ImagePacker, ImageSource, ImageString, PackingOptions, PixelMap,
};
use ohos_pasteboard_binding::Pasteboard;
use ohos_udmf_binding::{UdmfData, UdmfRecord, UdsHtml, UdsPixelMap, UdsPlainText};

use crate::app::{
    runtime::{Dispatcher, Message},
    session::Services,
};

const MAX_IMAGE_BYTES: usize = 48 * 1024 * 1024;
const UTD_PLAIN_TEXT: &str = "general.plain-text";
const UTD_PIXEL_MAP: &str = "openharmony.pixel-map";
const UTD_FILE_URI: &str = "general.file-uri";
const MIME_TEXT_PLAIN: &str = "text/plain";
const MIME_PIXEL_MAP: &str = "pixelMap";
const MIME_TEXT_URI: &str = "text/uri";

enum ClipboardOp {
    WriteText {
        text: String,
        html: Option<String>,
    },
    WriteImage {
        uri: String,
    },
    ReadText {
        host: Arc<Services>,
        route: Weak<Dispatcher>,
        id: u32,
        has_only: bool,
    },
    ReadImage {
        host: Arc<Services>,
        route: Weak<Dispatcher>,
        id: u32,
        has_only: bool,
    },
    ReadFiles {
        host: Arc<Services>,
        route: Weak<Dispatcher>,
        id: u32,
    },
    PasteToInput {
        host: Arc<Services>,
        route: Weak<Dispatcher>,
        node_id: u32,
        report_only: bool,
    },
}

/// Per-session serialized clipboard worker. Lazily spawned on first clipboard
/// command so surfaces that never touch the clipboard create zero threads.
#[derive(Default)]
pub(crate) struct Clipboard {
    worker: Mutex<Option<mpsc::Sender<ClipboardOp>>>,
}

impl Clipboard {
    fn send(&self, op: ClipboardOp) -> napi_ohos::Result<()> {
        let mut slot = self.worker.lock().unwrap();
        let op = match slot.as_ref() {
            Some(tx) => match tx.send(op) {
                Ok(()) => return Ok(()),
                Err(mpsc::SendError(op)) => op,
            },
            None => op,
        };
        // Either not started yet or previous worker exited; spawn a fresh worker.
        let (tx, rx) = mpsc::channel::<ClipboardOp>();
        std::thread::Builder::new()
            .name("nsh-clipboard".into())
            .spawn(move || {
                while let Ok(op) = rx.recv() {
                    run_op(op);
                }
            })
            .map_err(|error| napi_ohos::Error::from_reason(error.to_string()))?;
        let send_ok = tx.send(op).is_ok();
        *slot = Some(tx);
        if !send_ok {
            return Err(napi_ohos::Error::from_reason(
                "clipboard worker terminated unexpectedly",
            ));
        }
        Ok(())
    }

    pub(crate) fn write_text(&self, text: String, html: Option<String>) -> napi_ohos::Result<()> {
        self.send(ClipboardOp::WriteText { text, html })
    }

    pub(crate) fn write_image(&self, uri: String) -> napi_ohos::Result<()> {
        self.send(ClipboardOp::WriteImage { uri })
    }

    pub(crate) fn read_text(
        &self,
        host: Arc<Services>,
        route: Weak<Dispatcher>,
        id: u32,
        has_only: bool,
    ) -> napi_ohos::Result<()> {
        self.send(ClipboardOp::ReadText {
            host,
            route,
            id,
            has_only,
        })
    }

    pub(crate) fn read_image(
        &self,
        host: Arc<Services>,
        route: Weak<Dispatcher>,
        id: u32,
        has_only: bool,
    ) -> napi_ohos::Result<()> {
        self.send(ClipboardOp::ReadImage {
            host,
            route,
            id,
            has_only,
        })
    }

    pub(crate) fn read_files(
        &self,
        host: Arc<Services>,
        route: Weak<Dispatcher>,
        id: u32,
    ) -> napi_ohos::Result<()> {
        self.send(ClipboardOp::ReadFiles { host, route, id })
    }

    pub(crate) fn paste_to_input(
        &self,
        host: Arc<Services>,
        route: Weak<Dispatcher>,
        node_id: u32,
        report_only: bool,
    ) -> napi_ohos::Result<()> {
        self.send(ClipboardOp::PasteToInput {
            host,
            route,
            node_id,
            report_only,
        })
    }
}

/// Synchronously writes plain text (and optional HTML) to the system pasteboard.
pub(crate) fn write_text_sync(text: &str, html: Option<&str>) -> Result<(), String> {
    let board = Pasteboard::try_new().map_err(|e| e.to_string())?;
    let clean_text = strip_nul(text);
    let mut data = UdmfData::try_new().map_err(|e| e.to_string())?;
    let record = UdmfRecord::try_new().map_err(|e| e.to_string())?;
    let uds_text = UdsPlainText::new();
    uds_text
        .set_content(&clean_text)
        .map_err(|e| e.to_string())?;
    record.add(uds_text.into()).map_err(|e| e.to_string())?;
    if let Some(html) = html {
        let clean_html = strip_nul(html);
        let uds_html = UdsHtml::new();
        uds_html.set_html(&clean_html).map_err(|e| e.to_string())?;
        uds_html
            .set_primary_content(&clean_text)
            .map_err(|e| e.to_string())?;
        record.add(uds_html.into()).map_err(|e| e.to_string())?;
    }
    data.add_record(&record).map_err(|e| e.to_string())?;
    board.set_data(&data).map_err(|e| e.to_string())
}

fn strip_nul(value: &str) -> std::borrow::Cow<'_, str> {
    if value.contains('\0') {
        std::borrow::Cow::Owned(value.replace('\0', ""))
    } else {
        std::borrow::Cow::Borrowed(value)
    }
}

fn write_image_sync(uri: &str) -> Result<(), String> {
    let comma = uri.find(',').ok_or("invalid image data URI")?;
    let header = &uri[..comma];
    if !header.starts_with("data:image/") || !header.ends_with(";base64") {
        return Err("invalid image data URI".into());
    }
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(&uri[comma + 1..])
        .map_err(|e| format!("invalid base64 image data: {e}"))?;
    if decoded.is_empty() {
        return Err("empty image data URI".into());
    }
    let source = ImageSource::create_from_data(&decoded).map_err(|e| format!("{e:?}"))?;
    let mut opts = DecodingOptions::new().map_err(|e| format!("{e:?}"))?;
    opts.set_pixel_format(ohos_image_native_binding::types::PixelFormat::Rgba8888)
        .map_err(|e| format!("{e:?}"))?;
    let pixel_map = source
        .create_pixelmap(&mut opts)
        .map_err(|e| format!("{e:?}"))?;

    let uds = UdsPixelMap::try_new().map_err(|e| e.to_string())?;
    uds.set_pixel_map(&pixel_map.handle())
        .map_err(|e| e.to_string())?;
    let record = UdmfRecord::try_new().map_err(|e| e.to_string())?;
    record.add(uds.into()).map_err(|e| e.to_string())?;
    let mut data = UdmfData::try_new().map_err(|e| e.to_string())?;
    data.add_record(&record).map_err(|e| e.to_string())?;
    let board = Pasteboard::try_new().map_err(|e| e.to_string())?;
    board.set_data(&data).map_err(|e| e.to_string())
}

fn route_live(route: &Weak<Dispatcher>) -> Option<Arc<Dispatcher>> {
    route.upgrade().filter(|d| d.live.load(Ordering::Acquire))
}

fn has_type_any(board: &Pasteboard, utd: &str, mime: &str) -> bool {
    board.has_type_str(utd).unwrap_or(false)
        || board.has_type_str(mime).unwrap_or(false)
        || board
            .mime_types()
            .is_ok_and(|types| types.iter().any(|t| t == utd || t == mime))
}

fn read_board_data(board: &Pasteboard) -> Result<UdmfData, ()> {
    let Ok(data) = board.data() else {
        return Err(());
    };
    if data.count() <= 0 && board.has_data() {
        return Err(());
    }
    Ok(data)
}

enum PackedPng {
    Data {
        uri: String,
        width: u32,
        height: u32,
    },
    TooLarge(usize),
}

fn pack_pixelmap_png(pixel_map: &PixelMap) -> Result<PackedPng, String> {
    let info = pixel_map.image_info().map_err(|e| format!("{e:?}"))?;
    let width = info.width().map_err(|e| format!("{e:?}"))?;
    let height = info.height().map_err(|e| format!("{e:?}"))?;
    if width == 0 || height == 0 {
        return Err("empty clipboard pixelmap".into());
    }
    let mut options = PackingOptions::new().map_err(|e| format!("{e:?}"))?;
    let mut mime = ImageString::from_str("image/png").map_err(|e| format!("{e:?}"))?;
    options
        .set_mime_type(&mut mime)
        .map_err(|e| format!("{e:?}"))?;
    options.set_quality(100).map_err(|e| format!("{e:?}"))?;
    let mut packer = ImagePacker::new().map_err(|e| format!("{e:?}"))?;

    let pixel_estimate = (width as usize)
        .saturating_mul(height as usize)
        .saturating_mul(4)
        .saturating_add(4096);
    let max_buf = MAX_IMAGE_BYTES + 1;
    let mut capacity = pixel_estimate.clamp(64 * 1024, max_buf);
    for attempt in 0..3 {
        let mut out = Vec::new();
        if out.try_reserve_exact(capacity).is_err() {
            return Ok(PackedPng::TooLarge(capacity));
        }
        out.resize(capacity, 0);
        match packer.pack_to_data_from_pixelmap(&mut options, pixel_map, &mut out) {
            Ok(written) => {
                if written > MAX_IMAGE_BYTES {
                    return Ok(PackedPng::TooLarge(written));
                }
                out.truncate(written);
                let encoded = base64::engine::general_purpose::STANDARD.encode(&out);
                return Ok(PackedPng::Data {
                    uri: format!("data:image/png;base64,{encoded}"),
                    width,
                    height,
                });
            }
            Err(_) if attempt < 2 && capacity < max_buf => {
                capacity = capacity.saturating_mul(2).min(max_buf);
            }
            Err(error) => {
                if capacity >= max_buf {
                    return Ok(PackedPng::TooLarge(max_buf));
                }
                return Err(format!("{error:?}"));
            }
        }
    }
    Ok(PackedPng::TooLarge(max_buf))
}

fn extract_plain_text(data: &mut UdmfData) -> Option<String> {
    if let Ok(primary) = data.primary_plain_text()
        && let Ok(text) = primary.get_content()
        && !text.is_empty()
    {
        return Some(text);
    }
    let Ok(records) = data.records() else {
        return None;
    };
    for record in records {
        if let Ok(plain) = record.plain_text()
            && let Ok(text) = plain.get_content()
            && !text.is_empty()
        {
            return Some(text);
        }
        if let Ok(html) = record.html()
            && let Ok(text) = html.get_primary_content()
            && !text.is_empty()
        {
            return Some(text);
        }
    }
    None
}

fn extract_html(data: &mut UdmfData) -> Option<String> {
    if let Ok(primary) = data.primary_html()
        && let Ok(html) = primary.get_html()
        && !html.is_empty()
    {
        return Some(html);
    }
    let Ok(records) = data.records() else {
        return None;
    };
    for record in records {
        if let Ok(uds) = record.html()
            && let Ok(html) = uds.get_html()
            && !html.is_empty()
        {
            return Some(html);
        }
    }
    None
}

fn extract_file_uris(data: &mut UdmfData) -> Vec<String> {
    let mut files = Vec::new();
    let Ok(records) = data.records() else {
        return files;
    };
    for record in records {
        if let Ok(uds) = record.file_uri()
            && let Ok(uri) = uds.file_uri()
            && uri.starts_with("file:")
        {
            files.push(uri);
        }
    }
    files
}

fn extract_first_pixelmap(data: &mut UdmfData) -> Option<PixelMap> {
    let Ok(records) = data.records() else {
        return None;
    };
    for record in records {
        if let Ok(uds) = record.pixel_map()
            && let Ok(pixel_map) = uds.to_pixel_map()
        {
            return Some(pixel_map);
        }
    }
    None
}

fn send_clipboard_event(route: &Dispatcher, fields: Vec<(&'static str, PayloadValue)>) {
    route.send(Message::HostEvent(
        EventKind::Clipboard,
        EventPayload::Map(fields),
    ));
}

fn run_op(op: ClipboardOp) {
    match op {
        ClipboardOp::WriteText { text, html } => {
            if let Err(error) = write_text_sync(&text, html.as_deref()) {
                log::warn!("OHOS native clipboard write failed: {error}");
            }
        }
        ClipboardOp::WriteImage { uri } => {
            if let Err(error) = write_image_sync(&uri) {
                log::warn!("OHOS native clipboard image write failed: {error}");
            }
        }
        ClipboardOp::ReadText {
            host,
            route,
            id,
            has_only,
        } => {
            let Some(dispatcher) = route_live(&route) else {
                return;
            };
            let Ok(board) = Pasteboard::try_new() else {
                let _ =
                    host.system
                        .submit_clipboard_fallback(&host, route, id, "clipboard-read", None);
                return;
            };
            let has = has_type_any(&board, UTD_PLAIN_TEXT, MIME_TEXT_PLAIN);
            if has_only || !has {
                send_clipboard_event(
                    &dispatcher,
                    vec![
                        ("id", PayloadValue::Number(f64::from(id))),
                        ("has", PayloadValue::Bool(has)),
                    ],
                );
                return;
            }
            match read_board_data(&board) {
                Ok(mut data) => {
                    let has_plain = data.has_type(UTD_PLAIN_TEXT).unwrap_or(false)
                        || data.has_type(MIME_TEXT_PLAIN).unwrap_or(false);
                    let text = extract_plain_text(&mut data).unwrap_or_default();
                    send_clipboard_event(
                        &dispatcher,
                        vec![
                            ("id", PayloadValue::Number(f64::from(id))),
                            ("has", PayloadValue::Bool(has_plain || !text.is_empty())),
                            ("text", PayloadValue::Str(text)),
                        ],
                    );
                }
                Err(()) => {
                    let _ = host.system.submit_clipboard_fallback(
                        &host,
                        route,
                        id,
                        "clipboard-read",
                        None,
                    );
                }
            }
        }
        ClipboardOp::ReadImage {
            host,
            route,
            id,
            has_only,
        } => {
            let Some(dispatcher) = route_live(&route) else {
                return;
            };
            let Ok(board) = Pasteboard::try_new() else {
                let _ = host.system.submit_clipboard_fallback(
                    &host,
                    route,
                    id,
                    "clipboard-image",
                    None,
                );
                return;
            };
            let has = has_type_any(&board, UTD_PIXEL_MAP, MIME_PIXEL_MAP);
            if has_only || !has {
                send_clipboard_event(
                    &dispatcher,
                    vec![
                        ("id", PayloadValue::Number(f64::from(id))),
                        ("has", PayloadValue::Bool(has)),
                    ],
                );
                return;
            }
            match read_board_data(&board) {
                Ok(mut data) => {
                    let Some(pixel_map) = extract_first_pixelmap(&mut data) else {
                        send_clipboard_event(
                            &dispatcher,
                            vec![
                                ("id", PayloadValue::Number(f64::from(id))),
                                ("has", PayloadValue::Bool(false)),
                            ],
                        );
                        return;
                    };
                    match pack_pixelmap_png(&pixel_map) {
                        Ok(PackedPng::Data { uri, width, height }) => {
                            send_clipboard_event(
                                &dispatcher,
                                vec![
                                    ("id", PayloadValue::Number(f64::from(id))),
                                    ("has", PayloadValue::Bool(true)),
                                    ("uri", PayloadValue::Str(uri)),
                                    ("width", PayloadValue::Number(f64::from(width))),
                                    ("height", PayloadValue::Number(f64::from(height))),
                                ],
                            );
                        }
                        Ok(PackedPng::TooLarge(bytes)) => {
                            send_clipboard_event(
                                &dispatcher,
                                vec![
                                    ("id", PayloadValue::Number(f64::from(id))),
                                    ("has", PayloadValue::Bool(true)),
                                    ("tooLarge", PayloadValue::Number(bytes as f64)),
                                ],
                            );
                        }
                        Err(error) => {
                            log::warn!("OHOS native clipboard image pack failed: {error}");
                            send_clipboard_event(
                                &dispatcher,
                                vec![
                                    ("id", PayloadValue::Number(f64::from(id))),
                                    ("has", PayloadValue::Bool(false)),
                                ],
                            );
                        }
                    }
                }
                Err(()) => {
                    let _ = host.system.submit_clipboard_fallback(
                        &host,
                        route,
                        id,
                        "clipboard-image",
                        None,
                    );
                }
            }
        }
        ClipboardOp::ReadFiles { host, route, id } => {
            let Some(dispatcher) = route_live(&route) else {
                return;
            };
            let Ok(board) = Pasteboard::try_new() else {
                let _ = host.system.submit_clipboard_fallback(
                    &host,
                    route,
                    id,
                    "clipboard-files",
                    None,
                );
                return;
            };
            if !has_type_any(&board, UTD_FILE_URI, MIME_TEXT_URI) {
                send_clipboard_event(
                    &dispatcher,
                    vec![
                        ("id", PayloadValue::Number(f64::from(id))),
                        ("has", PayloadValue::Bool(false)),
                    ],
                );
                return;
            }
            match read_board_data(&board) {
                Ok(mut data) => {
                    let files = extract_file_uris(&mut data);
                    send_clipboard_event(
                        &dispatcher,
                        vec![
                            ("id", PayloadValue::Number(f64::from(id))),
                            ("has", PayloadValue::Bool(!files.is_empty())),
                            ("files", PayloadValue::Str(files.join("\n"))),
                        ],
                    );
                }
                Err(()) => {
                    let _ = host.system.submit_clipboard_fallback(
                        &host,
                        route,
                        id,
                        "clipboard-files",
                        None,
                    );
                }
            }
        }
        ClipboardOp::PasteToInput {
            host,
            route,
            node_id,
            report_only,
        } => {
            let Some(dispatcher) = route_live(&route) else {
                return;
            };
            let Ok(board) = Pasteboard::try_new() else {
                let _ = host.system.submit_clipboard_fallback(
                    &host,
                    route,
                    0,
                    "clipboard-paste",
                    Some((node_id, report_only)),
                );
                return;
            };
            if !board.has_data() {
                return;
            }
            match read_board_data(&board) {
                Ok(mut data) => {
                    let text = extract_plain_text(&mut data).unwrap_or_default();
                    let html = extract_html(&mut data);
                    let uris = extract_file_uris(&mut data);
                    let files = (!uris.is_empty()).then(|| uris.join("\n"));
                    let image = extract_first_pixelmap(&mut data).and_then(|pm| {
                        match pack_pixelmap_png(&pm) {
                            Ok(PackedPng::Data { uri, .. }) => Some(uri),
                            _ => None,
                        }
                    });
                    if !text.is_empty() || html.is_some() || files.is_some() || image.is_some() {
                        dispatcher.send(Message::InputPaste {
                            id: node_id,
                            text,
                            html,
                            files,
                            image,
                            report_only,
                        });
                    }
                }
                Err(()) => {
                    let _ = host.system.submit_clipboard_fallback(
                        &host,
                        route,
                        0,
                        "clipboard-paste",
                        Some((node_id, report_only)),
                    );
                }
            }
        }
    }
}
