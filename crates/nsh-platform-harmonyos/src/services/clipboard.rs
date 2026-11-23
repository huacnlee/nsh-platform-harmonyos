//! Native system clipboard via `ohos-pasteboard-binding`, `ohos-udmf-binding`,
//! and `ohos-image-native-binding`.
//!
//! Writes, `has_only` probes, and direct reads run on a serialized per-session
//! worker thread without crossing to ArkTS. Only when OHOS requires the
//! security-component `PasteButton` prompt does a read fall back to
//! `NativeClipboard.ets`.

use std::{
    borrow::Cow,
    sync::{Arc, Mutex, Weak, atomic::Ordering, mpsc},
};

use base64::Engine as _;
use nsh_core::{EventKind, EventPayload, PayloadValue};
use ohos_image_native_binding::{
    DecodingOptions, ImagePacker, ImageSource, ImageString, PackingOptions, PixelMap,
    types::PixelFormat,
};
use ohos_pasteboard_binding::Pasteboard;
use ohos_udmf_binding::{UdmfData, UdmfRecord, UdsHtml, UdsPixelMap, UdsPlainText};

use crate::app::{
    runtime::{Dispatcher, Message},
    session::Services,
};

const MAX_IMAGE_BYTES: usize = 48 * 1024 * 1024;
const UTD_PLAIN_TEXT: &str = "general.plain-text";
const UTD_HTML: &str = "general.html";
const UTD_PIXEL_MAP: &str = "openharmony.pixel-map";
const UTD_FILE_URI: &str = "general.file-uri";
const MIME_TEXT_PLAIN: &str = "text/plain";
const MIME_TEXT_HTML: &str = "text/html";
const MIME_PIXEL_MAP: &str = "pixelMap";
const MIME_TEXT_URI: &str = "text/uri";

type Job = Box<dyn FnOnce() + Send>;

/// Per-session serialized clipboard worker, lazily spawned on first command.
#[derive(Default)]
pub(crate) struct Clipboard {
    worker: Mutex<Option<mpsc::Sender<Job>>>,
}

impl Clipboard {
    fn enqueue(&self, job: impl FnOnce() + Send + 'static) -> napi_ohos::Result<()> {
        let mut slot = self.worker.lock().unwrap();
        let job: Job = Box::new(job);
        let job = match slot.as_ref() {
            Some(tx) => match tx.send(job) {
                Ok(()) => return Ok(()),
                Err(mpsc::SendError(job)) => job,
            },
            None => job,
        };
        let (tx, rx) = mpsc::channel::<Job>();
        std::thread::Builder::new()
            .name("nsh-clipboard".into())
            .spawn(move || {
                while let Ok(job) = rx.recv() {
                    job();
                }
            })
            .map_err(|e| napi_ohos::Error::from_reason(e.to_string()))?;
        let sent = tx.send(job).is_ok();
        *slot = Some(tx);
        if !sent {
            return Err(napi_ohos::Error::from_reason(
                "clipboard worker terminated unexpectedly",
            ));
        }
        Ok(())
    }

    pub(crate) fn write_text(&self, text: String, html: Option<String>) -> napi_ohos::Result<()> {
        self.enqueue(move || {
            if let Err(e) = write_text_sync(&text, html.as_deref()) {
                log::warn!("OHOS native clipboard write failed: {e}");
            }
        })
    }

    pub(crate) fn write_image(&self, uri: String) -> napi_ohos::Result<()> {
        self.enqueue(move || {
            if let Err(e) = write_image_sync(&uri) {
                log::warn!("OHOS native clipboard image write failed: {e}");
            }
        })
    }

    pub(crate) fn read_text(
        &self,
        host: Arc<Services>,
        route: Weak<Dispatcher>,
        id: u32,
        has_only: bool,
    ) -> napi_ohos::Result<()> {
        self.enqueue(move || {
            read_with_fallback(
                &host,
                route,
                id,
                "clipboard-read",
                None,
                |board, d| {
                    let has = has_type_any(board, UTD_PLAIN_TEXT, MIME_TEXT_PLAIN)
                        || has_type_any(board, UTD_HTML, MIME_TEXT_HTML);
                    if has_only || !has {
                        send_has(d, id, has);
                        return false;
                    }
                    true
                },
                |data, d| {
                    let has_plain = data.has_type(UTD_PLAIN_TEXT).unwrap_or(false)
                        || data.has_type(MIME_TEXT_PLAIN).unwrap_or(false);
                    let text = extract_plain_text(data).unwrap_or_default();
                    send_event(
                        d,
                        vec![
                            ("id", PayloadValue::Number(f64::from(id))),
                            ("has", PayloadValue::Bool(has_plain || !text.is_empty())),
                            ("text", PayloadValue::Str(text)),
                        ],
                    );
                },
            );
        })
    }

