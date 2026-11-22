//! Desktop window management, application/popup menus, and display reporting.
//! Pure conversions and menu expansion stay host-testable outside `target_env = "ohos"`.

use nsh_core::{
    Bounds, Edges, EventKind, EventPayload, PayloadValue, Point, Points, Size,
    desktop::{
        Backdrop, CloseReason, DesktopCommand, Display, DisplayId, Key as DesktopKey,
        MainWindowOptions, MenuItem, MenuLabels, MenuRole, OpenWindow, ResizeEdge, Shortcut,
        SizeToContent, TitleBarStyle, WindowAttributes, WindowFrameStyle, WindowKind, WindowLevel,
        WindowMode, WindowPlacement, WindowPointerEvents, WindowPosition, WindowRequest,
        WindowState,
    },
    keymap::{Key as KeymapKey, Keystroke},
    platform::{Idiom, Os},
    single_instance::SecondInstance,
};
use serde::{Deserialize, Serialize};

/// Resolves the reported NSH OS for an OHOS device idiom.
/// Phone and tablet stay on `Os::Android`; `2in1`/PC desktop reports `Os::Linux`.
pub(crate) fn os_for_idiom(idiom: Idiom) -> Os {
    if idiom == Idiom::Desktop {
        Os::Linux
    } else {
        Os::Android
    }
}

/// Native navigation header heights for the resolved OS.
pub(crate) fn header_heights_for_os(os: Os) -> (Points, Points) {
    if os.is_desktop() {
        nsh_core::NO_NATIVE_HEADER
    } else {
        (Points(52.0), Points(52.0))
    }
}

/// Converts a keymap `Keystroke` into a desktop menu `Shortcut` for `MenuItem::find_shortcut`.
pub(crate) fn shortcut_from_keystroke(stroke: Keystroke) -> Option<Shortcut> {
    use nsh_core::desktop::Key as DesktopKey;
    let key = match stroke.key {
        KeymapKey::Char(' ') => DesktopKey::Space,
        KeymapKey::Char(c) => DesktopKey::Char(c),
        KeymapKey::Enter => DesktopKey::Enter,
        KeymapKey::Escape => DesktopKey::Escape,
        KeymapKey::Tab => DesktopKey::Tab,
        KeymapKey::Backspace => DesktopKey::Backspace,
        KeymapKey::Delete => DesktopKey::Delete,
        KeymapKey::Insert => DesktopKey::Insert,
        KeymapKey::Up => DesktopKey::Up,
        KeymapKey::Down => DesktopKey::Down,
        KeymapKey::Left => DesktopKey::Left,
        KeymapKey::Right => DesktopKey::Right,
        KeymapKey::Home => DesktopKey::Home,
        KeymapKey::End => DesktopKey::End,
        KeymapKey::PageUp => DesktopKey::PageUp,
        KeymapKey::PageDown => DesktopKey::PageDown,
        KeymapKey::F(n) => DesktopKey::F(n),
    };
    Some(Shortcut {
        key,
        ctrl: stroke.modifiers.ctrl,
        alt: stroke.modifiers.alt,
        shift: stroke.modifiers.shift,
        meta: stroke.modifiers.cmd,
    })
}

/// Converts a `MenuRole` to its wire name used across the ArkTS bridge.
pub(crate) fn role_name(role: MenuRole) -> &'static str {
    match role {
        MenuRole::About => "about",
        MenuRole::Hide => "hide",
        MenuRole::HideOthers => "hideOthers",
        MenuRole::ShowAll => "showAll",
        MenuRole::Quit => "quit",
        MenuRole::Undo => "undo",
        MenuRole::Redo => "redo",
        MenuRole::Cut => "cut",
        MenuRole::Copy => "copy",
        MenuRole::Paste => "paste",
        MenuRole::SelectAll => "selectAll",
        MenuRole::Minimize => "minimize",
        MenuRole::Zoom => "zoom",
        MenuRole::ToggleFullScreen => "toggleFullScreen",
        MenuRole::Close => "close",
    }
}

/// Parses a `MenuRole` from its wire name.
pub(crate) fn parse_role(name: &str) -> Option<MenuRole> {
    Some(match name {
        "about" => MenuRole::About,
        "hide" => MenuRole::Hide,
        "hideOthers" => MenuRole::HideOthers,
        "showAll" => MenuRole::ShowAll,
        "quit" => MenuRole::Quit,
        "undo" => MenuRole::Undo,
        "redo" => MenuRole::Redo,
        "cut" => MenuRole::Cut,
        "copy" => MenuRole::Copy,
        "paste" => MenuRole::Paste,
        "selectAll" => MenuRole::SelectAll,
        "minimize" => MenuRole::Minimize,
        "zoom" => MenuRole::Zoom,
        "toggleFullScreen" => MenuRole::ToggleFullScreen,
        "close" => MenuRole::Close,
        _ => return None,
    })
}

/// Serialized menu item crossed to `NativeDesktop.ets` as JSON.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct WireMenuItem {
    pub kind: String,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub id: u32,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub label: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checked: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shortcut: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub destructive: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<WireMenuItem>,
}

fn is_zero(v: &u32) -> bool {
    *v == 0
}
fn is_false(v: &bool) -> bool {
    !*v
}
fn default_true() -> bool {
    true
}

