//! The real device's input, and conversion from surface points to screen pixels.

use super::device::Device;
use nsh_core::{
    NodeId,
    geometry::{Bounds, Point, Points, Size},
};
use nsh_dev::{
    DevServer,
    protocol::{SceneQuery, TestStep},
};
use nsh_testing::agent::{Key, Native};
use serde::de::DeserializeOwned;
use std::{
    io::Cursor,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

const ASK: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Geometry {
    pixels: [u32; 4],
    logical: [f32; 2],
    scale: f32,
}

impl Geometry {
    fn from_surface(pixels: [f32; 4], logical: [f32; 2]) -> Result<Self, String> {
        let [left, top, width, height] = pixels;
        if left < 0.0
            || top < 0.0
            || width <= 0.0
            || height <= 0.0
            || logical.iter().any(|v| !v.is_finite() || *v <= 0.0)
        {
            return Err("test surface has no usable screen rectangle".into());
        }
        let scale = width / logical[0];
        let vertical = height / logical[1];
        if !(0.25..=8.0).contains(&scale) || (scale - vertical).abs() > scale * 0.02 {
            return Err("surface changed size while reading device geometry".into());
        }
        Ok(Self {
            pixels: [
                left.round() as u32,
                top.round() as u32,
                width.round() as u32,
                height.round() as u32,
            ],
            logical,
            scale,
        })
    }

    fn point(self, point: Point<Points>) -> (i32, i32) {
        (
            (self.pixels[0] as f32 + point.x.0 * self.scale).round() as i32,
            (self.pixels[1] as f32 + point.y.0 * self.scale).round() as i32,
        )
    }
}

#[derive(Clone)]
pub(super) struct HdcNative {
    device: Arc<Device>,
    server: Arc<DevServer>,
    geometry: Arc<Mutex<Geometry>>,
    error: Arc<Mutex<Option<String>>>,
}

impl HdcNative {
    pub(super) fn new(device: Arc<Device>, server: Arc<DevServer>) -> Self {
        Self {
            device,
            server,
            geometry: Arc::default(),
            error: Arc::default(),
        }
    }

    pub(super) fn refresh(&self) -> Result<(), String> {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match self.read_geometry() {
                Ok(geometry) => {
                    *self.geometry.lock().unwrap_or_else(|e| e.into_inner()) = geometry;
                    return Ok(());
                }
                Err(error)
                    if Instant::now() >= deadline
                        || self
                            .device
                            .cancelled
                            .load(std::sync::atomic::Ordering::Acquire) =>
                {
                    return Err(error);
                }
                Err(_) => std::thread::sleep(Duration::from_millis(25)),
            }
        }
    }

    fn read_geometry(&self) -> Result<Geometry, String> {
        let logical: [f32; 2] = self.query(SceneQuery::Viewport)?;
        let pixels: Option<[f32; 4]> = self.query(SceneQuery::Bounds { id: u32::MAX })?;
        let pixels = pixels.ok_or("test surface is not laid out on screen")?;
        Geometry::from_surface(pixels, logical)
    }

    pub(super) fn take_error(&self) -> Option<String> {
        self.error.lock().unwrap_or_else(|e| e.into_inner()).take()
    }

    fn track<T: Default>(&self, result: Result<T, String>) -> T {
        match result {
            Ok(value) => value,
            Err(error) => {
                self.error
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get_or_insert(error);
                T::default()
            }
        }
    }

    fn query<T: DeserializeOwned>(&self, query: SceneQuery) -> Result<T, String> {
        if self
            .device
            .cancelled
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return Err("test run interrupted".into());
        }
        let value = self.server.step(TestStep::Scene { query }, ASK)?;
        serde_json::from_value(value).map_err(|e| format!("invalid native scene reply: {e}"))
    }

    fn geometry(&self) -> Geometry {
        *self.geometry.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn shell(&self, args: &[&str]) -> Result<(), String> {
        self.device
            .shell(args)
            .map(|_| ())
            .map_err(|e| format!("{e:#}"))
    }

    fn press_at(&self, at: Point<Points>) -> Result<(), String> {
        let (x, y) = self.geometry().point(at);
        self.shell(&["uinput", "-T", "-c", &x.to_string(), &y.to_string()])
    }

    fn bounds_query(&self, query: SceneQuery) -> Result<Option<Bounds<Points>>, String> {
        let bounds: Option<[f32; 4]> = self.query(query)?;
        Ok(bounds.map(|[x, y, w, h]| {
            Bounds::new(
                Point::new(Points(x), Points(y)),
                Size::new(Points(w), Points(h)),
            )
        }))
    }

    fn keys(&self, codes: &[u32]) -> Result<(), String> {
        let mut args = vec!["uinput".to_owned(), "-K".to_owned()];
        for code in codes {
            args.push("-d".into());
            args.push(code.to_string());
        }
        for code in codes.iter().rev() {
            args.push("-u".into());
            args.push(code.to_string());
        }
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        self.shell(&refs)
    }
}

