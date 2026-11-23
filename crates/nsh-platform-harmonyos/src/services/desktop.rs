//! Desktop window management, native display enumeration/hotplug,
//! native global hotkeys, and application/popup menu state.
//! Pure conversions and wire schemas live in `desktop_spec`.

use std::sync::{Arc, Weak, atomic::Ordering};

use nsh_core::{
    Bounds, Point, Points, Size,
    desktop::{Display, DisplayId, MenuBarMode, MenuItem, MenuLabels, Shortcut},
};
use ohos_display_binding::{
    DisplayListener, all_displays, default_display_id, display_available_area, display_position,
    on_available_area_change, on_display_add, on_display_change, on_display_remove,
    primary_display,
};
use ohos_multi_modal_input_binding::{HotkeyMonitor, HotkeySpec};
use rustc_hash::FxHashMap;

use crate::app::runtime::{Dispatcher, Message};

pub(crate) use crate::services::desktop_spec::*;

/// Queries connected displays natively via `ohos-display-binding` (API 14/20).
pub(crate) fn read_native_displays() -> Vec<Display> {
    let screens = match all_displays() {
        Ok(list) if !list.is_empty() => list,
        _ => match primary_display() {
            Ok(primary) => vec![primary],
            Err(_) => return Vec::new(),
        },
    };
    let primary_id = primary_display()
        .map(|d| d.id)
        .unwrap_or_else(|_| default_display_id() as u32);
    let has_primary = screens.iter().any(|s| s.id == primary_id);

    screens
        .into_iter()
        .enumerate()
        .map(|(index, info)| {
            let scale = if info.density_pixels.is_finite() && info.density_pixels > 0.0 {
                (info.density_pixels * 10_000.0).round() / 10_000.0
            } else {
                1.0
            };
            let (raw_x, raw_y) = display_position(u64::from(info.id)).unwrap_or((0, 0));
            let x = raw_x as f32 / scale;
            let y = raw_y as f32 / scale;
            let width = (info.width.max(0) as f32) / scale;
            let height = (info.height.max(0) as f32) / scale;
            let bounds = Bounds::new(
                Point::new(Points(x), Points(y)),
                Size::new(Points(width), Points(height)),
            );
            let visible_bounds = match display_available_area(u64::from(info.id)) {
                Ok(area) if area.width > 0 && area.height > 0 => Bounds::new(
                    Point::new(
                        Points(x + area.left as f32 / scale),
                        Points(y + area.top as f32 / scale),
                    ),
                    Size::new(
                        Points(area.width as f32 / scale),
                        Points(area.height as f32 / scale),
                    ),
                ),
                _ => bounds,
            };
            let is_primary = if has_primary {
                info.id == primary_id
            } else {
                index == 0
            };
            let id_name = format!("display-{}", info.id);
            let id = DisplayId::from_name(id_name.as_bytes());
            let trimmed = info.name.trim();
            let label = if !trimmed.is_empty() && !trimmed.eq_ignore_ascii_case("UNKNOWN") {
                trimmed.to_owned()
            } else if is_primary {
                "Built-in Display".to_owned()
            } else {
                format!("Display {}", index + 1)
            };
            let mut display = Display::new(id, bounds, visible_bounds, is_primary);
            display.name = label;
            display.scale_factor = scale;
            display.refresh_rate = (info.refresh_rate > 0).then_some(info.refresh_rate as f32);
            display
        })
        .collect()
}

/// RAII owner for native display hotplug/metrics listeners.
/// Dropping this watcher unregisters every listener cleanly.
pub(crate) struct DisplayWatcher {
    _listeners: Vec<DisplayListener>,
}

impl DisplayWatcher {
    pub(crate) fn start<F>(on_change: F) -> Self
    where
        F: Fn() + Send + Sync + 'static,
    {
        let cb: Arc<dyn Fn() + Send + Sync> = Arc::new(on_change);
        let mut listeners = Vec::with_capacity(4);
        let c1 = cb.clone();
        if let Ok(listener) = on_display_change(move |_| c1()) {
            listeners.push(listener);
        }
        let c2 = cb.clone();
        if let Ok(listener) = on_display_add(move |_| c2()) {
            listeners.push(listener);
        }
        let c3 = cb.clone();
        if let Ok(listener) = on_display_remove(move |_| c3()) {
            listeners.push(listener);
        }
        let c4 = cb;
        if let Ok(listener) = on_available_area_change(move |_| c4()) {
            listeners.push(listener);
        }
        Self {
            _listeners: listeners,
        }
    }
}