/// Expands inline sections and converts menu items into the ArkTS wire format.
pub(crate) fn serialize_menu_items(items: &[MenuItem], labels: &MenuLabels) -> Vec<WireMenuItem> {
    fn convert(items: &[MenuItem], labels: &MenuLabels) -> Vec<WireMenuItem> {
        let mut out = Vec::with_capacity(items.len());
        for item in items {
            match item {
                MenuItem::Separator => out.push(WireMenuItem {
                    kind: "separator".into(),
                    id: 0,
                    label: String::new(),
                    enabled: true,
                    checked: None,
                    shortcut: None,
                    role: None,
                    destructive: false,
                    items: Vec::new(),
                }),
                MenuItem::Action(action) => {
                    let label = if !action.label.is_empty() {
                        action.label.clone()
                    } else if let Some(role) = action.role {
                        labels.label(role).to_owned()
                    } else {
                        String::new()
                    };
                    let shortcut = action
                        .shortcut
                        .or_else(|| action.role.and_then(|role| role.shortcut(false)))
                        .map(|s| s.label_for(Os::Linux));
                    out.push(WireMenuItem {
                        kind: "action".into(),
                        id: action.id,
                        label,
                        enabled: action.enabled,
                        checked: action.checked,
                        shortcut,
                        role: action.role.map(|r| role_name(r).to_owned()),
                        destructive: action.destructive,
                        items: Vec::new(),
                    });
                }
                MenuItem::Submenu {
                    label,
                    enabled,
                    items,
                    ..
                } => out.push(WireMenuItem {
                    kind: "submenu".into(),
                    id: 0,
                    label: label.clone(),
                    enabled: *enabled,
                    checked: None,
                    shortcut: None,
                    role: None,
                    destructive: false,
                    items: convert(items, labels),
                }),
            }
        }
        out
    }
    let expanded = MenuItem::with_inline_expanded(items);
    convert(&expanded, labels)
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct WireSize {
    pub width: f32,
    pub height: f32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct WireBounds {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl From<WireBounds> for Bounds<Points> {
    fn from(b: WireBounds) -> Self {
        Bounds::new(
            Point::new(Points(b.x), Points(b.y)),
            Size::new(Points(b.width.max(0.0)), Points(b.height.max(0.0))),
        )
    }
}

/// Serialized tray icon crossed to `NativeDesktop.ets`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct WireTrayIcon {
    pub format: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub white_base64: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub black_base64: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data_base64: Option<String>,
}

/// Wire command sent from Rust to `NativeDesktop.ets` via `HostCommand { kind: "desktop", text }`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "op", rename_all = "camelCase")]
pub(crate) enum WireCommand {
    #[serde(rename_all = "camelCase")]
    OpenSubWindow {
        window_id: u32,
        kind: &'static str,
        modal: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        parent_window_id: Option<u32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        display: Option<String>,
        width: f32,
        height: f32,
        position: &'static str,
        #[serde(skip_serializing_if = "Option::is_none")]
        x: Option<f32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        y: Option<f32>,
        mode: &'static str,
        #[serde(skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        title_bar_style: Option<&'static str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        title_bar_height: Option<f32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        frame: Option<&'static str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        min_size: Option<Option<WireSize>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        max_size: Option<Option<WireSize>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        resizable: Option<bool>,
        #[serde(skip_serializing_if = "Option::is_none")]
        movable: Option<bool>,
        #[serde(skip_serializing_if = "Option::is_none")]
        minimizable: Option<bool>,
        #[serde(skip_serializing_if = "Option::is_none")]
        maximizable: Option<bool>,
        #[serde(skip_serializing_if = "Option::is_none")]
        closable: Option<bool>,
        #[serde(skip_serializing_if = "Option::is_none")]
        focusable: Option<bool>,
        #[serde(skip_serializing_if = "Option::is_none")]
        activate: Option<bool>,
        #[serde(skip_serializing_if = "Option::is_none")]
        close_on_press: Option<bool>,
        #[serde(skip_serializing_if = "Option::is_none")]
        close_on_blur: Option<bool>,
        #[serde(skip_serializing_if = "Option::is_none")]
        level: Option<&'static str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pointer_events: Option<&'static str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        transparent: Option<bool>,
        #[serde(skip_serializing_if = "Option::is_none")]
        backdrop: Option<&'static str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        shadow: Option<bool>,
    },
    #[serde(rename_all = "camelCase")]
    CloseSubWindow {
        window_id: u32,
    },
    #[serde(rename_all = "camelCase")]
    SetAttributes {
        #[serde(skip_serializing_if = "Option::is_none")]
        window_id: Option<u32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        title_bar_style: Option<&'static str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        title_bar_height: Option<f32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        frame: Option<&'static str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        min_size: Option<Option<WireSize>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        max_size: Option<Option<WireSize>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        resizable: Option<bool>,
        #[serde(skip_serializing_if = "Option::is_none")]
        movable: Option<bool>,
        #[serde(skip_serializing_if = "Option::is_none")]
        minimizable: Option<bool>,
        #[serde(skip_serializing_if = "Option::is_none")]
        maximizable: Option<bool>,
        #[serde(skip_serializing_if = "Option::is_none")]
        closable: Option<bool>,
        #[serde(skip_serializing_if = "Option::is_none")]
        focusable: Option<bool>,
        #[serde(skip_serializing_if = "Option::is_none")]
        activate: Option<bool>,
        #[serde(skip_serializing_if = "Option::is_none")]
        close_on_press: Option<bool>,
        #[serde(skip_serializing_if = "Option::is_none")]
        close_on_blur: Option<bool>,
        #[serde(skip_serializing_if = "Option::is_none")]
        level: Option<&'static str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pointer_events: Option<&'static str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        transparent: Option<bool>,
        #[serde(skip_serializing_if = "Option::is_none")]
        backdrop: Option<&'static str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        shadow: Option<bool>,
    },
    #[serde(rename_all = "camelCase")]
    Request {
        #[serde(skip_serializing_if = "Option::is_none")]
        window_id: Option<u32>,
        action: &'static str,
        #[serde(skip_serializing_if = "Option::is_none")]
        mode: Option<&'static str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        x: Option<f32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        y: Option<f32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        width: Option<f32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        height: Option<f32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        display: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        critical: Option<bool>,
    },
    #[serde(rename_all = "camelCase")]
    PopupMenu {
        #[serde(skip_serializing_if = "Option::is_none")]
        window_id: Option<u32>,
        generation: u32,
        #[serde(skip_serializing_if = "Option::is_none")]
        x: Option<f32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        y: Option<f32>,
        items: Vec<WireMenuItem>,
    },
    ClosePopupMenu,
    #[serde(rename_all = "camelCase")]
    SetTray {
        #[serde(skip_serializing_if = "String::is_empty")]
        tooltip: String,
        #[serde(skip_serializing_if = "String::is_empty")]
        title: String,
        click: u32,
        #[serde(skip_serializing_if = "Option::is_none")]
        icon: Option<WireTrayIcon>,
        items: Vec<WireMenuItem>,
    },
    RemoveTray,
    #[serde(rename_all = "camelCase")]
    RegisterGlobalShortcut {
        id: u32,
        pre_keys: Vec<i32>,
        final_key: i32,
    },
    #[serde(rename_all = "camelCase")]
    UnregisterGlobalShortcut {
        id: u32,
    },
    #[serde(rename_all = "camelCase")]
    StartMoving {
        #[serde(skip_serializing_if = "Option::is_none")]
        window_id: Option<u32>,
    },
    #[serde(rename_all = "camelCase")]
    ToggleMaximize {
        #[serde(skip_serializing_if = "Option::is_none")]
        window_id: Option<u32>,
    },
    #[serde(rename_all = "camelCase")]
    ApplyMainWindowLaunch {
        #[serde(skip_serializing_if = "Option::is_none")]
        x: Option<f32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        y: Option<f32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        width: Option<f32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        height: Option<f32>,
        content_sized: bool,
        #[serde(skip_serializing_if = "is_false")]
        title_bar_hidden: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        position: Option<&'static str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        display: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        mode: Option<&'static str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        visible: Option<bool>,
    },
    Quit,
    Restart,
}

pub(crate) fn mode_name(mode: WindowMode) -> &'static str {
    match mode {
        WindowMode::Normal => "normal",
        WindowMode::Maximized => "maximized",
        WindowMode::FullScreen => "fullScreen",
    }
}

pub(crate) fn parse_mode(name: &str) -> WindowMode {
    match name {
        "maximized" => WindowMode::Maximized,
        "fullScreen" => WindowMode::FullScreen,
        _ => WindowMode::Normal,
    }
}

pub(crate) fn kind_name(kind: WindowKind) -> &'static str {
    match kind {
        WindowKind::Normal => "normal",
        WindowKind::Dialog => "dialog",
        WindowKind::Panel => "panel",
        WindowKind::Popup => "popup",
        WindowKind::Overlay => "overlay",
    }
}

pub(crate) fn backdrop_name(backdrop: Backdrop) -> &'static str {
    match backdrop {
        Backdrop::None => "none",
        Backdrop::Window => "window",
        Backdrop::Sidebar => "sidebar",
        Backdrop::Popover => "popover",
        Backdrop::Hud => "hud",
    }
}

pub(crate) fn wire_attributes(attrs: &WindowAttributes) -> WireCommand {
    wire_attributes_for(None, attrs)
}

pub(crate) fn wire_attributes_for(window_id: Option<u32>, attrs: &WindowAttributes) -> WireCommand {
    WireCommand::SetAttributes {
        window_id,
        title: attrs.title.clone(),
        title_bar_style: attrs.title_bar.map(|tb| match tb.style {
            TitleBarStyle::Default => "default",
            TitleBarStyle::Hidden => "hidden",
        }),
        title_bar_height: attrs.title_bar.and_then(|tb| tb.height.map(|h| h.0)),
        frame: attrs.frame.map(|frame| match frame {
            WindowFrameStyle::Default => "default",
            WindowFrameStyle::None => "none",
        }),
        min_size: attrs.min_size.map(|opt| {
            opt.map(|s| WireSize {
                width: s.width.0,
                height: s.height.0,
            })
        }),
        max_size: attrs.max_size.map(|opt| {
            opt.map(|s| WireSize {
                width: s.width.0,
                height: s.height.0,
            })
        }),
        resizable: attrs.resizable,
        movable: attrs.movable,
        minimizable: attrs.minimizable,
        maximizable: attrs.maximizable,
        closable: attrs.closable,
        focusable: attrs.focusable,
        activate: attrs.activate,
        close_on_press: attrs.close_on_press,
        close_on_blur: attrs.close_on_blur,
        level: attrs.level.map(|level| match level {
            WindowLevel::Normal => "normal",
            WindowLevel::Floating => "floating",
            WindowLevel::AboveDock => "aboveDock",
            WindowLevel::DesktopIcon => "desktopIcon",
        }),
        pointer_events: attrs.pointer_events.map(|pe| match pe {
            WindowPointerEvents::Auto => "auto",
            WindowPointerEvents::None => "none",
        }),
        transparent: attrs.transparent,
        backdrop: attrs.backdrop.map(backdrop_name),
        shadow: attrs.shadow,
    }
}

pub(crate) fn wire_open_sub_window(
    window_id: u32,
    open: &OpenWindow,
    size: (f32, f32),
) -> WireCommand {
    let attrs = &open.options.attributes;
    let mode = open
        .placement
        .map(|p| p.mode)
        .or(open.options.default_mode)
        .unwrap_or(WindowMode::Normal);
    let (position, x, y) = if let Some(placement) = open.placement {
        (
            "at",
            Some(placement.bounds.origin.x.0),
            Some(placement.bounds.origin.y.0),
        )
    } else {
        match open.options.default_position {
            Some(WindowPosition::At(point)) => ("at", Some(point.x.0), Some(point.y.0)),
            Some(WindowPosition::Center) => ("center", None, None),
            Some(WindowPosition::ParentCenter) => ("parentCenter", None, None),
            None => match open.options.kind {
                WindowKind::Dialog => ("parentCenter", None, None),
                _ => ("auto", None, None),
            },
        }
    };
    let parent_window_id = open.parent.filter(|id| *id != nsh_core::ROOT_ID);
    let display = open.options.display.map(|id| format!("{:016x}", id.0));
    WireCommand::OpenSubWindow {
        window_id,
        kind: kind_name(open.options.kind),
        modal: open.options.modal,
        parent_window_id,
        display,
        width: size.0,
        height: size.1,
        position,
        x,
        y,
        mode: mode_name(mode),
        title: attrs.title.clone(),
        title_bar_style: attrs.title_bar.map(|tb| match tb.style {
            TitleBarStyle::Default => "default",
            TitleBarStyle::Hidden => "hidden",
        }),
        title_bar_height: attrs.title_bar.and_then(|tb| tb.height.map(|h| h.0)),
        frame: attrs.frame.map(|frame| match frame {
            WindowFrameStyle::Default => "default",
            WindowFrameStyle::None => "none",
        }),
        min_size: attrs.min_size.map(|opt| {
            opt.map(|s| WireSize {
                width: s.width.0,
                height: s.height.0,
            })
        }),
        max_size: attrs.max_size.map(|opt| {
            opt.map(|s| WireSize {
                width: s.width.0,
                height: s.height.0,
            })
        }),
        resizable: attrs.resizable,
        movable: attrs.movable,
        minimizable: attrs.minimizable,
        maximizable: attrs.maximizable,
        closable: attrs.closable,
        focusable: attrs.focusable,
        activate: attrs.activate,
        close_on_press: attrs.close_on_press,
        close_on_blur: attrs.close_on_blur,
        level: attrs.level.map(|level| match level {
            WindowLevel::Normal => "normal",
            WindowLevel::Floating => "floating",
            WindowLevel::AboveDock => "aboveDock",
            WindowLevel::DesktopIcon => "desktopIcon",
        }),
        pointer_events: attrs.pointer_events.map(|pe| match pe {
            WindowPointerEvents::Auto => "auto",
            WindowPointerEvents::None => "none",
        }),
        transparent: attrs.transparent,
        backdrop: attrs.backdrop.map(backdrop_name),
        shadow: attrs.shadow,
    }
}

