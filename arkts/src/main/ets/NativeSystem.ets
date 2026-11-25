import { NativeShare } from "./NativeShare";
import { common, wantConstant } from "@kit.AbilityKit";
import { fileUri, picker } from "@kit.CoreFileKit";
import { UIContext } from "@ohos.arkui.UIContext";
import { NativeAuthSession } from "./NativeAuthSession";
import { NativeClipboard } from "./NativeClipboard";
import { NativeNotifications } from "./NativeNotifications";

// The system bridge uses the same typed completion for every service.
export interface SystemReply {
  has?: boolean;
  denied?: boolean;
  text?: string;
  html?: string;
  uri?: string;
  width?: number;
  height?: number;
  tooLarge?: number;
  files?: string;
  permission?: string;
  shown?: boolean;
  opened?: boolean;
  pressed?: string;
  error?: string;
  code?: string;
  phase?: string;
  final?: boolean;
}

export interface SystemRequest {
  op: string;
  paths?: string[];
  text?: string;
  uri?: string;
  name?: string;
  title?: string;
  body?: string;
  silent?: boolean;
  count?: number;
  maxBytes?: number;
  ask?: boolean;
  lang?: string;
  interim?: boolean;
  ephemeral?: boolean;
}
export class NativeSystem {
  private readonly context: common.UIAbilityContext;
  private readonly authSession: NativeAuthSession;
  private readonly clipboard: NativeClipboard;
  private readonly notifications: NativeNotifications;
  private readonly share: NativeShare = new NativeShare();
  private readonly reply: (key: number, event: SystemReply) => void;
  private active: Set<number> = new Set();
  private listening?: number;
  private closed: boolean = false;
  constructor(
    ui: UIContext,
    context: common.UIAbilityContext,
    reply: (key: number, event: SystemReply) => void,
  ) {
    this.context = context;
    this.authSession = new NativeAuthSession(ui);
    this.clipboard = new NativeClipboard(ui);
    this.notifications = new NativeNotifications(context);
    this.reply = reply;
  }
  setColorMode(mode: ThemeColorMode): void {
    this.authSession.setColorMode(mode);
  }
  async execute(key: number, request: SystemRequest): Promise<void> {
    if (this.closed) {
      return;
    }
    this.active.add(key);
    const reply = (event: SystemReply): void => {
      if (!this.closed && this.active.has(key)) {
        this.reply(key, event);
      }
    };
    try {
      if (request.op.startsWith("clipboard-")) {
        reply(
          await this.clipboard.execute(
            key,
            request.op,
            () => this.active.has(key),
            request.maxBytes,
          ),
        );
      } else if (request.op === "auth-session-open") {
        this.authSession.open(
          key,
          request.uri ?? "",
          request.name ?? "",
          request.ephemeral ?? false,
          (event: SystemReply) => {
            reply(event);
            this.active.delete(key);
          },
        );
        return;
      } else if (request.op === "auth-session-close") {
        this.authSession.close(key);
        this.active.delete(key);
        return;
      } else if (request.op === "share") {
        reply(
          await this.share.show(
            this.context,
            key,
            request,
            () => !this.closed && this.active.has(key),
          ),
        );
      } else if (request.op === "notification-permission") {
        reply(await this.notifications.permission(request.ask ?? false));
      } else if (request.op === "notification-show") {
        reply(
          await this.notifications.show(
            request.name ?? "",
            request.title ?? "",
            request.body ?? "",
            request.silent ?? false,
          ),
        );
      } else if (request.op === "notification-close") {
        await this.notifications.cancel(request.name ?? "");
        reply({} as SystemReply);
      } else if (request.op === "notification-badge") {
        await this.notifications.badge(request.count ?? 0);
        reply({} as SystemReply);
      } else if (request.op === "notification-listen") {
        if (this.listening !== undefined) {
          this.active.delete(this.listening);
        }
        this.listening = key;
        this.notifications.listen(reply);
        return;
      } else if (request.op === "file-open-path") {
        const path = request.uri ?? "";
        const uri = path.includes("://") ? path : fileUri.getUriFromPath(path);
        await this.context.startAbility({
          action: "ohos.want.action.viewData",
          uri,
          flags: wantConstant.Flags.FLAG_AUTH_READ_URI_PERMISSION,
        });
        reply({} as SystemReply);
      } else if (request.op === "file-reveal-path") {
        const path = request.uri ?? "";
        const uri = path.includes("://") ? path : fileUri.getUriFromPath(path);
        await this.context.startAbility({
          action: "ohos.want.action.viewData",
          uri: "filemanager://openDirectory",
          parameters: {
            fileUri: uri,
          },
        });
        reply({} as SystemReply);
      } else if (request.op === "file-move-to-trash") {
        if (!canIUse("SystemCapability.FileManagement.FileManagerService.Core")) {
          throw new Error("moving files to trash is not supported on this device");
        }
        const path = request.uri ?? "";
        const uri = path.includes("://") ? path : fileUri.getUriFromPath(path);
        const kit = await import("@hms.filemanagement.fileManagerService");
        let trashedUri = "";
        try {
          trashedUri = await kit.default.deleteToTrash(uri);
        } catch (err) {
          if (path.startsWith("/data/storage/")) {
            throw new Error(
              `moving app sandbox files to trash is not supported on HarmonyOS (${String(err)})`,
            );
          }
          throw new Error(String(err));
        }
        let trashedPath = trashedUri ?? "";
        if (trashedPath.startsWith("file://")) {
          try {
            trashedPath = new fileUri.FileUri(trashedPath).path;
          } catch {}
        }
        reply({
          files: trashedPath.length > 0 ? JSON.stringify([trashedPath]) : undefined,
        } as SystemReply);
      } else if (request.op === "file-open-dialog") {
        const documentPicker = new picker.DocumentViewPicker(this.context);
        const options = new picker.DocumentSelectOptions();
        options.maxSelectNumber = Math.max(1, Math.min(500, request.count ?? 1));
        if (request.name === "folders") {
          options.selectMode = picker.DocumentSelectMode.FOLDER;
        } else if (request.name === "both") {
          options.selectMode = picker.DocumentSelectMode.MIXED;
        } else {
          options.selectMode = picker.DocumentSelectMode.FILE;
        }
        if (request.paths && request.paths.length > 0 && request.name !== "folders") {
          options.fileSuffixFilters = request.paths;
        }
        if (request.uri && request.uri.length > 0) {
          options.defaultFilePathUri = request.uri.includes("://")
            ? request.uri
            : fileUri.getUriFromPath(request.uri);
        }
        const uris = (await documentPicker.select(options)) ?? [];
        const paths = uris.map((uri: string): string => {
          try {
            return uri.startsWith("file://") ? new fileUri.FileUri(uri).path : uri;
          } catch {
            return uri;
          }
        });
        reply({ files: JSON.stringify(paths) } as SystemReply);
      } else if (request.op === "file-save-dialog") {
        const documentPicker = new picker.DocumentViewPicker(this.context);
        const options = new picker.DocumentSaveOptions();
        if (request.name && request.name.length > 0) {
          options.newFileNames = [request.name];
        }
        if (request.paths && request.paths.length > 0) {
          options.fileSuffixChoices = request.paths;
        }
        if (request.uri && request.uri.length > 0) {
          options.defaultFilePathUri = request.uri.includes("://")
            ? request.uri
            : fileUri.getUriFromPath(request.uri);
        }
        const uris = (await documentPicker.save(options)) ?? [];
        const paths = uris.map((uri: string): string => {
          try {
            return uri.startsWith("file://") ? new fileUri.FileUri(uri).path : uri;
          } catch {
            return uri;
          }
        });
        reply({ files: JSON.stringify(paths) } as SystemReply);
      } else if (request.op === "open-app-settings") {
        try {
          await this.context.startAbility({
            bundleName: "com.huawei.hmos.settings",
            abilityName: "com.huawei.hmos.settings.MainAbility",
            uri: "application_info_entry",
            parameters: {
              pushParams: this.context.abilityInfo.bundleName,
            },
          });
          reply({ opened: true } as SystemReply);
        } catch {
          reply({ opened: false } as SystemReply);
        }
      } else {
        throw new Error(`unknown system operation: ${request.op}`);
      }
    } catch (error) {
      reply({
        error: String(error),
        code: "ERR_NSH_HOST_ERROR",
        has: false,
        shown: false,
        permission: "unavailable",
      } as SystemReply);
    }
    this.active.delete(key);
  }
  cancel(key: number): void {
    this.authSession.cancel(key);
    this.share.cancel(key);
    this.active.delete(key);
    this.clipboard.cancel(key);
    if (this.listening === key) {
      this.notifications.listen();
      this.listening = undefined;
    }
  }
  close(): void {
    this.closed = true;
    this.active.clear();
    this.authSession.closeAll();
    this.clipboard.close();
    this.notifications.close();
    this.share.close();
  }
}
