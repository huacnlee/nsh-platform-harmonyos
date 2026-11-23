//! Single owner of the window-associated `NativeVSync` expected frame-rate
//! range. Pure arbitration logic lives in `frame_rate_spec`.

use std::sync::Arc;

use ohos_vsync_binding::{OH_NativeVSync_ExpectedRateRange, Vsync};

pub(crate) use crate::app::frame_rate_spec::{
    FrameRate, RateRange, RateSink, UiDemand, normalize_display_hz,
};

struct OhosSink {
    vsync: Arc<Vsync>,
    host: Arc<crate::app::session::Services>,
}

impl RateSink for OhosSink {
    fn set_range(&self, range: RateRange) -> i32 {
        self.vsync
            .set_expected_frame_rate_range(OH_NativeVSync_ExpectedRateRange {
                min: range.min,
                max: range.max,
                expected: range.expected,
            })
    }

    fn read_display_hz(&self) -> u32 {
        self.host.max_refresh_rate()
    }
}

impl FrameRate {
    pub(crate) fn new(
        vsync: Arc<Vsync>,
        host: Arc<crate::app::session::Services>,
    ) -> std::io::Result<Arc<Self>> {
        let sink: Arc<dyn RateSink> = Arc::new(OhosSink { vsync, host });
        Self::from_sink(sink)
    }
}