/// Hit-test margin (vp) along the perimeter of frameless resizable desktop windows.
pub(crate) const FRAMELESS_RESIZE_MARGIN_VP: f32 = 6.0;

/// Fallback initial content size (vp) when a subwindow omits both `defaultSize` and `placement`.
/// HarmonyOS `createSubWindowWithOptions` creates a 0×0 window when `windowRect` is omitted.
pub(crate) fn default_sub_window_size(kind: WindowKind) -> (f32, f32) {
    match kind {
        WindowKind::Panel => (320.0, 440.0),
        WindowKind::Dialog => (420.0, 320.0),
        WindowKind::Popup => (520.0, 360.0),
        _ => (640.0, 520.0),
    }
}

/// Clamps `size` (vp) to explicit `min_size` / `max_size` window attributes without imposing an
/// artificial platform floor (HarmonyOS enforces system window minimums via `WindowLimits`).
pub(crate) fn clamp_window_size(size: (f32, f32), attrs: &WindowAttributes) -> (f32, f32) {
    let (mut w, mut h) = size;
    if let Some(Some(min)) = attrs.min_size {
        if min.width.0.is_finite() && min.width.0 > 0.0 {
            w = w.max(min.width.0);
        }
        if min.height.0.is_finite() && min.height.0 > 0.0 {
            h = h.max(min.height.0);
        }
    }
    if let Some(Some(max)) = attrs.max_size {
        if max.width.0.is_finite() && max.width.0 > 0.0 {
            w = w.min(max.width.0);
        }
        if max.height.0.is_finite() && max.height.0 > 0.0 {
            h = h.min(max.height.0);
        }
    }
    (w.max(1.0), h.max(1.0))
}

#[allow(dead_code)]
pub(crate) fn wire_main_window_launch(
    options: &MainWindowOptions,
    kept: Option<WindowPlacement>,
) -> Option<WireCommand> {
    // HarmonyOS EntryAbility renders directly in its main window rather than a
    // hidden host window, so only minimize when the app explicitly requests
    // `defaultVisible: false`.
    let visible = (options.default_visible == Some(false)).then_some(false);
    let title_bar_hidden = options.attributes.frame == Some(WindowFrameStyle::None)
        || options
            .attributes
            .title_bar
            .is_some_and(|tb| tb.style == TitleBarStyle::Hidden);
    if let Some(placement) = kept {
        return Some(WireCommand::ApplyMainWindowLaunch {
            x: Some(placement.bounds.origin.x.0),
            y: Some(placement.bounds.origin.y.0),
            width: Some(placement.bounds.size.width.0),
            height: Some(placement.bounds.size.height.0),
            content_sized: false,
            title_bar_hidden,
            position: Some("at"),
            display: None,
            mode: Some(mode_name(placement.mode)),
            visible,
        });
    }
    let size = options
        .default_size
        .map(|s| clamp_window_size((s.width.0, s.height.0), &options.attributes));
    let (position, x, y) = match options.default_position {
        Some(WindowPosition::At(point)) => (Some("at"), Some(point.x.0), Some(point.y.0)),
        Some(WindowPosition::Center) => (Some("center"), None, None),
        Some(WindowPosition::ParentCenter) => (Some("parentCenter"), None, None),
        None => (None, None, None),
    };
    let display = options.display.map(|id| format!("{:016x}", id.0));
    let mode = options.default_mode.map(mode_name);
    if size.is_none()
        && position.is_none()
        && display.is_none()
        && mode.is_none()
        && visible.is_none()
    {
        return None;
    }
    Some(WireCommand::ApplyMainWindowLaunch {
        x,
        y,
        width: size.map(|(w, _)| w),
        height: size.map(|(_, h)| h),
        content_sized: size.is_some(),
        title_bar_hidden,
        position,
        display,
        mode,
        visible,
    })
}

#[allow(dead_code)]
pub(crate) fn wire_open_main_window_launch(open: &OpenWindow) -> Option<WireCommand> {
    let mut main = MainWindowOptions::default();
    main.default_size = open.options.default_size;
    main.default_position = open.options.default_position;
    main.display = open.options.display;
    main.default_mode = open.options.default_mode;
    main.attributes = open.options.attributes.clone();
    wire_main_window_launch(&main, open.placement)
}

pub(crate) fn wire_request(request: &WindowRequest) -> WireCommand {
    wire_request_for(None, request)
}

pub(crate) fn wire_request_for(window_id: Option<u32>, request: &WindowRequest) -> WireCommand {
    match request {
        WindowRequest::Show => WireCommand::Request {
            window_id,
            action: "show",
            mode: None,
            x: None,
            y: None,
            width: None,
            height: None,
            display: None,
            critical: None,
        },
        WindowRequest::Hide => WireCommand::Request {
            window_id,
            action: "hide",
            mode: None,
            x: None,
            y: None,
            width: None,
            height: None,
            display: None,
            critical: None,
        },
        WindowRequest::Focus => WireCommand::Request {
            window_id,
            action: "focus",
            mode: None,
            x: None,
            y: None,
            width: None,
            height: None,
            display: None,
            critical: None,
        },
        WindowRequest::Close => WireCommand::Request {
            window_id,
            action: "close",
            mode: None,
            x: None,
            y: None,
            width: None,
            height: None,
            display: None,
            critical: None,
        },
        WindowRequest::Minimize => WireCommand::Request {
            window_id,
            action: "minimize",
            mode: None,
            x: None,
            y: None,
            width: None,
            height: None,
            display: None,
            critical: None,
        },
        WindowRequest::Restore => WireCommand::Request {
            window_id,
            action: "restore",
            mode: None,
            x: None,
            y: None,
            width: None,
            height: None,
            display: None,
            critical: None,
        },
        WindowRequest::SetMode(mode) => WireCommand::Request {
            window_id,
            action: "setMode",
            mode: Some(mode_name(*mode)),
            x: None,
            y: None,
            width: None,
            height: None,
            display: None,
            critical: None,
        },
        WindowRequest::SetBounds(frame) => WireCommand::Request {
            window_id,
            action: "setBounds",
            mode: None,
            x: frame.x.map(|p| p.0),
            y: frame.y.map(|p| p.0),
            width: frame.width.map(|p| p.0),
            height: frame.height.map(|p| p.0),
            display: None,
            critical: None,
        },
        WindowRequest::SetContentSize { width, height } => WireCommand::Request {
            window_id,
            action: "setContentSize",
            mode: None,
            x: None,
            y: None,
            width: width.map(|p| p.0),
            height: height.map(|p| p.0),
            display: None,
            critical: None,
        },
        WindowRequest::SetPlacement(placement) => WireCommand::Request {
            window_id,
            action: "setPlacement",
            mode: Some(mode_name(placement.mode)),
            x: Some(placement.bounds.origin.x.0),
            y: Some(placement.bounds.origin.y.0),
            width: Some(placement.bounds.size.width.0),
            height: Some(placement.bounds.size.height.0),
            display: None,
            critical: None,
        },
        WindowRequest::Center { display } => WireCommand::Request {
            window_id,
            action: "center",
            mode: None,
            x: None,
            y: None,
            width: None,
            height: None,
            display: display.map(|id| format!("{:016x}", id.0)),
            critical: None,
        },
        WindowRequest::RequestAttention { critical } => WireCommand::Request {
            window_id,
            action: "requestAttention",
            mode: None,
            x: None,
            y: None,
            width: None,
            height: None,
            display: None,
            critical: Some(*critical),
        },
    }
}

