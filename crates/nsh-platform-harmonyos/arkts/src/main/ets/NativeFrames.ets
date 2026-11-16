import displaySync from "@ohos.graphics.displaySync";
import { UIContext } from "@ohos.arkui.UIContext";

// API 20's ArkUI FrameRateRange validates max <= 144, including on panels
// whose native frame-rate demand is higher. Bound only this UI consumer;
// changing FrameRate's shared demand here would also throttle GPU consumers.
const DISPLAY_SYNC_MAX_HZ = 144;

export interface FrameCallbacks {
  frame(timestamp: number): boolean;
}

export interface FrameRequest {
  enabled: boolean;
  close: boolean;
  min: number;
  max: number;
  expected: number;
}

// The runtime owns this consumer, independently of its optional shader outputs.
export function createFrameSource(
  ui: UIContext,
  callbacks: FrameCallbacks,
): (request: FrameRequest) => void {
  let source: displaySync.DisplaySync | undefined = undefined;
  ui.runScopedTask((): void => {
    source = displaySync.create();
    source.on("frame", (frame: displaySync.IntervalInfo): void => {
      let pending = false;
      try {
        pending = callbacks.frame(frame.timestamp);
      } finally {
        if (!pending) {
          source?.stop();
        }
      }
    });
  });
  return (request: FrameRequest): void => {
    const current = source;
    if (current === undefined) {
      return;
    }
    if (request.close) {
      try {
        ui.runScopedTask((): void => current.stop());
      } finally {
        current.off("frame");
        source = undefined;
      }
      return;
    }
    ui.runScopedTask((): void => {
      if (request.enabled) {
        current.setExpectedFrameRateRange({
          min: Math.min(request.min, DISPLAY_SYNC_MAX_HZ),
          max: Math.min(request.max, DISPLAY_SYNC_MAX_HZ),
          expected: Math.min(request.expected, DISPLAY_SYNC_MAX_HZ),
        });
        current.start();
      } else {
        current.stop();
      }
    });
  };
}
