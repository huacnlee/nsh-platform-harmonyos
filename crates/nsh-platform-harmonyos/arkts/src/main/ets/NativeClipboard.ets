import { SystemReply } from "./NativeSystem";
import pasteboard from "@ohos.pasteboard";
import image from "@ohos.multimedia.image";
import util from "@ohos.util";
import { ComponentContent } from "@ohos.arkui.node";
import { UIContext } from "@ohos.arkui.UIContext";

export interface ClipboardReply extends SystemReply {
  has?: boolean;
  text?: string;
  uri?: string;
  width?: number;
  height?: number;
  tooLarge?: number;
  files?: string;
}
class PasteRequest {
  constructor(read: () => void) {
    this.read = read;
  }
  read: () => void;
}
@Builder
function pastePrompt(request: PasteRequest) {
  Column({ space: 16 }) {
    Text("Paste from clipboard").fontSize(20)
    PasteButton({
      icon: PasteIconStyle.LINES,
      text: PasteDescription.PASTE,
      buttonType: ButtonType.Capsule,
    }).onClick((_event: ClickEvent, result: PasteButtonOnClickResult) => {
      if (result === PasteButtonOnClickResult.SUCCESS) {
        request.read();
      }
    })
  }
  .padding(24)
  .backgroundColor($r("sys.color.ohos_id_color_dialog_bg"))
  .borderRadius(24);
}
export class NativeClipboard {
  private readonly ui: UIContext;
  private prompt?: ComponentContent<PasteRequest>;
  private dismiss?: () => void;
  private reading?: number;
  private reads: Promise<void> = Promise.resolve();
  private closed: boolean = false;
  constructor(ui: UIContext) {
    this.ui = ui;
  }
  private async read(): Promise<pasteboard.PasteData> {
    const board = pasteboard.getSystemPasteboard();
    try {
      const data = await board.getData();
      if (data.getRecordCount() > 0 || !board.hasDataSync()) {
        return data;
      }
      throw new Error("clipboard access requires the paste action");
    } catch {
      if (this.closed) {
        throw new Error("clipboard owner is closed");
      }
      // The security component grants this user action clipboard access without
      // a restricted READ_PASTEBOARD permission in the embedding application.
      return new Promise<pasteboard.PasteData>((resolve, reject) => {
        let settled = false;
        const finish = (): void => {
          const prompt = this.prompt;
          this.prompt = undefined;
          this.dismiss = undefined;
          if (prompt) {
            this.ui
              .getPromptAction()
              .closeCustomDialog(prompt)
              .finally(() => prompt.dispose());
          }
        };
        this.dismiss = (): void => {
          if (!settled) {
            settled = true;
            reject(new Error("paste cancelled"));
            finish();
          }
        };
        const request = new PasteRequest(() => {
          if (settled) {
            return;
          }
          // Start the read inside the granted security-component callback.
          const result = board.getData();
          settled = true;
          result.then(resolve, reject).finally(finish);
        });
        this.prompt = new ComponentContent<PasteRequest>(
          this.ui,
          wrapBuilder(pastePrompt),
          request,
        );
        this.ui
          .getPromptAction()
          .openCustomDialog(this.prompt, {
            autoCancel: true,
            onWillDismiss: () => this.dismiss?.(),
          })
          .catch((error: Error) => {
            if (!settled) {
              settled = true;
              reject(error);
              finish();
            }
          });
      });
    }
  }
  async execute(
    key: number,
    op: string,
    text: string,
    hasOnly: boolean,
    live: () => boolean,
  ): Promise<ClipboardReply> {
    if (this.closed) {
      throw new Error("clipboard owner is closed");
    }
    const board = pasteboard.getSystemPasteboard();
    if (op === "clipboard-write") {
      await board.setData(pasteboard.createData(pasteboard.MIMETYPE_TEXT_PLAIN, text));
      return {};
    }
    if (op === "clipboard-write-image") {
      const comma = text.indexOf(",");
      if (
        !text.startsWith("data:image/") ||
        comma < 0 ||
        !text.substring(0, comma).endsWith(";base64")
      ) {
        throw new Error("invalid image data URI");
      }
      const bytes = new util.Base64Helper().decodeSync(text.substring(comma + 1));
      const source = image.createImageSource(bytes.buffer);
      let pixels: image.PixelMap | undefined;
      try {
        pixels = await source.createPixelMap();
        await board.setData(pasteboard.createData(pasteboard.MIMETYPE_PIXELMAP, pixels));
      } finally {
        pixels?.release();
        source.release();
      }
      return {};
    }
    const type =
      op === "clipboard-image"
        ? pasteboard.MIMETYPE_PIXELMAP
        : op === "clipboard-files"
          ? pasteboard.MIMETYPE_TEXT_URI
          : pasteboard.MIMETYPE_TEXT_PLAIN;
    const has = board.hasDataType(type);
    if (hasOnly || !has) {
      return { has };
    }
    let reply: ClipboardReply = { has: false };
    let failure: Error | undefined;
    // One visible paste prompt per window; a second request waits its turn.
    const work = this.reads.then(async () => {
      try {
        if (this.closed || !live()) {
          throw new Error("clipboard request cancelled");
        }
        this.reading = key;
        const data = await this.read();
        if (op === "clipboard-read") {
          reply = { has: data.hasType(type), text: data.getPrimaryText() ?? "" };
        } else if (op === "clipboard-files") {
          const files: string[] = [];
          for (let i = 0; i < data.getRecordCount(); i++) {
            const uri = data.getRecord(i).uri;
            if (uri?.startsWith("file:")) {
              files.push(uri);
            }
          }
          reply = { has: files.length > 0, files: files.join("\n") };
        } else {
          const pixels = data.getPrimaryPixelMap();
          if (!pixels) {
            reply = { has: false };
            return;
          }
          const packer = image.createImagePacker();
          try {
            const info = await pixels.getImageInfo();
            const bytes = await packer.packing(pixels, { format: "image/png", quality: 100 });
            reply =
              bytes.byteLength > 48 * 1024 * 1024
                ? { has: true, tooLarge: bytes.byteLength }
                : {
                    has: true,
                    uri: `data:image/png;base64,${new util.Base64Helper().encodeToStringSync(new Uint8Array(bytes))}`,
                    width: info.size.width,
                    height: info.size.height,
                  };
          } finally {
            packer.release();
            pixels.release();
          }
        }
      } catch (error) {
        failure = new Error(String(error));
      } finally {
        this.reading = undefined;
      }
    });
    this.reads = work;
    await work;
    if (failure) {
      throw failure;
    }
    return reply;
  }
  cancel(key: number): void {
    if (this.reading === key) {
      this.dismiss?.();
    }
  }
  close(): void {
    this.closed = true;
    this.dismiss?.();
  }
}
