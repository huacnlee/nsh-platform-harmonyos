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