impl Native for HdcNative {
    fn bounds(&self, id: NodeId) -> Option<Bounds<Points>> {
        self.track(self.bounds_query(SceneQuery::Bounds { id }))
    }
    fn text_bounds(&self, id: NodeId, start: u32, end: u32) -> Option<Bounds<Points>> {
        self.track(self.bounds_query(SceneQuery::TextBounds { id, start, end }))
    }
    fn viewport(&self) -> Size<Points> {
        let [w, h] = self.geometry().logical;
        Size::new(Points(w), Points(h))
    }
    fn press(&self, at: Point<Points>) {
        self.track(self.press_at(at));
    }
    fn long_press(&self, at: Point<Points>, hold: Duration) {
        let (x, y) = self.geometry().point(at);
        let (x_s, y_s) = (x.to_string(), y.to_string());
        let result = if hold.as_millis() > 60_000 {
            Err("long presses over 60 seconds are unsupported".into())
        } else {
            self.shell(&[
                "uinput",
                "-T",
                "-d",
                &x_s,
                &y_s,
                "-i",
                &hold.as_millis().max(1).to_string(),
                "-u",
                &x_s,
                &y_s,
            ])
        };
        if result.is_err() {
            self.device.release_touch(x, y);
        }
        self.track(result);
    }
    fn scroll(&self, at: Point<Points>, delta: Point<Points>) {
        let geometry = self.geometry();
        let end = Point::new(
            Points((at.x.0 - delta.x.0).clamp(8.0, (geometry.logical[0] - 8.0).max(8.0))),
            Points((at.y.0 - delta.y.0).clamp(8.0, (geometry.logical[1] - 8.0).max(8.0))),
        );
        let (x, y) = geometry.point(at);
        let (end_x, end_y) = geometry.point(end);
        let result = self.shell(&[
            "uinput",
            "-T",
            "-m",
            &x.to_string(),
            &y.to_string(),
            &end_x.to_string(),
            &end_y.to_string(),
            "-k",
            "120",
            "300",
        ]);
        if result.is_err() {
            self.device.release_touch(end_x, end_y);
        }
        self.track(result);
    }
    fn drag(&self, from: Point<Points>, to: Point<Points>) {
        let (x, y) = self.geometry().point(from);
        let (end_x, end_y) = self.geometry().point(to);
        let result = self.shell(&[
            "uinput",
            "-T",
            "-m",
            &x.to_string(),
            &y.to_string(),
            &end_x.to_string(),
            &end_y.to_string(),
            "-k",
            "80",
            "240",
        ]);
        if result.is_err() {
            self.device.release_touch(end_x, end_y);
        }
        self.track(result);
    }
    fn type_text(&self, text: &str) {
        self.track(
            self.server
                .step(
                    TestStep::Type {
                        selector: String::new(),
                        text: text.to_owned(),
                    },
                    ASK,
                )
                .map(drop),
        );
    }
    fn key(&self, key: Key) {
        let name = match key {
            Key::Return => "return",
            Key::Backspace => "backspace",
            Key::SelectAll => "select-all",
        };
        self.track(
            self.server
                .step(
                    TestStep::PressKey {
                        keys: name.to_owned(),
                    },
                    ASK,
                )
                .map(drop),
        );
    }
    fn press_keys(&self, keys: &str) -> Result<(), String> {
        let codes = key_codes(keys)?;
        self.keys(&codes)
    }
    fn focused(&self) -> Option<NodeId> {
        let text: Option<String> = self.track(self.query(SceneQuery::InputText { id: u32::MAX }));
        text.and_then(|s| s.parse().ok())
    }
    fn paste(&self, content: &nsh_core::PasteEvent) -> Result<(), String> {
        self.server
            .step(
                TestStep::Paste {
                    selector: String::new(),
                    text: content.text.clone(),
                    html: content.html.clone(),
                    files: content
                        .files
                        .iter()
                        .map(|f| {
                            nsh_dev::protocol::TestFile::new(
                                f.name.clone(),
                                f.mime.clone(),
                                f.data.clone(),
                            )
                        })
                        .collect(),
                    uris: content.uris.clone(),
                },
                ASK,
            )
            .map(drop)
    }
    fn input_text(&self, id: NodeId) -> Option<String> {
        self.track(self.query(SceneQuery::InputText { id }))
    }
    fn back(&self) -> Result<(), String> {
        self.shell(&["uinput", "-K", "-d", "2", "-u", "2"])
    }
    fn answer_dialog(&self, button: Option<&str>) -> Result<(), String> {
        if button.is_none() {
            let _: Option<[f32; 2]> = self.query(SceneQuery::DialogPoint { button: None })?;
            return self.back();
        }
        let point: Option<[f32; 2]> = self.query(SceneQuery::DialogPoint {
            button: button.map(str::to_owned),
        })?;
        let [x, y] = point.ok_or("the dialog button is not on screen")?;
        self.press_at(Point::new(Points(x), Points(y)))
    }
    fn animating(&self) -> bool {
        self.track(self.query(SceneQuery::Animating))
    }
    fn screenshot(&self) -> Result<Vec<u8>, String> {
        let bytes = self
            .device
            .artifact("png", &["uitest", "screenCap"])
            .map_err(|e| format!("{e:#}"))?;
        let image = image::load_from_memory(&bytes).map_err(|e| e.to_string())?;
        let [x, y, width, height] = self.geometry().pixels;
        if width == 0
            || height == 0
            || x.saturating_add(width) > image.width()
            || y.saturating_add(height) > image.height()
        {
            return Err("the test surface moved outside the captured screen".into());
        }
        let mut output = Cursor::new(Vec::new());
        image
            .crop_imm(x, y, width, height)
            .write_to(&mut output, image::ImageFormat::Png)
            .map_err(|e| e.to_string())?;
        Ok(output.into_inner())
    }
    fn resize(&self, _: Points, _: Points) -> Result<(), String> {
        Err("ERR_NSH_TEST_UNSUPPORTED: a HarmonyOS test fills the device screen".into())
    }
    fn set_dark(&self, dark: bool) -> Result<(), String> {
        self.server
            .step(TestStep::SetColorScheme { dark }, ASK)
            .map(drop)
    }
    fn quit(&self) {
        self.track(self.server.step(TestStep::Quit, ASK).map(drop));
    }
}

