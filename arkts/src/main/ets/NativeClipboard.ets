import { SystemReply } from "./NativeSystem";
import pasteboard from "@ohos.pasteboard";
import image from "@ohos.multimedia.image";
import util from "@ohos.util";
import { ComponentContent } from "@ohos.arkui.node";
import { UIContext } from "@ohos.arkui.UIContext";

export interface ClipboardReply extends SystemReply {
  has?: boolean;
  denied?: boolean;
  text?: string;
  html?: string;
  uri?: string;
  width?: number;
  height?: number;
  tooLarge?: number;
  files?: string;
}

class PasteCancelledError extends Error {
  constructor() {
    super("paste cancelled");
  }
}

export interface ExtractedPasteData {
  text: string;
  html?: string;
  files?: string;
  image?: string;
  tooLarge?: number;
}

async function readRecordMimeText(
  record: pasteboard.PasteDataRecord,
  mimeType: string,
): Promise<string | undefined> {
  try {
    const valid = record.getValidTypes([mimeType]);
    if (!valid.includes(mimeType)) {
      return undefined;
    }
    const value = await record.getData(mimeType);
    return typeof value === "string" && value.length > 0 ? value : undefined;
  } catch {
    return undefined;
  }
}

export async function extractPasteData(
  data: pasteboard.PasteData,
  maxBytes?: number,
): Promise<ExtractedPasteData> {
  let text = "";
  try {
    text = data.getPrimaryText() ?? "";
  } catch {}
  let html: string | undefined;
  try {
    const primaryHtml = data.getPrimaryHtml();
    if (primaryHtml && primaryHtml.length > 0) {
      html = primaryHtml;
    }
  } catch {}
  const uris: string[] = [];
  try {
    const count = data.getRecordCount();
    for (let i = 0; i < count; i++) {
      const record = data.getRecord(i);
      if (text.length === 0) {
        if (record.plainText && record.plainText.length > 0) {
          text = record.plainText;
        } else {
          const fallbackText = await readRecordMimeText(record, pasteboard.MIMETYPE_TEXT_PLAIN);
          if (fallbackText !== undefined) {
            text = fallbackText;
          }
        }
      }
      if (!html) {
        if (record.htmlText && record.htmlText.length > 0) {
          html = record.htmlText;
        } else {
          // Multi-MIME records created with `pasteboard.createData(Record)` expose
          // non-default representations (such as HTML alongside plain text) only
          // through `PasteDataRecord.getData(mimeType)`.
          html = await readRecordMimeText(record, pasteboard.MIMETYPE_TEXT_HTML);
        }
      }
      if (record.uri && record.uri.startsWith("file:")) {
        uris.push(record.uri);
      }
    }
  } catch {}
  const files = uris.length > 0 ? uris.join("\n") : undefined;
  let imgUri: string | undefined;
  let tooLarge: number | undefined;
  const limit =
    maxBytes !== undefined && maxBytes >= 0
      ? Math.min(maxBytes, 48 * 1024 * 1024)
      : 48 * 1024 * 1024;
  try {
    const pixels = data.getPrimaryPixelMap();
    if (pixels) {
      const packer = image.createImagePacker();
      try {
        const bytes = await packer.packing(pixels, { format: "image/png", quality: 100 });
        if (bytes.byteLength > 0) {
          if (bytes.byteLength <= limit) {
            imgUri = `data:image/png;base64,${new util.Base64Helper().encodeToStringSync(new Uint8Array(bytes))}`;
          } else {
            tooLarge = bytes.byteLength;
          }
        }
      } finally {
        packer.release();
        pixels.release();
      }
    }
  } catch {}
  return { text, html, files, image: imgUri, tooLarge };
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
    })
      .onClick((_event: ClickEvent, result: PasteButtonOnClickResult) => {
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
            reject(new PasteCancelledError());
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
    live: () => boolean,
    maxBytes?: number,
  ): Promise<ClipboardReply> {
    if (this.closed) {
      throw new Error("clipboard owner is closed");
    }
    const board = pasteboard.getSystemPasteboard();
    const type =
      op === "clipboard-image"
        ? pasteboard.MIMETYPE_PIXELMAP
        : op === "clipboard-files"
          ? pasteboard.MIMETYPE_TEXT_URI
          : pasteboard.MIMETYPE_TEXT_PLAIN;
    const has = op === "clipboard-paste" ? board.hasDataSync() : board.hasDataType(type);
    if (!has) {
      return { has: false };
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
        if (op === "clipboard-paste") {
          const rich = await extractPasteData(data, maxBytes);
          reply = {
            has:
              rich.text.length > 0 ||
              rich.html !== undefined ||
              rich.files !== undefined ||
              rich.image !== undefined ||
              rich.tooLarge !== undefined,
            text: rich.text,
            html: rich.html,
            files: rich.files,
            uri: rich.image,
            tooLarge: rich.tooLarge,
          };
        } else if (op === "clipboard-read") {
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
        if (error instanceof PasteCancelledError) {
          reply = { has: false, denied: true };
        } else {
          failure = new Error(String(error));
        }
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
