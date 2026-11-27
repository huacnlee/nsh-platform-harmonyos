//! Host-side checks for OHOS geometry, motion and native ownership protocols.

extern crate self as ohos_arkui_binding;
use native_ownership::{common, component, event, render, types};
mod native_ownership;

#[path = "../../src/accessibility/value.rs"]
mod accessibility_value;
#[path = "../../src/accessibility/action_descriptions.rs"]
mod action_descriptions;
#[path = "../../src/render/composition/blend.rs"]
mod composite_blend;
#[path = "../../src/render/composition/geometry.rs"]
#[allow(dead_code)]
mod composite_geometry;
#[path = "../../src/render/effects.rs"]
#[allow(dead_code)]
mod effects;
#[path = "../../src/app/features.rs"]
#[allow(unreachable_pub)]
mod features;
#[path = "../../src/render/geometry.rs"]
mod geometry;
#[path = "../../src/render/gradient.rs"]
mod gradient;
#[path = "../../src/media/request.rs"]
mod image_request;
#[path = "../../src/navigation/motion.rs"]
mod navigation_motion;
#[path = "../../src/scroll/motion.rs"]
mod scroll_motion;
mod selection;
#[path = "../../src/render/shader/navigation.rs"]
mod shader_navigation;
#[cfg(unix)]
#[path = "../../src/media/video/source.rs"]
mod video_source;

#[path = "../../src/app/dev/presentation.rs"]
mod dev_presentation;

#[path = "../../src/input/editor_state.rs"]
#[allow(dead_code)]
mod editor_state;

#[path = "../../src/input/intercept.rs"]
#[allow(dead_code)]
mod input_intercept;

#[path = "../../src/services/executor.rs"]
mod executor;

#[path = "../../src/media/video/controls.rs"]
mod video_controls;

#[path = "../../src/render/shadow_geometry.rs"]
mod shadow_geometry;

#[path = "../../src/render/frame_clock.rs"]
mod frame_clock;

#[path = "../../src/app/chunks.rs"]
mod chunks;

#[path = "../../src/app/preboot.rs"]
#[allow(dead_code)]
mod preboot;

#[path = "../../src/render/transition.rs"]
#[allow(dead_code)]
mod transition;

#[path = "../../src/services/system.rs"]
#[allow(dead_code)]
mod system;

#[path = "../../src/media/web_evaluation.rs"]
mod web_evaluation;

#[path = "../../src/app/frame_rate.rs"]
#[allow(dead_code)]
mod frame_rate;

#[path = "../../src/render/composition/mailbox.rs"]
#[allow(dead_code)]
mod composition_mailbox;

#[path = "../../src/media/video/mailbox.rs"]
#[allow(dead_code)]
mod video_mailbox;

#[path = "../../src/render/composition/scene_policy.rs"]
#[allow(dead_code)]
mod scene_policy;

#[path = "../../src/services/desktop.rs"]
#[allow(dead_code)]
mod desktop;

#[path = "../../src/text/family.rs"]
mod text_family;

#[path = "../../src/render/canvas_render/spec.rs"]
mod canvas_render;

#[path = "../../src/media/web_downloads.rs"]
#[allow(dead_code)]
mod web_downloads;

#[path = "../../src/extensions.rs"]
#[allow(dead_code, unreachable_pub)]
mod extensions;

#[path = "../../src/services/network_monitor.rs"]
#[allow(dead_code)]
mod network_monitor;

#[path = "../../src/services/net.rs"]
#[allow(dead_code)]
mod net;

#[path = "../../src/services/speech.rs"]
#[allow(dead_code)]
mod speech;

mod services {
    pub(crate) use super::desktop;
}

#[path = "../../src/input/pan.rs"]
#[allow(dead_code)]
mod pan;

#[path = "../../src/input/wheel.rs"]
#[allow(dead_code)]
mod wheel;

#[path = "../../src/render/canvas_cache.rs"]
#[allow(dead_code)]
mod canvas_cache;

#[path = "../../src/interop/gpu_surface.rs"]
#[allow(dead_code, unreachable_pub)]
mod gpu_surface;

#[cfg(test)]
mod canvas_frame_rate_integration {
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicU32, Ordering},
    };
    use std::time::{Duration, Instant};

    use crate::{
        canvas_cache::CanvasActivity,
        frame_rate::{FrameRate, RateRange, RateSink, classify_frame_schedule},
    };

    struct TestSink {
        hz: AtomicU32,
        calls: Mutex<Vec<RateRange>>,
        notify: std::sync::mpsc::Sender<RateRange>,
    }

    impl RateSink for TestSink {
        fn set_range(&self, range: RateRange) -> i32 {
            self.calls.lock().unwrap().push(range);
            let _ = self.notify.send(range);
            0
        }

        fn read_display_hz(&self) -> u32 {
            self.hz.load(Ordering::Relaxed)
        }
    }

    #[test]
    fn streaming_canvas_votes_120hz_and_releases_when_idle() {
        let (notify, seen) = std::sync::mpsc::channel();
        let sink = Arc::new(TestSink {
            hz: AtomicU32::new(120),
            calls: Mutex::new(Vec::new()),
            notify,
        });
        let (rate, worker) = FrameRate::start_for_test(sink.clone());
        let idle = RateRange {
            min: 0,
            max: 120,
            expected: 60,
        };
        let full = RateRange {
            min: 120,
            max: 120,
            expected: 120,
        };
        assert_eq!(seen.recv_timeout(Duration::from_secs(2)).unwrap(), idle);
        let mut vote = rate.vote();
        let mut activity = CanvasActivity::default();
        let t0 = Instant::now();

        // Single static canvas update does not engage 120 Hz.
        assert!(!activity.record_redraw(t0));
        assert!(!rate.has_shader_demand());
        let idle_sched = classify_frame_schedule(
            true,
            false,
            Default::default(),
            rate.has_shader_demand(),
            true,
        );
        assert!(!idle_sched.rate_active);
        assert!(idle_sched.ui_active);

        // Second redraw 12 ms later engages sustained 120 Hz vote.
        let t1 = t0 + Duration::from_millis(12);
        assert!(activity.record_redraw(t1));
        assert!(vote.update(true));
        assert!(rate.has_shader_demand());
        let active_sched = classify_frame_schedule(
            true,
            false,
            Default::default(),
            rate.has_shader_demand(),
            false,
        );
        assert!(active_sched.rate_active);
        assert!(active_sched.ui_active);
        assert_eq!(rate.desired_range(), full);
        rate.publish_frame();
        assert_eq!(seen.recv_timeout(Duration::from_secs(2)).unwrap(), full);

        // Idle timeout releases the vote back to 60 Hz baseline.
        let t_idle = t1 + Duration::from_millis(210);
        assert!(!activity.check_idle(t_idle));
        assert!(vote.update(false));
        assert!(!rate.has_shader_demand());
        assert_eq!(rate.desired_range(), idle);
        rate.release();
        assert_eq!(seen.recv_timeout(Duration::from_secs(2)).unwrap(), idle);

        drop(vote);
        let applied = FrameRate::finish_for_test(rate, worker);
        assert_eq!(applied, Some(idle));
        assert_eq!(*sink.calls.lock().unwrap(), [idle, full, idle]);
    }
}
