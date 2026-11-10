//! One window VSync vote, shared by the surface's rendering lanes. Ordinary
//! frames use the system's normal range; live shader animation raises its floor.
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicU32, Ordering},
    },
    time::Instant,
};

use ohos_vsync_binding::{OH_NativeVSync_ExpectedRateRange, Vsync};

pub(crate) struct FrameRate {
    vsync: Arc<Vsync>,
    animations: Mutex<usize>,
    hz: AtomicU32,
    refreshed: Mutex<Instant>,
}
impl FrameRate {
    pub(crate) fn new(vsync: Arc<Vsync>) -> Arc<Self> {
        Arc::new(Self {
            vsync,
            animations: Mutex::new(0),
            hz: AtomicU32::new(display_hz()),
            refreshed: Mutex::new(Instant::now()),
        })
    }
    fn refresh(&self) -> bool {
        let mut refreshed = self.refreshed.lock().unwrap();
        if refreshed.elapsed().as_secs() == 0 {
            return false;
        }
        *refreshed = Instant::now();
        let hz = display_hz();
        self.hz.swap(hz, Ordering::Relaxed) != hz
    }
    pub(crate) fn vsync(&self) -> &Vsync {
        &self.vsync
    }
    pub(crate) fn vote(self: &Arc<Self>) -> Vote {
        Vote {
            rate: self.clone(),
            active: false,
        }
    }
}
pub(crate) struct Vote {
    rate: Arc<FrameRate>,
    active: bool,
}
impl Vote {
    pub(crate) fn update(&mut self, active: bool) {
        let refreshed = self.rate.refresh();
        if self.active == active && !refreshed {
            return;
        }
        let changed = self.active != active;
        self.active = active;
        let mut count = self.rate.animations.lock().unwrap();
        let was_high = *count > 0;
        if changed {
            if active {
                *count += 1;
            } else {
                *count -= 1;
            }
        }
        let high = *count > 0;
        if was_high != high || refreshed {
            let hz = self.rate.hz.load(Ordering::Relaxed) as i32;
            let status =
                self.rate
                    .vsync
                    .set_expected_frame_rate_range(OH_NativeVSync_ExpectedRateRange {
                        min: if high { hz } else { 0 },
                        max: hz,
                        expected: if high { hz } else { hz.min(60) },
                    });
            if status != 0 {
                log::warn!("OHOS VSync rate vote failed: {status}");
            }
        }
    }
}
impl Drop for Vote {
    fn drop(&mut self) {
        self.update(false);
    }
}

/// The binding reports zero when the display rate is unavailable.
pub(crate) fn display_hz() -> u32 {
    match ohos_display_binding::default_display_refresh_rate() {
        0 => 120,
        hz => hz.clamp(30, 240),
    }
}