/// Wire snapshot of a main window state reported by `NativeDesktop.ets`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct WireWindowState {
    #[serde(default)]
    pub mode: String,
    #[serde(default)]
    pub minimized: bool,
    #[serde(default = "default_true")]
    pub visible: bool,
    #[serde(default = "default_true")]
    pub focused: bool,
    pub bounds: WireBounds,
    #[serde(default)]
    pub content_bounds: Option<WireBounds>,
    #[serde(default)]
    pub normal_bounds: Option<WireBounds>,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default = "default_scale")]
    pub scale_factor: f32,
}

fn default_scale() -> f32 {
    1.0
}

impl WireWindowState {
    pub(crate) fn into_window_state(self) -> WindowState {
        let mode = parse_mode(&self.mode);
        let bounds: Bounds<Points> = self.bounds.into();
        let content_bounds = self.content_bounds.map(Into::into).unwrap_or(bounds);
        let normal_bounds = self.normal_bounds.map(Into::into).unwrap_or(bounds);
        let placement = WindowPlacement::new(mode, normal_bounds);
        let mut state = WindowState::new(bounds, placement);
        state.minimized = self.minimized;
        state.visible = self.visible && !self.minimized;
        state.focused = self.focused && state.visible;
        state.content_bounds = content_bounds;
        state.display = self
            .display_name
            .as_deref()
            .filter(|name| !name.is_empty())
            .map(|name| DisplayId::from_name(name.as_bytes()));
        state.scale_factor = if self.scale_factor.is_finite() && self.scale_factor > 0.0 {
            self.scale_factor
        } else {
            1.0
        };
        state
    }
}

/// Wire snapshot of a display reported by `NativeDesktop.ets`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct WireDisplay {
    pub id_name: String,
    #[serde(default)]
    pub name: String,
    pub bounds: WireBounds,
    #[serde(default)]
    pub visible_bounds: Option<WireBounds>,
    #[serde(default)]
    pub primary: bool,
    #[serde(default = "default_scale")]
    pub scale_factor: f32,
    #[serde(default)]
    pub refresh_rate: Option<f32>,
}