fn key_codes(keys: &str) -> Result<Vec<u32>, String> {
    let mut result = Vec::new();
    for part in keys.to_ascii_lowercase().split('+') {
        let code = match part {
            "mod" | "ctrl" | "control" => 2072,
            "shift" => 2047,
            "alt" => 2045,
            "meta" | "super" => 2076,
            "enter" | "return" => 2054,
            "backspace" => 2055,
            "delete" => 2071,
            "escape" | "esc" => 2070,
            "tab" => 2049,
            "space" | " " => 2050,
            "up" | "arrowup" => 2012,
            "down" | "arrowdown" => 2013,
            "left" | "arrowleft" => 2014,
            "right" | "arrowright" => 2015,
            "home" => 2081,
            "end" => 2082,
            text if text.len() == 1 && text.as_bytes()[0].is_ascii_lowercase() => {
                2017 + (text.as_bytes()[0] - b'a') as u32
            }
            _ => return Err(format!("ERR_NSH_TEST_UNSUPPORTED: hardware key {part:?}")),
        };
        result.push(code);
    }
    if result.is_empty() || result.len() > 3 {
        return Err("UITest supports one to three simultaneous keys".into());
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn density_and_window_offsets_are_applied_once() {
        let geometry = Geometry::from_surface([0.0, 96.0, 1200.0, 2400.0], [400.0, 800.0]).unwrap();
        assert_eq!(
            geometry.point(Point::new(Points(10.0), Points(20.0))),
            (30, 156)
        );
        assert_eq!(geometry.pixels, [0, 96, 1200, 2400]);
        assert!(Geometry::from_surface([0.0, 96.0, 1200.0, 2400.0], [400.0, 900.0]).is_err());
    }
    #[test]
    fn invalid_surface_geometry_is_rejected() {
        assert!(Geometry::from_surface([0.0, 0.0, 0.0, 800.0], [400.0, 800.0]).is_err());
        assert!(Geometry::from_surface([-10.0, 0.0, 400.0, 800.0], [400.0, 800.0]).is_err());
    }
    #[test]
    fn unsupported_hardware_keys_are_reported() {
        assert_eq!(key_codes("mod+a").unwrap(), [2072, 2017]);
        assert_eq!(key_codes("Enter").unwrap(), [2054]);
        assert!(key_codes("unknown").is_err());
    }
}