    pub(crate) fn read_image(
        &self,
        host: Arc<Services>,
        route: Weak<Dispatcher>,
        id: u32,
        has_only: bool,
    ) -> napi_ohos::Result<()> {
        self.enqueue(move || {
            read_with_fallback(
                &host,
                route,
                id,
                "clipboard-image",
                None,
                |board, d| {
                    let has = has_type_any(board, UTD_PIXEL_MAP, MIME_PIXEL_MAP);
                    if has_only || !has {
                        send_has(d, id, has);
                        return false;
                    }
                    true
                },
                |data, d| {
                    let Some(pm) = extract_first_pixelmap(data) else {
                        send_has(d, id, false);
                        return;
                    };
                    match pack_pixelmap_png(&pm) {
                        Ok(PackedPng::Data { uri, width, height }) => send_event(
                            d,
                            vec![
                                ("id", PayloadValue::Number(f64::from(id))),
                                ("has", PayloadValue::Bool(true)),
                                ("uri", PayloadValue::Str(uri)),
                                ("width", PayloadValue::Number(f64::from(width))),
                                ("height", PayloadValue::Number(f64::from(height))),
                            ],
                        ),
                        Ok(PackedPng::TooLarge(bytes)) => send_event(
                            d,
                            vec![
                                ("id", PayloadValue::Number(f64::from(id))),
                                ("has", PayloadValue::Bool(true)),
                                ("tooLarge", PayloadValue::Number(bytes as f64)),
                            ],
                        ),
                        Err(e) => {
                            log::warn!("OHOS native clipboard image pack failed: {e}");
                            send_has(d, id, false);
                        }
                    }
                },
            );
        })
    }

    pub(crate) fn read_files(
        &self,
        host: Arc<Services>,
        route: Weak<Dispatcher>,
        id: u32,
    ) -> napi_ohos::Result<()> {
        self.enqueue(move || {
            read_with_fallback(
                &host,
                route,
                id,
                "clipboard-files",
                None,
                |board, d| {
                    if !has_type_any(board, UTD_FILE_URI, MIME_TEXT_URI) {
                        send_has(d, id, false);
                        return false;
                    }
                    true
                },
                |data, d| {
                    let files = extract_file_uris(data);
                    send_event(
                        d,
                        vec![
                            ("id", PayloadValue::Number(f64::from(id))),
                            ("has", PayloadValue::Bool(!files.is_empty())),
                            ("files", PayloadValue::Str(files.join("\n"))),
                        ],
                    );
                },
            );
        })
    }

