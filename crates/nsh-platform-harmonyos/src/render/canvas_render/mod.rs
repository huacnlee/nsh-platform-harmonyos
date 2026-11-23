//! Offscreen Canvas 2D rendering (`Command::RenderCanvas`) into encoded PNG bytes.
//! Rasterization and PNG packing run on the background image decode pool so large
//! offscreen exports never stall the UI frame clock.

mod spec;

use std::sync::{
    Arc, Mutex, Weak,
    atomic::{AtomicBool, Ordering},
};

use nsh_core::{
    EventKind, EventPayload,
    canvas::{
        Drawing,
        render::{ImageFormat, RenderFailure},
    },
    image::loader::{self, Priority},
};
use ohos_drawing_binding::{AlphaFormat, Bitmap, BitmapFormat, Canvas, ColorFormat, ImageInfo};
use ohos_image_native_binding::{
    ImageNativeError, ImagePacker, ImageString, PackingOptions, PixelMap,
    PixelMapInitializationOptions, sys,
    types::{PixelFormat, PixelMapAlphaType},
};
use rustc_hash::FxHashMap;

use self::spec::{RenderSpec, pack_buffer_size, unique_image_uris, validate_request};
use crate::{
    app::runtime::{Dispatcher, Message},
    media::image::{Data, Loader, Target},
    text::fonts::Fonts,
};

struct PendingRender {
    spec: RenderSpec,
    drawing: Arc<Drawing>,
    rtl: bool,
    uris: Vec<Arc<str>>,
    loaded: Vec<Option<Arc<Data>>>,
    remaining: usize,
    jobs: Vec<Arc<AtomicBool>>,
}

#[derive(Default)]
pub(crate) struct OffscreenRenders {
    pending: FxHashMap<u32, PendingRender>,
    active: Vec<Arc<AtomicBool>>,
}

impl OffscreenRenders {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn start(
        &mut self,
        id: u32,
        drawing: Arc<Drawing>,
        width: u32,
        height: u32,
        scale: f32,
        format: ImageFormat,
        rtl: bool,
        images: &Arc<Loader>,
        fonts: &Arc<Mutex<Fonts>>,
        dispatcher: &Weak<Dispatcher>,
    ) -> Result<(), RenderFailure> {
        self.active.retain(|flag| Arc::strong_count(flag) > 1);
        let spec = validate_request(width, height, scale, format)?;
        let uris = unique_image_uris(&drawing);
        if uris.is_empty() {
            self.spawn(id, spec, drawing, rtl, Vec::new(), fonts, dispatcher);
            return Ok(());
        }
        let count = uris.len();
        let mut jobs = Vec::with_capacity(count);
        for (index, uri) in uris.iter().enumerate() {
            jobs.push(images.load(Target::OffscreenCanvas { id, index }, uri.to_string(), None));
        }
        self.pending.insert(
            id,
            PendingRender {
                spec,
                drawing,
                rtl,
                uris,
                loaded: vec![None; count],
                remaining: count,
                jobs,
            },
        );
        Ok(())
    }

    pub(crate) fn image_ready(
        &mut self,
        id: u32,
        index: usize,
        data: Result<Arc<Data>, String>,
        fonts: &Arc<Mutex<Fonts>>,
        dispatcher: &Weak<Dispatcher>,
    ) {
        let ready_to_render = if let Some(entry) = self.pending.get_mut(&id) {
            if index >= entry.loaded.len() {
                return;
            }
            match data {
                Ok(decoded) => entry.loaded[index] = Some(decoded),
                Err(error) => {
                    log::warn!("OHOS offscreen canvas image {}: {error}", entry.uris[index]);
                }
            }
            entry.remaining = entry.remaining.saturating_sub(1);
            entry.remaining == 0
        } else {
            return;
        };
        if ready_to_render && let Some(entry) = self.pending.remove(&id) {
            let images = entry
                .uris
                .into_iter()
                .zip(entry.loaded)
                .filter_map(|(uri, data)| data.map(|data| (uri, data)))
                .collect();
            self.spawn(
                id,
                entry.spec,
                entry.drawing,
                entry.rtl,
                images,
                fonts,
                dispatcher,
            );
        }
    }

