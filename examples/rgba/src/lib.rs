//! Raw RGBA pixel buffers straight to the screen (`ImageSrc::rgba`): a test
//! pattern redrawn every frame, with no encoding anywhere on the way.

use std::{
    cell::Cell,
    time::{Duration, Instant},
};

use nsh_base::theme;
use nsh_core::{element::ImageSrc, prelude::*};

const WIDTH: u32 = 1280;
const HEIGHT: u32 = 720;

/// The app's UI set (`nsh.json` `"rust"`).
#[native_module("rgba/native")]
fn native(registry: &mut Registry) {
    registry.register::<App>();
}

/// The application's root component.
#[component]
pub fn App() -> impl IntoElement {
    let frame = signal(0_u64);
    let fps = signal(0_u32);
    let source = signal(ImageSrc::rgba(WIDTH, HEIGHT, render(0)));
    let ticks = Cell::new(0_u32);
    let since = Cell::new(Instant::now());
    set_interval(Duration::from_millis(16), move || {
        let n = frame.get() + 1;
        frame.set(n);
        source.set(ImageSrc::rgba(WIDTH, HEIGHT, render(n)));
        ticks.set(ticks.get() + 1);
        let elapsed = since.get().elapsed();
        if elapsed >= Duration::from_secs(1) {
            fps.set((f64::from(ticks.get()) / elapsed.as_secs_f64()).round() as u32);
            ticks.set(0);
            since.set(Instant::now());
        }
    });
    view()
        .flex_1()
        .p_safe_area()
        .bg(move || theme().background)
        .text_color(move || theme().foreground)
        .child(
            view()
                .flex_1()
                .items_center()
                .justify_center()
                .gap_4()
                .child(
                    image(move || source.get())
                        .w_full()
                        .aspect_ratio(WIDTH as f32 / HEIGHT as f32)
                        .rounded(px(8.0)),
                )
                .child(
                    text(move || format!("frame {} · {} fps", frame.get(), fps.get()))
                        .text_size(px(17.0)),
                ),
        )
}

/// The test pattern: opaque color bars with a moving marker over a
/// straight-alpha ramp, so a wrong alpha interpretation shows as wrong
/// colors against the app's background.
fn render(frame: u64) -> Vec<u8> {
    const BARS: [(u8, u8, u8); 7] = [
        (192, 192, 192),
        (192, 192, 0),
        (0, 192, 192),
        (0, 192, 0),
        (192, 0, 192),
        (192, 0, 0),
        (0, 0, 192),
    ];
    let w = WIDTH as usize;
    let h = HEIGHT as usize;
    let marker = (frame as usize * 8) % w;
    let mut pixels = vec![0u8; w * h * 4];
    for y in 0..h {
        for x in 0..w {
            let i = (y * w + x) * 4;
            let (r, g, b, a) = if y < h * 2 / 3 {
                let (r, g, b) = BARS[x * BARS.len() / w];
                if x.abs_diff(marker) < 6 {
                    (255, 255, 255, 255)
                } else {
                    (r, g, b, 255)
                }
            } else {
                (220, 40, 40, (x * 255 / w) as u8)
            };
            pixels[i..i + 4].copy_from_slice(&[r, g, b, a]);
        }
    }
    pixels
}