/// Encodes a tray icon `ImageSource` into a wire payload for `NativeDesktop.ets`.
/// SVGs are rasterized as 48×48 premultiplied RGBA white/black templates so the status bar
/// icon stays crisp against both dark and light desktop wallpapers.
pub(crate) fn build_tray_icon(
    source: Option<&nsh_core::ImageSource>,
    assets: &dyn nsh_engine::Assets,
) -> Option<WireTrayIcon> {
    use base64::Engine as _;
    const TRAY_SIDE: u32 = 48;
    const MAX_ENCODED_BYTES: usize = 2 << 20;

    let source = source?;
    let bytes: Vec<u8> = if let Some(data) = &source.data {
        data.as_ref().to_vec()
    } else if source.uri.starts_with("asset:/") {
        assets.read(&source.uri).ok()?
    } else if let Some(rest) = source.uri.strip_prefix("file://") {
        let decoded = percent_encoding::percent_decode_str(rest).decode_utf8_lossy();
        std::fs::read(decoded.as_ref()).ok()?
    } else if source.uri.starts_with('/') {
        std::fs::read(source.uri.as_ref()).ok()?
    } else {
        return None;
    };
    if bytes.is_empty() || bytes.len() > MAX_ENCODED_BYTES {
        return None;
    }

    if nsh_core::svg::sniff(&bytes) {
        let raster = nsh_core::svg::rasterize(&bytes, TRAY_SIDE).ok()?;
        let fitted = nsh_core::desktop::menu_icon::canvas(
            &raster.pixels,
            raster.width,
            raster.height,
            TRAY_SIDE,
        );
        let mut white = vec![0u8; fitted.len()];
        let mut black = vec![0u8; fitted.len()];
        for (idx, chunk) in fitted.as_chunks::<4>().0.iter().enumerate() {
            let a = chunk[3];
            let offset = idx * 4;
            // Premultiplied RGBA: white = (a, a, a, a), black = (0, 0, 0, a).
            white[offset] = a;
            white[offset + 1] = a;
            white[offset + 2] = a;
            white[offset + 3] = a;
            black[offset + 3] = a;
        }
        return Some(WireTrayIcon {
            format: "rgba",
            width: Some(TRAY_SIDE),
            height: Some(TRAY_SIDE),
            white_base64: Some(base64::engine::general_purpose::STANDARD.encode(white)),
            black_base64: Some(base64::engine::general_purpose::STANDARD.encode(black)),
            data_base64: None,
        });
    }

    Some(WireTrayIcon {
        format: "encoded",
        width: None,
        height: None,
        white_base64: None,
        black_base64: None,
        data_base64: Some(base64::engine::general_purpose::STANDARD.encode(bytes)),
    })
}

pub(crate) struct DesktopState {
    pub(crate) labels: MenuLabels,
    pub(crate) menu: Option<std::sync::Arc<[MenuItem]>>,
    pub(crate) menu_bar_mode: MenuBarMode,
    pub(crate) menu_bar_alt_shown: bool,
    pub(crate) menu_bar_inset: std::cell::Cell<f32>,
    pub(crate) menu_bar_host: Option<(
        crate::render::node::NativeNode,
        crate::interop::attachment::NativeAttachment,
    )>,
    pub(crate) quit_on_request: bool,
    pub(crate) popup_generation: u32,
    pub(crate) popup_open: bool,
    pub(crate) close_on_press: bool,
    pub(crate) selection_menu_target: Option<nsh_core::NodeId>,
    /// Primary desktop window node hosted in-place inside EntryAbility's main window
    /// under NSH window model v2 (plain TSX implicit `"main"` or primary `<Window>`).
    pub(crate) hosted_main_window: Option<nsh_core::NodeId>,
    pub(crate) main_window_launch_applied: bool,
    pub(crate) hotkeys: FxHashMap<u32, HotkeyMonitor>,
}

impl Default for DesktopState {
    fn default() -> Self {
        Self {
            labels: MenuLabels::default(),
            menu: None,
            menu_bar_mode: MenuBarMode::Shown,
            menu_bar_alt_shown: false,
            menu_bar_inset: std::cell::Cell::new(0.0),
            menu_bar_host: None,
            quit_on_request: true,
            popup_generation: 0,
            popup_open: false,
            close_on_press: true,
            selection_menu_target: None,
            hosted_main_window: None,
            main_window_launch_applied: false,
            hotkeys: FxHashMap::default(),
        }
    }
}

impl DesktopState {
    fn advance_popup_generation(&mut self) -> u32 {
        let mut next = self.popup_generation.wrapping_add(1);
        if next == 0 || next == TRAY_MENU_GENERATION {
            next = 1;
        }
        self.popup_generation = next;
        next
    }

    pub(crate) fn next_popup_generation(&mut self) -> u32 {
        let generation = self.advance_popup_generation();
        self.popup_open = true;
        generation
    }

    pub(crate) fn next_selection_popup_generation(&mut self) -> u32 {
        let generation = self.advance_popup_generation();
        self.popup_open = false;
        generation
    }

    /// Registers a global hotkey natively via `ohos-multi-modal-input-binding`.
    pub(crate) fn register_global_shortcut(
        &mut self,
        id: u32,
        shortcut: Shortcut,
        route: Weak<Dispatcher>,
    ) -> bool {
        self.hotkeys.remove(&id);
        if id == 0 {
            return false;
        }
        let Some((pre_keys, final_key)) = ohos_hotkey(shortcut) else {
            return false;
        };
        let spec = HotkeySpec::new(pre_keys, final_key);
        match HotkeyMonitor::register(&spec, move |_| {
            if let Some(dispatcher) = route.upgrade()
                && dispatcher.live.load(Ordering::Acquire)
            {
                dispatcher.send(Message::HostDesktop(DesktopEvent::GlobalShortcutTrigger {
                    id,
                }));
            }
        }) {
            Ok(monitor) => {
                self.hotkeys.insert(id, monitor);
                true
            }
            Err(error) => {
                log::warn!("OHOS native global shortcut registration failed (id={id}): {error}");
                false
            }
        }
    }

    pub(crate) fn unregister_global_shortcut(&mut self, id: u32) {
        self.hotkeys.remove(&id);
    }

    pub(crate) fn clear_hotkeys(&mut self) {
        self.hotkeys.clear();
    }
}