    pub(crate) fn close(&mut self) {
        for (_, pending) in self.pending.drain() {
            for job in pending.jobs {
                job.store(true, Ordering::Relaxed);
            }
        }
        for flag in self.active.drain(..) {
            flag.store(true, Ordering::Relaxed);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn spawn(
        &mut self,
        id: u32,
        spec: RenderSpec,
        drawing: Arc<Drawing>,
        rtl: bool,
        images: Vec<(Arc<str>, Arc<Data>)>,
        fonts: &Arc<Mutex<Fonts>>,
        dispatcher: &Weak<Dispatcher>,
    ) {
        let cancelled = Arc::new(AtomicBool::new(false));
        self.active.push(cancelled.clone());
        let fonts = fonts.clone();
        let dispatcher = dispatcher.clone();
        loader::loader()
            .decode(Priority::Normal, move || {
                if cancelled.load(Ordering::Relaxed) {
                    return;
                }
                let result = render_and_encode(spec, &drawing, rtl, &images, &fonts, &cancelled);
                if !cancelled.load(Ordering::Relaxed)
                    && let Some(dispatcher) = dispatcher.upgrade()
                {
                    dispatcher.send(Message::HostEvent(
                        EventKind::CanvasImage,
                        EventPayload::CanvasImage { id, result },
                    ));
                }
            })
            .detach();
    }
}

impl Drop for OffscreenRenders {
    fn drop(&mut self) {
        self.close();
    }
}

fn map_image_error(error: ImageNativeError) -> RenderFailure {
    if error.code == sys::Image_ErrorCode_IMAGE_ALLOC_FAILED {
        RenderFailure::TooLarge(format!("image allocation failed ({error})"))
    } else {
        RenderFailure::Failed(format!("offscreen canvas encode failed ({error})"))
    }
}

fn render_and_encode(
    spec: RenderSpec,
    drawing: &Drawing,
    rtl: bool,
    loaded_images: &[(Arc<str>, Arc<Data>)],
    fonts: &Arc<Mutex<Fonts>>,
    cancelled: &AtomicBool,
) -> Result<Vec<u8>, RenderFailure> {
    let mut bound_images = Vec::with_capacity(loaded_images.len());
    for (uri, data) in loaded_images {
        if cancelled.load(Ordering::Relaxed) {
            return Err(RenderFailure::Failed("cancelled".into()));
        }
        match data.bitmap() {
            Ok(bitmap) => bound_images.push((uri.clone(), bitmap)),
            Err(error) => log::warn!("OHOS offscreen canvas image {uri}: {error}"),
        }
    }
    let bitmap = Bitmap::new(spec.width, spec.height, BitmapFormat::default());
    if bitmap.pixels().len() != spec.byte_len {
        return Err(RenderFailure::TooLarge(format!(
            "failed to allocate {}x{} offscreen canvas bitmap",
            spec.width, spec.height
        )));
    }
    let canvas = Canvas::with_bitmap(bitmap);
    canvas.clear(0);
    crate::render::canvas::replay_offscreen(
        &canvas,
        drawing,
        spec.scale,
        &bound_images,
        fonts,
        rtl,
    );
    drop(bound_images);
    if cancelled.load(Ordering::Relaxed) {
        return Err(RenderFailure::Failed("cancelled".into()));
    }

    let mut rgba = Vec::new();
    if rgba.try_reserve_exact(spec.byte_len).is_err() {
        return Err(RenderFailure::TooLarge(format!(
            "failed to allocate {}x{} offscreen pixel buffer",
            spec.width, spec.height
        )));
    }
    rgba.resize(spec.byte_len, 0);
    let row_bytes = (spec.width as usize) * 4;
    let info = ImageInfo::new(
        spec.width as i32,
        spec.height as i32,
        ColorFormat::Rgba8888,
        AlphaFormat::Unpremul,
    );
    let read_ok = canvas
        .bitmap()
        .is_some_and(|bitmap| bitmap.read_pixels(info, &mut rgba, row_bytes, 0, 0));
    // Free the backing drawing bitmap before allocating PixelMap and PNG output.
    drop(canvas);
    if !read_ok {
        return Err(RenderFailure::Failed(
            "failed to read pixels from offscreen canvas bitmap".into(),
        ));
    }
    if cancelled.load(Ordering::Relaxed) {
        return Err(RenderFailure::Failed("cancelled".into()));
    }

    let mut init = PixelMapInitializationOptions::new().map_err(map_image_error)?;
    init.set_width(spec.width).map_err(map_image_error)?;
    init.set_height(spec.height).map_err(map_image_error)?;
    init.set_src_pixel_format(PixelFormat::Rgba8888)
        .map_err(map_image_error)?;
    init.set_pixel_format(PixelFormat::Rgba8888)
        .map_err(map_image_error)?;
    init.set_alpha_type(PixelMapAlphaType::Unpremultiplied)
        .map_err(map_image_error)?;
    let pixelmap = PixelMap::create(&mut rgba, &mut init).map_err(map_image_error)?;
    drop(rgba);

    let mut options = PackingOptions::new().map_err(map_image_error)?;
    let mut mime = ImageString::from_str(spec.format.mime()).map_err(map_image_error)?;
    options.set_mime_type(&mut mime).map_err(map_image_error)?;
    options.set_quality(100).map_err(map_image_error)?;
    let mut packer = ImagePacker::new().map_err(map_image_error)?;

    let mut capacity = pack_buffer_size(spec.width, spec.height);
    let mut last_error = None;
    for _ in 0..2 {
        if cancelled.load(Ordering::Relaxed) {
            return Err(RenderFailure::Failed("cancelled".into()));
        }
        let mut out = Vec::new();
        if out.try_reserve_exact(capacity).is_err() {
            return Err(RenderFailure::TooLarge(
                "failed to allocate offscreen PNG output buffer".into(),
            ));
        }
        out.resize(capacity, 0);
        match packer.pack_to_data_from_pixelmap(&mut options, &pixelmap, &mut out) {
            Ok(written) => {
                out.truncate(written);
                return Ok(out);
            }
            Err(error) => {
                last_error = Some(error);
                capacity = capacity.saturating_mul(2);
            }
        }
    }
    Err(map_image_error(last_error.expect("pack attempted")))
}