    pub(crate) fn paste_to_input(
        &self,
        host: Arc<Services>,
        route: Weak<Dispatcher>,
        node_id: u32,
        report_only: bool,
    ) -> napi_ohos::Result<()> {
        self.enqueue(move || {
            read_with_fallback(
                &host,
                route,
                0,
                "clipboard-paste",
                Some((node_id, report_only)),
                |board, _| board.has_data(),
                |data, d| {
                    let text = extract_plain_text(data).unwrap_or_default();
                    let html = extract_html(data);
                    let uris = extract_file_uris(data);
                    let files = (!uris.is_empty()).then(|| uris.join("\n"));
                    let image =
                        extract_first_pixelmap(data).and_then(|pm| match pack_pixelmap_png(&pm) {
                            Ok(PackedPng::Data { uri, .. }) => Some(uri),
                            _ => None,
                        });
                    if !text.is_empty() || html.is_some() || files.is_some() || image.is_some() {
                        d.send(Message::InputPaste {
                            id: node_id,
                            text,
                            html,
                            files,
                            image,
                            report_only,
                        });
                    }
                },
            );
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
        let uds_html = UdsHtml::new();
        uds_html
            .set_html(strip_nul(html))
            .map_err(|e| e.to_string())?;
        uds_html
            .set_primary_content(&clean_text)
            .map_err(|e| e.to_string())?;
        record.add(uds_html.into()).map_err(|e| e.to_string())?;
    }
    data.add_record(&record).map_err(|e| e.to_string())?;
    board.set_data(&data).map_err(|e| e.to_string())
}

fn write_image_sync(uri: &str) -> Result<(), String> {
    let (header, payload) = uri.split_once(',').ok_or("invalid image data URI")?;
    if !header.starts_with("data:image/") || !header.ends_with(";base64") {
        return Err("invalid image data URI".into());
    }
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(payload)
        .map_err(|e| format!("invalid base64 image data: {e}"))?;
    if decoded.is_empty() {
        return Err("empty image data URI".into());
    }
    let source = ImageSource::create_from_data(&decoded).map_err(|e| format!("{e:?}"))?;
    let mut opts = DecodingOptions::new().map_err(|e| format!("{e:?}"))?;
    opts.set_pixel_format(PixelFormat::Rgba8888)
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
    Pasteboard::try_new()
        .map_err(|e| e.to_string())?
        .set_data(&data)
        .map_err(|e| e.to_string())
}

fn read_with_fallback(
    host: &Arc<Services>,
    route: Weak<Dispatcher>,
    id: u32,
    fallback_op: &str,
    paste: Option<(u32, bool)>,
    precheck: impl FnOnce(&Pasteboard, &Dispatcher) -> bool,
    on_data: impl FnOnce(&mut UdmfData, &Dispatcher),
) {
    let Some(dispatcher) = route.upgrade().filter(|d| d.live.load(Ordering::Acquire)) else {
        return;
    };
    let Ok(board) = Pasteboard::try_new() else {
        let _ = host
            .system
            .submit_clipboard_fallback(host, route, id, fallback_op, paste);
        return;
    };
    if !precheck(&board, &dispatcher) {
        return;
    }
    match board.data() {
        Ok(mut data) if data.count() > 0 || !board.has_data() => on_data(&mut data, &dispatcher),
        _ => {
            let _ = host
                .system
                .submit_clipboard_fallback(host, route, id, fallback_op, paste);
        }
    }
}

fn strip_nul(value: &str) -> Cow<'_, str> {
    if value.contains('\0') {
        Cow::Owned(value.replace('\0', ""))
    } else {
        Cow::Borrowed(value)
    }
}

fn has_type_any(board: &Pasteboard, utd: &str, mime: &str) -> bool {
    board.has_type_str(utd).unwrap_or(false)
        || board.has_type_str(mime).unwrap_or(false)
        || board
            .mime_types()
            .is_ok_and(|types| types.iter().any(|t| t == utd || t == mime))
}

fn send_event(route: &Dispatcher, fields: Vec<(&'static str, PayloadValue)>) {
    route.send(Message::HostEvent(
        EventKind::Clipboard,
        EventPayload::Map(fields),
    ));
}

fn send_has(route: &Dispatcher, id: u32, has: bool) {
    send_event(
        route,
        vec![
            ("id", PayloadValue::Number(f64::from(id))),
            ("has", PayloadValue::Bool(has)),
        ],
    );
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

    let max_buf = MAX_IMAGE_BYTES + 1;
    let mut capacity = (width as usize)
        .saturating_mul(height as usize)
        .saturating_mul(4)
        .saturating_add(4096)
        .clamp(64 * 1024, max_buf);
    for attempt in 0..3 {
        let mut out = Vec::new();
        if out.try_reserve_exact(capacity).is_err() {
            return Ok(PackedPng::TooLarge(capacity));
        }
        out.resize(capacity, 0);
        match packer.pack_to_data_from_pixelmap(&mut options, pixel_map, &mut out) {
            Ok(written) if written > MAX_IMAGE_BYTES => return Ok(PackedPng::TooLarge(written)),
            Ok(written) => {
                let encoded = base64::engine::general_purpose::STANDARD.encode(&out[..written]);
                return Ok(PackedPng::Data {
                    uri: format!("data:image/png;base64,{encoded}"),
                    width,
                    height,
                });
            }
            Err(e) => {
                // `IMAGE_TOO_LARGE` (7600103), `IMAGE_ALLOC_FAILED` (7600301), and
                // `IMAGE_ENCODE_FAILED` (7800301) can indicate an undersized output buffer;
                // other codes (e.g. bad parameter / unsupported MIME) should fail immediately.
                let retryable = matches!(e.code, 7600103 | 7600301 | 7800301);
                if retryable && attempt < 2 && capacity < max_buf {
                    capacity = capacity.saturating_mul(2).min(max_buf);
                } else if retryable && (e.code == 7600103 || capacity >= max_buf) {
                    return Ok(PackedPng::TooLarge(max_buf));
                } else {
                    return Err(format!("{e:?}"));
                }
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
    data.records().ok()?.into_iter().find_map(|record| {
        record
            .plain_text()
            .ok()
            .and_then(|p| p.get_content().ok())
            .filter(|t| !t.is_empty())
            .or_else(|| {
                record
                    .html()
                    .ok()
                    .and_then(|h| h.get_primary_content().ok())
                    .filter(|t| !t.is_empty())
            })
    })
}

fn extract_html(data: &mut UdmfData) -> Option<String> {
    if let Ok(primary) = data.primary_html()
        && let Ok(html) = primary.get_html()
        && !html.is_empty()
    {
        return Some(html);
    }
    data.records().ok()?.into_iter().find_map(|record| {
        record
            .html()
            .ok()
            .and_then(|u| u.get_html().ok())
            .filter(|h| !h.is_empty())
    })
}

fn extract_file_uris(data: &mut UdmfData) -> Vec<String> {
    data.records()
        .ok()
        .into_iter()
        .flatten()
        .filter_map(|record| {
            record
                .file_uri()
                .ok()
                .and_then(|u| u.file_uri().ok())
                .filter(|uri| uri.starts_with("file:"))
        })
        .collect()
}

fn extract_first_pixelmap(data: &mut UdmfData) -> Option<PixelMap> {
    data.records()
        .ok()?
        .into_iter()
        .find_map(|record| record.pixel_map().ok().and_then(|u| u.to_pixel_map().ok()))
}
