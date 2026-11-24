import { common } from "@kit.AbilityKit";
import { fileUri } from "@kit.CoreFileKit";
import { uniformTypeDescriptor } from "@kit.ArkData";
import type systemShare from "@hms.collaboration.systemShare";
import type { SystemRequest, SystemReply } from "./NativeSystem";

export class NativeShare {
  private readonly pending: Map<number, () => void> = new Map();

  async show(
    context: common.UIAbilityContext,
    key: number,
    request: SystemRequest,
    live: () => boolean,
  ): Promise<SystemReply> {
    const kit = await import("@hms.collaboration.systemShare");
    if (!live()) {
      return { phase: "dismissed" };
    }
    const paths = request.paths ?? [];
    const text = [request.text, request.uri].filter((part?: string): boolean => !!part).join("\n");
    const records: systemShare.SharedRecord[] = [];
    for (const path of paths) {
      records.push({
        utd: uniformTypeDescriptor.UniformDataType.FILE,
        uri: fileUri.getUriFromPath(path),
        title: request.title,
        description: text || undefined,
      });
    }
    if (records.length === 0) {
      records.push({
        utd: uniformTypeDescriptor.UniformDataType.PLAIN_TEXT,
        content: text,
        title: request.title,
      });
    }
    const data = new kit.default.SharedData(records[0]);
    for (let i = 1; i < records.length; i++) {
      data.addRecord(records[i]);
    }
    const controller = new kit.default.ShareController(data);
    return new Promise<SystemReply>((resolve, reject) => {
      let done = false;
      const complete = (result: SystemReply, error?: Error): void => {
        if (done) {
          return;
        }
        done = true;
        this.pending.delete(key);
        controller.off("shareCompleted", shared);
        controller.off("dismiss", dismissed);
        if (error) {
          reject(error);
        } else {
          resolve(result);
        }
      };
      const shared = (event: systemShare.ShareOperationResult): void =>
        complete({ phase: "shared", text: event.targetAbilityInfo.name });
      const dismissed = (): void => complete({ phase: "dismissed" });
      this.pending.set(key, dismissed);
      controller.on("shareCompleted", shared);
      controller.on("dismiss", dismissed);
      controller
        .show(context, {
          selectionMode:
            paths.length > 1 ? kit.default.SelectionMode.BATCH : kit.default.SelectionMode.SINGLE,
        })
        .catch((error: Error) => complete({}, error));
    });
  }
  cancel(key: number): void {
    this.pending.get(key)?.();
  }
  close(): void {
    this.pending.forEach((cancel: () => void) => cancel());
  }
}