impl WireDisplay {
    pub(crate) fn into_display(self) -> Display {
        let id = DisplayId::from_name(self.id_name.as_bytes());
        let bounds: Bounds<Points> = self.bounds.into();
        let visible_bounds = self.visible_bounds.map(Into::into).unwrap_or(bounds);
        let mut display = Display::new(id, bounds, visible_bounds, self.primary);
        display.name = self.name;
        display.scale_factor = if self.scale_factor.is_finite() && self.scale_factor > 0.0 {
            self.scale_factor
        } else {
            1.0
        };
        display.refresh_rate = self.refresh_rate.filter(|hz| hz.is_finite() && *hz > 0.0);
        display
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct WireTitleBar {
    #[serde(default)]
    pub top: f32,
    #[serde(default)]
    pub right: f32,
    #[serde(default)]
    pub bottom: f32,
    #[serde(default)]
    pub left: f32,
}

impl From<WireTitleBar> for Edges<Points> {
    fn from(t: WireTitleBar) -> Self {
        let clamp = |v: f32| Points(if v.is_finite() { v.max(0.0) } else { 0.0 });
        Edges {
            top: clamp(t.top),
            right: clamp(t.right),
            bottom: clamp(t.bottom),
            left: clamp(t.left),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireMenuSelect {
    generation: u32,
    #[serde(default)]
    id: u32,
    #[serde(default)]
    role: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireMenuClose {
    generation: u32,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireClosePress {
    #[serde(default)]
    reason: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireGlobalShortcutRegistered {
    id: u32,
    registered: bool,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireGlobalShortcutTrigger {
    id: u32,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireTrayClick {
    id: u32,
    #[serde(default)]
    bounds: Option<WireBounds>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireSecondInstance {
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    cwd: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireSubWindowState {
    window_id: u32,
    state: WireWindowState,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireSubWindowTitleBar {
    window_id: u32,
    #[serde(default)]
    top: f32,
    #[serde(default)]
    right: f32,
    #[serde(default)]
    bottom: f32,
    #[serde(default)]
    left: f32,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireSubWindowClosePress {
    window_id: u32,
    #[serde(default)]
    reason: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireSubWindowId {
    window_id: u32,
}

/// Parsed event sent from `NativeDesktop.ets` into Rust.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum DesktopEvent {
    WindowState(WindowState),
    TitleBar(Edges<Points>),
    Displays(Vec<Display>),
    ClosePress(CloseReason),
    SubWindowState {
        window_id: u32,
        state: WindowState,
    },
    SubWindowTitleBar {
        window_id: u32,
        edges: Edges<Points>,
    },
    SubWindowClosePress {
        window_id: u32,
        reason: CloseReason,
    },
    SubWindowBlur {
        window_id: u32,
    },
    SubWindowDestroyed {
        window_id: u32,
    },
    MenuSelect {
        generation: u32,
        id: u32,
        role: Option<MenuRole>,
    },
    MenuClose {
        generation: u32,
    },
    GlobalShortcutRegistered {
        id: u32,
        registered: bool,
    },
    GlobalShortcutTrigger {
        id: u32,
    },
    TrayClick {
        id: u32,
        bounds: Option<Bounds<Points>>,
    },
    SecondInstance(SecondInstance),
}

fn parse_close_reason(reason: &str) -> CloseReason {
    match reason {
        "shortcut" => CloseReason::Shortcut,
        "menu" => CloseReason::Menu,
        "taskbar" => CloseReason::Taskbar,
        "system" => CloseReason::System,
        _ => CloseReason::CloseButton,
    }
}

pub(crate) fn parse_desktop_event(kind: &str, payload: &str) -> Option<DesktopEvent> {
    match kind {
        "window-state" => serde_json::from_str::<WireWindowState>(payload)
            .ok()
            .map(|state| DesktopEvent::WindowState(state.into_window_state())),
        "title-bar" => serde_json::from_str::<WireTitleBar>(payload)
            .ok()
            .map(|edges| DesktopEvent::TitleBar(edges.into())),
        "displays" => serde_json::from_str::<Vec<WireDisplay>>(payload)
            .ok()
            .map(|list| {
                DesktopEvent::Displays(list.into_iter().map(WireDisplay::into_display).collect())
            }),
        "close-press" => {
            let parsed = serde_json::from_str::<WireClosePress>(payload).ok();
            let reason = parsed
                .as_ref()
                .map(|p| parse_close_reason(&p.reason))
                .unwrap_or(CloseReason::CloseButton);
            Some(DesktopEvent::ClosePress(reason))
        }
        "sub-window-state" => serde_json::from_str::<WireSubWindowState>(payload)
            .ok()
            .map(|msg| DesktopEvent::SubWindowState {
                window_id: msg.window_id,
                state: msg.state.into_window_state(),
            }),
        "sub-window-title-bar" => serde_json::from_str::<WireSubWindowTitleBar>(payload)
            .ok()
            .map(|msg| DesktopEvent::SubWindowTitleBar {
                window_id: msg.window_id,
                edges: WireTitleBar {
                    top: msg.top,
                    right: msg.right,
                    bottom: msg.bottom,
                    left: msg.left,
                }
                .into(),
            }),
        "sub-window-close-press" => serde_json::from_str::<WireSubWindowClosePress>(payload)
            .ok()
            .map(|msg| DesktopEvent::SubWindowClosePress {
                window_id: msg.window_id,
                reason: parse_close_reason(&msg.reason),
            }),
        "sub-window-blur" => serde_json::from_str::<WireSubWindowId>(payload)
            .ok()
            .map(|msg| DesktopEvent::SubWindowBlur {
                window_id: msg.window_id,
            }),
        "sub-window-destroyed" => {
            serde_json::from_str::<WireSubWindowId>(payload)
                .ok()
                .map(|msg| DesktopEvent::SubWindowDestroyed {
                    window_id: msg.window_id,
                })
        }
        "menu-select" => serde_json::from_str::<WireMenuSelect>(payload)
            .ok()
            .map(|select| DesktopEvent::MenuSelect {
                generation: select.generation,
                id: select.id,
                role: select.role.as_deref().and_then(parse_role),
            }),
        "menu-close" => serde_json::from_str::<WireMenuClose>(payload)
            .ok()
            .map(|close| DesktopEvent::MenuClose {
                generation: close.generation,
            }),
        "global-shortcut-registered" => {
            serde_json::from_str::<WireGlobalShortcutRegistered>(payload)
                .ok()
                .map(|msg| DesktopEvent::GlobalShortcutRegistered {
                    id: msg.id,
                    registered: msg.registered,
                })
        }
        "global-shortcut-trigger" => serde_json::from_str::<WireGlobalShortcutTrigger>(payload)
            .ok()
            .map(|msg| DesktopEvent::GlobalShortcutTrigger { id: msg.id }),
        "tray-click" => serde_json::from_str::<WireTrayClick>(payload)
            .ok()
            .map(|msg| DesktopEvent::TrayClick {
                id: msg.id,
                bounds: msg.bounds.map(Into::into),
            }),
        "second-instance" => serde_json::from_str::<WireSecondInstance>(payload)
            .ok()
            .map(|msg| DesktopEvent::SecondInstance(SecondInstance::new(msg.args, msg.cwd))),
        _ => None,
    }
}

/// Builds the `EventKind::Menu` payload when a menu action or global shortcut fires.
pub(crate) fn menu_select_payload(id: u32) -> (EventKind, EventPayload) {
    (
        EventKind::Menu,
        EventPayload::Map(vec![("id", PayloadValue::Number(f64::from(id)))]),
    )
}

/// Builds the `EventKind::Menu` payload answering global shortcut registration.
pub(crate) fn global_shortcut_registered_payload(
    id: u32,
    registered: bool,
) -> (EventKind, EventPayload) {
    (
        EventKind::Menu,
        EventPayload::Map(vec![
            ("id", PayloadValue::Number(f64::from(id))),
            ("registered", PayloadValue::Bool(registered)),
        ]),
    )
}

/// Builds the `EventKind::Menu` payload when the tray icon is clicked.
pub(crate) fn tray_click_payload(
    id: u32,
    bounds: Option<Bounds<Points>>,
) -> (EventKind, EventPayload) {
    let mut fields = vec![("id", PayloadValue::Number(f64::from(id)))];
    if let Some(bounds) = bounds {
        fields.push(("bounds", PayloadValue::Bounds(bounds)));
    }
    (EventKind::Menu, EventPayload::Map(fields))
}

/// Builds the `EventKind::Menu` payload when a popup menu is dismissed without a selection.
pub(crate) fn menu_closed_payload() -> (EventKind, EventPayload) {
    (
        EventKind::Menu,
        EventPayload::Map(vec![
            ("id", PayloadValue::Number(0.0)),
            ("closed", PayloadValue::Bool(true)),
        ]),
    )
}

/// Converts a desktop `Shortcut` into HarmonyOS `inputConsumer.HotkeyOptions` `(preKeys, finalKey)`.
/// `inputConsumer` requires 1..=4 modifiers from Ctrl/Alt/Shift and a non-modifier `finalKey`.
pub(crate) fn ohos_hotkey(shortcut: Shortcut) -> Option<(Vec<i32>, i32)> {
    if shortcut.meta {
        return None;
    }
    let mut pre_keys = Vec::with_capacity(3);
    if shortcut.ctrl {
        pre_keys.push(2072); // KEYCODE_CTRL_LEFT
    }
    if shortcut.alt {
        pre_keys.push(2045); // KEYCODE_ALT_LEFT
    }
    if shortcut.shift {
        pre_keys.push(2047); // KEYCODE_SHIFT_LEFT
    }
    if pre_keys.is_empty() {
        return None;
    }
    let final_key = match shortcut.key {
        DesktopKey::Char(c) => match c.to_ascii_lowercase() {
            '0'..='9' => 2000 + (c as i32 - '0' as i32),
            'a'..='z' => 2017 + (c.to_ascii_lowercase() as i32 - 'a' as i32),
            ',' => 2043,
            '.' => 2044,
            ' ' => 2050,
            '`' => 2056,
            '-' => 2057,
            '=' => 2058,
            '[' => 2059,
            ']' => 2060,
            '\\' => 2061,
            ';' => 2062,
            '\'' => 2063,
            '/' => 2064,
            _ => return None,
        },
        DesktopKey::Up => 2012,
        DesktopKey::Down => 2013,
        DesktopKey::Left => 2014,
        DesktopKey::Right => 2015,
        DesktopKey::Tab => 2049,
        DesktopKey::Space => 2050,
        DesktopKey::Enter => 2054,
        DesktopKey::Backspace => 2055,
        DesktopKey::PageUp => 2068,
        DesktopKey::PageDown => 2069,
        DesktopKey::Escape => 2070,
        DesktopKey::Delete => 2071,
        DesktopKey::Home => 2081,
        DesktopKey::End => 2082,
        DesktopKey::Insert => 2083,
        DesktopKey::F(n @ 1..=12) => 2090 + i32::from(n - 1),
        DesktopKey::F(n @ 13..=24) => 2816 + i32::from(n - 13),
        DesktopKey::F(_) => return None,
    };
    Some((pre_keys, final_key))
}

/// Concatenates tray title runs into plain text for platforms that display a single label.
pub(crate) fn tray_title_text(runs: &[nsh_core::desktop::TrayTitleRun]) -> String {
    let mut out = String::new();
    for run in runs {
        out.push_str(&run.text);
    }
    out
}

/// Encodes a tray icon `ImageSource` into a wire payload for `NativeDesktop.ets`.
/// SVGs are rasterized as 48×48 premultiplied RGBA white/black templates so the status bar
/// icon stays crisp against both dark and light desktop wallpapers.
#[cfg(target_env = "ohos")]
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

/// Maps a `DesktopCommand` targeted at the main window (`ROOT_ID`) to a wire command when applicable.
#[allow(dead_code)]
pub(crate) fn main_window_wire_command(command: &DesktopCommand) -> Option<WireCommand> {
    match command {
        DesktopCommand::SetAttributes(attrs) => Some(wire_attributes(attrs)),
        DesktopCommand::Request(req) => Some(wire_request(req)),
        DesktopCommand::Quit => Some(WireCommand::Quit),
        DesktopCommand::Restart => Some(WireCommand::Restart),
        _ => None,
    }
}

pub(crate) fn resize_edge_cursor(edge: ResizeEdge) -> nsh_core::Cursor {
    match edge {
        ResizeEdge::Top | ResizeEdge::Bottom => nsh_core::Cursor::RowResize,
        ResizeEdge::Left | ResizeEdge::Right => nsh_core::Cursor::ColResize,
        ResizeEdge::TopLeft | ResizeEdge::BottomRight => nsh_core::Cursor::NwseResize,
        ResizeEdge::TopRight | ResizeEdge::BottomLeft => nsh_core::Cursor::NeswResize,
    }
}

pub(crate) fn apply_resize_delta(
    edge: ResizeEdge,
    origin: (f32, f32),
    size: (f32, f32),
    dx: f32,
    dy: f32,
    min_size: (f32, f32),
    max_size: (Option<f32>, Option<f32>),
) -> ((f32, f32), (f32, f32)) {
    let (mut x, mut y) = origin;
    let (mut w, mut h) = size;
    let min_w = min_size.0.max(1.0);
    let min_h = min_size.1.max(1.0);
    let max_w = max_size.0.map_or(f32::INFINITY, |v| v.max(min_w));
    let max_h = max_size.1.map_or(f32::INFINITY, |v| v.max(min_h));

    match edge {
        ResizeEdge::Right | ResizeEdge::TopRight | ResizeEdge::BottomRight => {
            w = (w + dx).clamp(min_w, max_w);
        }
        ResizeEdge::Left | ResizeEdge::TopLeft | ResizeEdge::BottomLeft => {
            let right = x + w;
            let next_w = (w - dx).clamp(min_w, max_w);
            x = right - next_w;
            w = next_w;
        }
        ResizeEdge::Top | ResizeEdge::Bottom => {}
    }

    match edge {
        ResizeEdge::Bottom | ResizeEdge::BottomRight | ResizeEdge::BottomLeft => {
            h = (h + dy).clamp(min_h, max_h);
        }
        ResizeEdge::Top | ResizeEdge::TopRight | ResizeEdge::TopLeft => {
            let bottom = y + h;
            let next_h = (h - dy).clamp(min_h, max_h);
            y = (bottom - next_h).max(0.0);
            h = (bottom - y).clamp(min_h, max_h);
        }
        ResizeEdge::Left | ResizeEdge::Right => {}
    }

    ((x, y), (w, h))
}

pub(crate) fn resize_edge(
    x: f32,
    y: f32,
    width: f32,
    height: f32,
    margin: f32,
) -> Option<ResizeEdge> {
    if !x.is_finite()
        || !y.is_finite()
        || width <= margin * 2.0
        || height <= margin * 2.0
        || x < 0.0
        || y < 0.0
        || x > width
        || y > height
    {
        return None;
    }
    let left = x <= margin;
    let right = x >= width - margin;
    let top = y <= margin;
    let bottom = y >= height - margin;
    match (left, right, top, bottom) {
        (true, _, true, _) => Some(ResizeEdge::TopLeft),
        (_, true, true, _) => Some(ResizeEdge::TopRight),
        (true, _, _, true) => Some(ResizeEdge::BottomLeft),
        (_, true, _, true) => Some(ResizeEdge::BottomRight),
        (true, _, _, _) => Some(ResizeEdge::Left),
        (_, true, _, _) => Some(ResizeEdge::Right),
        (_, _, true, _) => Some(ResizeEdge::Top),
        (_, _, _, true) => Some(ResizeEdge::Bottom),
        _ => None,
    }
}

/// Converts ArkUI axis values into NSH `WheelDelta` (positive `y` scrolls down,
/// positive `x` scrolls right). Mouse wheels report degrees (15° per line) and
/// map Shift+vertical wheel to horizontal lines; touchpads report physical px.
pub(crate) fn wheel_delta_from_axis(
    vx: f64,
    vy: f64,
    is_mouse: bool,
    scale: f32,
    shift: bool,
) -> Option<nsh_core::WheelDelta> {
    let raw_x = if vx.is_finite() { vx as f32 } else { 0.0 };
    let raw_y = if vy.is_finite() { vy as f32 } else { 0.0 };
    if raw_x == 0.0 && raw_y == 0.0 {
        return None;
    }
    if is_mouse {
        let mut lx = raw_x / 15.0;
        let mut ly = raw_y / 15.0;
        if shift && lx == 0.0 && ly != 0.0 {
            lx = ly;
            ly = 0.0;
        }
        Some(nsh_core::WheelDelta::Lines(Point::new(lx, ly)))
    } else {
        let scale = scale.max(1.0);
        Some(nsh_core::WheelDelta::Points(Point::new(
            Points(raw_x / scale),
            Points(raw_y / scale),
        )))
    }
}

/// Converts a vertical mouse-wheel delta (in lines, negative = scroll up) into
/// an NSH `PinchEvent::delta` fraction matching `nsh_core::input::zoom_factor`.
pub(crate) fn ctrl_wheel_pinch_delta(dy_lines: f32) -> f32 {
    if !dy_lines.is_finite() || dy_lines == 0.0 {
        return 0.0;
    }
    nsh_core::input::zoom_factor(dy_lines) - 1.0
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub(crate) struct MainWindowState {
    pub(crate) movable: bool,
    pub(crate) maximizable: bool,
    pub(crate) resizable: bool,
    pub(crate) frame_none: bool,
    pub(crate) min_size: Option<Size<Points>>,
    pub(crate) max_size: Option<Size<Points>>,
    pub(crate) size_to_content: SizeToContent,
    pub(crate) backdrop: Backdrop,
}

impl Default for MainWindowState {
    fn default() -> Self {
        Self {
            movable: true,
            maximizable: true,
            resizable: true,
            frame_none: false,
            min_size: None,
            max_size: None,
            size_to_content: SizeToContent::Off,
            backdrop: Backdrop::None,
        }
    }
}

#[allow(dead_code)]
impl MainWindowState {
    pub(crate) fn apply_attributes(&mut self, attrs: &WindowAttributes) {
        if let Some(movable) = attrs.movable {
            self.movable = movable;
        }
        if let Some(maximizable) = attrs.maximizable {
            self.maximizable = maximizable;
        }
        if let Some(resizable) = attrs.resizable {
            self.resizable = resizable;
        }
        if let Some(frame) = attrs.frame {
            self.frame_none = frame == WindowFrameStyle::None;
        }
        if let Some(min_size) = attrs.min_size {
            self.min_size = min_size;
        }
        if let Some(max_size) = attrs.max_size {
            self.max_size = max_size;
        }
        if let Some(size_to_content) = attrs.size_to_content {
            self.size_to_content = size_to_content;
        }
        if let Some(backdrop) = attrs.backdrop {
            self.backdrop = backdrop;
        }
    }

    pub(crate) fn can_resize(&self, mode: WindowMode) -> bool {
        mode == WindowMode::Normal
            && self.resizable
            && matches!(self.size_to_content, SizeToContent::Off)
    }
}

#[cfg(target_env = "ohos")]
pub(crate) struct DesktopState {
    pub(crate) labels: MenuLabels,
    pub(crate) quit_on_request: bool,
    pub(crate) popup_generation: u32,
    pub(crate) popup_open: bool,
    pub(crate) close_on_press: bool,
    pub(crate) selection_menu_target: Option<nsh_core::NodeId>,
    /// Primary desktop window node hosted in-place inside EntryAbility's main window
    /// under NSH window model v2 (plain TSX implicit `"main"` or primary `<Window>`).
    pub(crate) hosted_main_window: Option<nsh_core::NodeId>,
    pub(crate) main_window_launch_applied: bool,
}

#[cfg(target_env = "ohos")]
impl Default for DesktopState {
    fn default() -> Self {
        Self {
            labels: MenuLabels::default(),
            quit_on_request: true,
            popup_generation: 0,
            popup_open: false,
            close_on_press: true,
            selection_menu_target: None,
            hosted_main_window: None,
            main_window_launch_applied: false,
        }
    }
}

#[cfg(target_env = "ohos")]
impl DesktopState {
    pub(crate) fn next_popup_generation(&mut self) -> u32 {
        self.popup_generation = self.popup_generation.wrapping_add(1).max(1);
        self.popup_open = true;
        self.popup_generation
    }

    pub(crate) fn next_selection_popup_generation(&mut self) -> u32 {
        self.popup_generation = self.popup_generation.wrapping_add(1).max(1);
        self.popup_open = false;
        self.popup_generation
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nsh_core::{
        desktop::{MenuAction, TitleBarOptions},
        keymap::{Modifiers, When},
    };

    #[test]
    fn desktop_idiom_maps_to_linux_without_native_header() {
        assert_eq!(os_for_idiom(Idiom::Desktop), Os::Linux);
        assert_eq!(os_for_idiom(Idiom::Phone), Os::Android);
        assert_eq!(os_for_idiom(Idiom::Pad), Os::Android);
        assert_eq!(os_for_idiom(Idiom::Tv), Os::Android);

        assert_eq!(header_heights_for_os(Os::Linux), nsh_core::NO_NATIVE_HEADER);
        assert_eq!(
            header_heights_for_os(Os::Android),
            (Points(52.0), Points(52.0))
        );
    }

    #[test]
    fn inline_sections_expand_and_format_linux_shortcuts() {
        let action = |id: u32, label: &str, shortcut: Option<&str>, checked: Option<bool>| {
            MenuItem::Action(MenuAction {
                id,
                label: label.into(),
                shortcut: shortcut.map(|s| Shortcut::parse(s, false).unwrap()),
                enabled: true,
                checked,
                role: None,
                when: When::Always,
                icon: None,
                symbol: None,
                destructive: false,
            })
        };
        let items = [
            MenuItem::Submenu {
                label: "Sort by".into(),
                enabled: true,
                inline: true,
                icon: None,
                symbol: None,
                items: vec![
                    action(1, "Name", None, Some(true)),
                    action(2, "Date", None, Some(false)),
                ],
            },
            action(3, "Share", Some("mod+shift+s"), None),
            MenuItem::Submenu {
                label: "More".into(),
                enabled: true,
                inline: false,
                icon: None,
                symbol: None,
                items: vec![MenuItem::Action(MenuAction {
                    id: 0,
                    label: String::new(),
                    shortcut: None,
                    enabled: true,
                    checked: None,
                    role: Some(MenuRole::Copy),
                    when: When::Always,
                    icon: None,
                    symbol: None,
                    destructive: false,
                })],
            },
        ];
        let wire = serialize_menu_items(&items, &MenuLabels::default());
        assert_eq!(wire.len(), 5);
        assert_eq!(wire[0].label, "Name");
        assert_eq!(wire[0].checked, Some(true));
        assert_eq!(wire[1].label, "Date");
        assert_eq!(wire[2].kind, "separator");
        assert_eq!(wire[3].label, "Share");
        assert_eq!(wire[3].shortcut.as_deref(), Some("Ctrl+Shift+S"));
        assert_eq!(wire[4].kind, "submenu");
        assert_eq!(wire[4].items[0].label, "Copy");
        assert_eq!(wire[4].items[0].shortcut.as_deref(), Some("Ctrl+C"));
        assert_eq!(wire[4].items[0].role.as_deref(), Some("copy"));
    }

    #[test]
    fn desktop_events_round_trip_window_state_displays_and_title_bar() {
        let state_event = parse_desktop_event(
            "window-state",
            r#"{"mode":"maximized","minimized":false,"visible":true,"focused":true,"bounds":{"x":10,"y":20,"width":1200,"height":800},"displayName":"display-0","scaleFactor":2.0}"#,
        )
        .unwrap();
        let DesktopEvent::WindowState(state) = state_event else {
            panic!("expected window state");
        };
        assert_eq!(state.mode, WindowMode::Maximized);
        assert!(state.visible && state.focused && !state.minimized);
        assert_eq!(state.bounds.origin.x, Points(10.0));
        assert_eq!(state.bounds.size.width, Points(1200.0));
        assert_eq!(state.display, Some(DisplayId::from_name(b"display-0")));
        assert_eq!(state.scale_factor, 2.0);

        let bar_event =
            parse_desktop_event("title-bar", r#"{"top":44,"right":136,"left":0,"bottom":0}"#)
                .unwrap();
        assert_eq!(
            bar_event,
            DesktopEvent::TitleBar(Edges {
                top: Points(44.0),
                right: Points(136.0),
                bottom: Points::ZERO,
                left: Points::ZERO,
            })
        );

        let displays_event = parse_desktop_event(
            "displays",
            r#"[{"idName":"display-0","name":"Built-in Display","bounds":{"x":0,"y":0,"width":1560,"height":1040},"primary":true,"scaleFactor":2.0,"refreshRate":60.0}]"#,
        )
        .unwrap();
        let DesktopEvent::Displays(list) = displays_event else {
            panic!("expected displays");
        };
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, DisplayId::from_name(b"display-0"));
        assert_eq!(list[0].name, "Built-in Display");
        assert!(list[0].primary);
        assert_eq!(list[0].refresh_rate, Some(60.0));
    }

    #[test]
    fn wire_attributes_serializes_hidden_title_bar_and_shadow() {
        let mut attrs = WindowAttributes::default();
        attrs.title = Some("Gallery".into());
        attrs.shadow = Some(false);
        attrs.movable = Some(false);
        attrs.resizable = Some(false);
        let mut title_bar = TitleBarOptions::default();
        title_bar.style = TitleBarStyle::Hidden;
        title_bar.height = Some(Points(44.0));
        attrs.title_bar = Some(title_bar);
        let json = serde_json::to_value(wire_attributes(&attrs)).unwrap();
        assert_eq!(json["op"], "setAttributes");
        assert_eq!(json["title"], "Gallery");
        assert_eq!(json["titleBarStyle"], "hidden");
        assert_eq!(json["titleBarHeight"], 44.0);
        assert_eq!(json["shadow"], false);
        assert_eq!(json["movable"], false);
        assert_eq!(json["resizable"], false);
        assert_eq!(
            main_window_wire_command(&DesktopCommand::Restart),
            Some(WireCommand::Restart)
        );
    }

    #[test]
    fn keystroke_maps_to_desktop_shortcut() {
        let stroke = Keystroke {
            key: KeymapKey::Char('q'),
            modifiers: Modifiers {
                ctrl: true,
                ..Modifiers::default()
            },
        };
        assert_eq!(
            shortcut_from_keystroke(stroke),
            Some(Shortcut::parse("ctrl+q", false).unwrap())
        );
    }

    #[test]
    fn ohos_hotkey_maps_modifiers_letters_digits_and_function_keys() {
        let s = Shortcut::parse("ctrl+alt+k", false).unwrap();
        assert_eq!(ohos_hotkey(s), Some((vec![2072, 2045], 2027)));

        let f12 = Shortcut::parse("ctrl+shift+f12", false).unwrap();
        assert_eq!(ohos_hotkey(f12), Some((vec![2072, 2047], 2101)));

        let f13 = Shortcut::parse("alt+f13", false).unwrap();
        assert_eq!(ohos_hotkey(f13), Some((vec![2045], 2816)));

        let digit = Shortcut::parse("ctrl+9", false).unwrap();
        assert_eq!(ohos_hotkey(digit), Some((vec![2072], 2009)));

        // Unmodified or Meta shortcuts are rejected by OHOS inputConsumer.
        assert_eq!(ohos_hotkey(Shortcut::parse("k", false).unwrap()), None);
        assert_eq!(ohos_hotkey(Shortcut::parse("cmd+k", true).unwrap()), None);
    }

    #[test]
    fn desktop_events_parse_global_shortcut_tray_and_second_instance() {
        assert_eq!(
            parse_desktop_event(
                "global-shortcut-registered",
                r#"{"id":101,"registered":true}"#
            ),
            Some(DesktopEvent::GlobalShortcutRegistered {
                id: 101,
                registered: true,
            })
        );
        assert_eq!(
            parse_desktop_event("global-shortcut-trigger", r#"{"id":101}"#),
            Some(DesktopEvent::GlobalShortcutTrigger { id: 101 })
        );
        assert_eq!(
            parse_desktop_event("tray-click", r#"{"id":42}"#),
            Some(DesktopEvent::TrayClick {
                id: 42,
                bounds: None,
            })
        );
        assert_eq!(
            parse_desktop_event(
                "tray-click",
                r#"{"id":42,"bounds":{"x":1200,"y":0,"width":24,"height":24}}"#
            ),
            Some(DesktopEvent::TrayClick {
                id: 42,
                bounds: Some(Bounds::new(
                    Point::new(Points(1200.0), Points(0.0)),
                    Size::new(Points(24.0), Points(24.0)),
                )),
            })
        );
        assert_eq!(
            parse_desktop_event(
                "second-instance",
                r#"{"args":["nsh://demo","--open"],"cwd":"/data"}"#
            ),
            Some(DesktopEvent::SecondInstance(SecondInstance::new(
                vec!["nsh://demo".into(), "--open".into()],
                "/data",
            )))
        );
    }

    #[test]
    fn resize_edge_detects_edges_corners_and_cursors() {
        assert_eq!(
            resize_edge(2.0, 2.0, 400.0, 300.0, 6.0),
            Some(ResizeEdge::TopLeft)
        );
        assert_eq!(
            resize_edge(398.0, 3.0, 400.0, 300.0, 6.0),
            Some(ResizeEdge::TopRight)
        );
        assert_eq!(
            resize_edge(3.0, 297.0, 400.0, 300.0, 6.0),
            Some(ResizeEdge::BottomLeft)
        );
        assert_eq!(
            resize_edge(396.0, 296.0, 400.0, 300.0, 6.0),
            Some(ResizeEdge::BottomRight)
        );
        assert_eq!(
            resize_edge(200.0, 2.0, 400.0, 300.0, 6.0),
            Some(ResizeEdge::Top)
        );
        assert_eq!(
            resize_edge(200.0, 298.0, 400.0, 300.0, 6.0),
            Some(ResizeEdge::Bottom)
        );
        assert_eq!(
            resize_edge(2.0, 150.0, 400.0, 300.0, 6.0),
            Some(ResizeEdge::Left)
        );
        assert_eq!(
            resize_edge(398.0, 150.0, 400.0, 300.0, 6.0),
            Some(ResizeEdge::Right)
        );
        assert_eq!(resize_edge(200.0, 150.0, 400.0, 300.0, 6.0), None);
        assert_eq!(
            resize_edge_cursor(ResizeEdge::Right),
            nsh_core::Cursor::ColResize
        );
        assert_eq!(
            resize_edge_cursor(ResizeEdge::Bottom),
            nsh_core::Cursor::RowResize
        );
        assert_eq!(
            resize_edge_cursor(ResizeEdge::BottomRight),
            nsh_core::Cursor::NwseResize
        );
        assert_eq!(
            resize_edge_cursor(ResizeEdge::TopRight),
            nsh_core::Cursor::NeswResize
        );
    }

    #[test]
    fn resize_edge_apply_delta_clamps_and_anchors_opposite_edge() {
        let (origin, size) = apply_resize_delta(
            ResizeEdge::Left,
            (100.0, 50.0),
            (400.0, 300.0),
            50.0,
            0.0,
            (200.0, 150.0),
            (Some(800.0), Some(600.0)),
        );
        assert_eq!(origin, (150.0, 50.0));
        assert_eq!(size, (350.0, 300.0));

        // Clamping at min_size keeps the right edge anchored at 100 + 400 = 500.
        let (origin, size) = apply_resize_delta(
            ResizeEdge::Left,
            (100.0, 50.0),
            (400.0, 300.0),
            350.0,
            0.0,
            (200.0, 150.0),
            (Some(800.0), Some(600.0)),
        );
        assert_eq!(origin, (300.0, 50.0));
        assert_eq!(size, (200.0, 300.0));

        // BottomRight grows width/height without moving origin, including with no max bound.
        let (origin, size) = apply_resize_delta(
            ResizeEdge::BottomRight,
            (100.0, 50.0),
            (400.0, 300.0),
            40.0,
            -20.0,
            (1.0, 1.0),
            (None, None),
        );
        assert_eq!(origin, (100.0, 50.0));
        assert_eq!(size, (440.0, 280.0));
    }

    #[test]
    fn wheel_delta_handles_mouse_lines_shift_swap_and_touchpad_points() {
        assert_eq!(
            wheel_delta_from_axis(0.0, 45.0, true, 2.0, false),
            Some(nsh_core::WheelDelta::Lines(Point::new(0.0, 3.0)))
        );
        assert_eq!(
            wheel_delta_from_axis(0.0, 45.0, true, 2.0, true),
            Some(nsh_core::WheelDelta::Lines(Point::new(3.0, 0.0)))
        );
        assert_eq!(
            wheel_delta_from_axis(16.0, -24.0, false, 2.0, false),
            Some(nsh_core::WheelDelta::Points(Point::new(
                Points(8.0),
                Points(-12.0),
            )))
        );
        assert_eq!(wheel_delta_from_axis(0.0, 0.0, true, 2.0, false), None);
    }

    #[test]
    fn ctrl_wheel_pinch_delta_matches_zoom_direction() {
        assert!(ctrl_wheel_pinch_delta(-1.0) > 0.0);
        assert!(ctrl_wheel_pinch_delta(1.0) < 0.0);
        assert_eq!(ctrl_wheel_pinch_delta(0.0), 0.0);
    }

    #[test]
    fn wire_open_sub_window_and_scoped_commands_serialize_window_id() {
        let mut attrs = WindowAttributes::default();
        attrs.title = Some("Inspector".into());
        attrs.transparent = Some(true);
        attrs.close_on_blur = Some(true);
        attrs.activate = Some(false);
        attrs.frame = Some(WindowFrameStyle::None);
        attrs.level = Some(WindowLevel::Floating);
        attrs.backdrop = Some(Backdrop::Sidebar);

        let mut options = nsh_core::desktop::WindowOptions::default();
        options.kind = WindowKind::Panel;
        options.modal = false;
        options.default_position = Some(WindowPosition::ParentCenter);
        options.default_size = Some(Size::new(Points(320.0), Points(440.0)));
        options.default_mode = Some(WindowMode::Normal);
        options.attributes = attrs.clone();

        let mut open = OpenWindow::default();
        open.options = options;
        open.parent = Some(7);
        let open_json =
            serde_json::to_value(wire_open_sub_window(42, &open, (320.0, 440.0))).unwrap();
        assert_eq!(open_json["op"], "openSubWindow");
        assert_eq!(open_json["windowId"], 42);
        assert_eq!(open_json["kind"], "panel");
        assert_eq!(open_json["modal"], false);
        assert_eq!(open_json["parentWindowId"], 7);
        assert_eq!(open_json["width"], 320.0);
        assert_eq!(open_json["height"], 440.0);
        assert_eq!(open_json["position"], "parentCenter");
        assert_eq!(open_json["title"], "Inspector");
        assert_eq!(open_json["transparent"], true);
        assert_eq!(open_json["closeOnBlur"], true);
        assert_eq!(open_json["activate"], false);
        assert_eq!(open_json["frame"], "none");
        assert_eq!(open_json["level"], "floating");
        assert_eq!(open_json["backdrop"], "sidebar");

        let attr_json = serde_json::to_value(wire_attributes_for(Some(42), &attrs)).unwrap();
        assert_eq!(attr_json["op"], "setAttributes");
        assert_eq!(attr_json["windowId"], 42);
        assert_eq!(attr_json["closeOnBlur"], true);
        assert_eq!(attr_json["backdrop"], "sidebar");

        let req_json = serde_json::to_value(wire_request_for(
            Some(42),
            &WindowRequest::SetContentSize {
                width: Some(Points(320.0)),
                height: Some(Points(280.0)),
            },
        ))
        .unwrap();
        assert_eq!(req_json["op"], "request");
        assert_eq!(req_json["windowId"], 42);
        assert_eq!(req_json["action"], "setContentSize");
        assert_eq!(req_json["width"], 320.0);
        assert_eq!(req_json["height"], 280.0);

        let display_id = DisplayId::from_name(b"display-1");
        let mut open_with_display = open.clone();
        open_with_display.options.display = Some(display_id);
        let open_disp_json =
            serde_json::to_value(wire_open_sub_window(43, &open_with_display, (320.0, 440.0)))
                .unwrap();
        assert_eq!(open_disp_json["display"], format!("{:016x}", display_id.0));

        let center_json = serde_json::to_value(wire_request_for(
            Some(42),
            &WindowRequest::Center {
                display: Some(display_id),
            },
        ))
        .unwrap();
        assert_eq!(center_json["op"], "request");
        assert_eq!(center_json["windowId"], 42);
        assert_eq!(center_json["action"], "center");
        assert_eq!(center_json["display"], format!("{:016x}", display_id.0));
    }

    #[test]
    fn main_window_launch_and_state_respect_options_and_kept_placement() {
        let mut options = MainWindowOptions::default();
        // When no launch overrides are configured, let HarmonyOS pick the system default geometry.
        assert_eq!(wire_main_window_launch(&options, None), None);

        options.default_size = Some(Size::new(Points(720.0), Points(500.0)));
        options.attributes.max_size = Some(Some(Size::new(Points(640.0), Points(480.0))));
        let clamped_json =
            serde_json::to_value(wire_main_window_launch(&options, None).unwrap()).unwrap();
        assert_eq!(clamped_json["width"], 640.0);
        assert_eq!(clamped_json["height"], 480.0);
        assert_eq!(clamped_json["contentSized"], true);
        options.attributes.max_size = None;

        options.attributes.min_size = Some(Some(Size::new(Points(800.0), Points(600.0))));
        let min_json =
            serde_json::to_value(wire_main_window_launch(&options, None).unwrap()).unwrap();
        assert_eq!(min_json["width"], 800.0);
        assert_eq!(min_json["height"], 600.0);
        options.attributes.min_size = None;

        options.default_position = Some(WindowPosition::Center);
        options.default_visible = Some(false);
        let cmd = wire_main_window_launch(&options, None).unwrap();
        let json = serde_json::to_value(cmd).unwrap();
        assert_eq!(json["op"], "applyMainWindowLaunch");
        assert_eq!(json["width"], 720.0);
        assert_eq!(json["height"], 500.0);
        assert_eq!(json["contentSized"], true);
        assert_eq!(json["position"], "center");
        assert_eq!(json["visible"], false);

        let mut kept = WindowPlacement::default();
        kept.bounds = Bounds::new(
            Point::new(Points(100.0), Points(120.0)),
            Size::new(Points(900.0), Points(640.0)),
        );
        kept.mode = WindowMode::Maximized;
        let kept_json =
            serde_json::to_value(wire_main_window_launch(&options, Some(kept)).unwrap()).unwrap();
        assert_eq!(kept_json["x"], 100.0);
        assert_eq!(kept_json["y"], 120.0);
        assert_eq!(kept_json["width"], 900.0);
        assert_eq!(kept_json["height"], 640.0);
        assert_eq!(kept_json["contentSized"], false);
        assert_eq!(kept_json["mode"], "maximized");

        // Primary window model v2 OpenWindow converts into main-window launch overrides only when set.
        let mut implicit_open = OpenWindow::default();
        implicit_open.implicit = true;
        assert_eq!(wire_open_main_window_launch(&implicit_open), None);
        implicit_open.options.default_size = Some(Size::new(Points(960.0), Points(640.0)));
        implicit_open.options.default_position = Some(WindowPosition::Center);
        let open_main_json =
            serde_json::to_value(wire_open_main_window_launch(&implicit_open).unwrap()).unwrap();
        assert_eq!(open_main_json["op"], "applyMainWindowLaunch");
        assert_eq!(open_main_json["width"], 960.0);
        assert_eq!(open_main_json["height"], 640.0);
        assert_eq!(open_main_json["position"], "center");

        // Subwindow clamping respects explicit min/max without imposing a 120x80 floor.
        let mut small_attrs = WindowAttributes::default();
        assert_eq!(clamp_window_size((48.0, 32.0), &small_attrs), (48.0, 32.0));
        small_attrs.min_size = Some(Some(Size::new(Points(64.0), Points(40.0))));
        assert_eq!(clamp_window_size((48.0, 32.0), &small_attrs), (64.0, 40.0));

        let mut main_state = MainWindowState::default();
        assert!(main_state.can_resize(WindowMode::Normal));
        let mut attrs = WindowAttributes::default();
        attrs.resizable = Some(false);
        attrs.movable = Some(false);
        attrs.maximizable = Some(false);
        attrs.frame = Some(WindowFrameStyle::None);
        main_state.apply_attributes(&attrs);
        assert!(!main_state.can_resize(WindowMode::Normal));
        assert!(!main_state.movable);
        assert!(!main_state.maximizable);
        assert!(main_state.frame_none);
    }

    #[test]
    fn sub_window_events_parse_round_trip() {
        let state_event = parse_desktop_event(
            "sub-window-state",
            r#"{"windowId":42,"state":{"mode":"normal","minimized":false,"visible":true,"focused":true,"bounds":{"x":120,"y":80,"width":640,"height":520},"contentBounds":{"x":120,"y":124,"width":640,"height":476},"displayName":"display-0","scaleFactor":1.9}}"#,
        )
        .unwrap();
        let DesktopEvent::SubWindowState { window_id, state } = state_event else {
            panic!("expected sub-window-state");
        };
        assert_eq!(window_id, 42);
        assert_eq!(state.bounds.origin.x, Points(120.0));
        assert_eq!(state.content_bounds.size.height, Points(476.0));
        assert!(state.focused && state.visible);

        assert_eq!(
            parse_desktop_event(
                "sub-window-title-bar",
                r#"{"windowId":42,"top":40,"right":120,"bottom":0,"left":0}"#,
            ),
            Some(DesktopEvent::SubWindowTitleBar {
                window_id: 42,
                edges: Edges {
                    top: Points(40.0),
                    right: Points(120.0),
                    bottom: Points::ZERO,
                    left: Points::ZERO,
                },
            })
        );
        assert_eq!(
            parse_desktop_event(
                "sub-window-close-press",
                r#"{"windowId":42,"reason":"closeButton"}"#,
            ),
            Some(DesktopEvent::SubWindowClosePress {
                window_id: 42,
                reason: CloseReason::CloseButton,
            })
        );
        assert_eq!(
            parse_desktop_event("sub-window-blur", r#"{"windowId":42}"#),
            Some(DesktopEvent::SubWindowBlur { window_id: 42 })
        );
        assert_eq!(
            parse_desktop_event("sub-window-destroyed", r#"{"windowId":42}"#),
            Some(DesktopEvent::SubWindowDestroyed { window_id: 42 })
        );
    }
}
