import displaySync from "@ohos.graphics.displaySync";
import { UIContext } from "@ohos.arkui.UIContext";

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

// The only animation clock. Rust owns demand and supplies the rate; ArkUI
// applies its system divider and invokes us before layout and RS submission.
// NativeVSync maintains the RS vote but never advances these consumers.
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
          min: request.min,
          max: request.max,
          expected: request.expected,
        });
        current.start();
      } else {
        current.stop();
      }
    });
  };
}
