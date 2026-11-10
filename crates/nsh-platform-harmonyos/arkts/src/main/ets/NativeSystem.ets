import { common, wantConstant } from "@kit.AbilityKit";
import { fileUri } from "@kit.CoreFileKit";
import { UIContext } from "@ohos.arkui.UIContext";
import { NativeAudio } from "./NativeAudio";
import { NativeClipboard, ClipboardReply } from "./NativeClipboard";
import { NativeNotifications, NotificationReply } from "./NativeNotifications";
import { NativeSpeech, SpeechReply } from "./NativeSpeech";

// The system bridge uses the same typed completion for every service.
export interface SystemReply {
  has?: boolean;
  text?: string;
  uri?: string;
  width?: number;
  height?: number;
  tooLarge?: number;
  files?: string;
  permission?: string;
  shown?: boolean;
  pressed?: string;
  error?: string;
  code?: string;
  phase?: string;
  final?: boolean;
  durationMs?: number;
  positionMs?: number;
  bytes?: number;
  truncated?: boolean;
  buffering?: boolean;
}

export interface SystemRequest {
  op: string;
  text?: string;
  uri?: string;
  hasOnly?: boolean;
  name?: string;
  title?: string;
  body?: string;
  silent?: boolean;
  count?: number;
  ask?: boolean;
  lang?: string;
  interim?: boolean;
  volume?: number;
  rate?: number;
  looped?: boolean;
  positionMs?: number;
  durationMs?: number;
}
export class NativeSystem {
  private readonly context: common.UIAbilityContext;
  private readonly clipboard: NativeClipboard;
  private readonly notifications: NativeNotifications;
  private readonly speech: NativeSpeech;
  private readonly audio: NativeAudio;
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
    this.clipboard = new NativeClipboard(ui);
    this.notifications = new NativeNotifications(context);
    this.speech = new NativeSpeech(context);
    this.audio = new NativeAudio();
    this.reply = reply;
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
            request.uri ?? request.text ?? "",
            request.hasOnly ?? false,
            () => this.active.has(key),
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
      } else if (request.op === "speech-start") {
        await this.speech.start(
          key,
          request.lang,
          request.interim ?? true,
          (event: SpeechReply) => {
            reply(event);
            if (event.phase === "end") {
              this.active.delete(key);
            }
          },
        );
        return;
      } else if (request.op === "speech-stop" || request.op === "speech-abort") {
        await this.speech.stop(key, request.op === "speech-abort");
        if (request.op === "speech-abort") {
          this.active.delete(key);
        }
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
      } else if (request.op === "audio-load") {
        await this.audio.loadSound(
          request.count ?? 0,
          key,
          request.uri ?? "",
          request.durationMs ?? 5000,
          (event: SystemReply) => {
            reply(event);
            this.active.delete(key);
          },
        );
        return;
      } else if (request.op === "audio-play") {
        this.audio.playSound(
          request.count ?? 0,
          request.volume ?? 1,
          request.rate ?? 1,
        );
      } else if (request.op === "audio-stop") {
        this.audio.stopSound(request.count ?? 0);
      } else if (request.op === "audio-unload") {
        this.audio.unloadSound(request.count ?? 0);
      } else if (request.op === "audio-open") {
        this.audio.openPlayer(key, request, reply);
        return;
      } else if (request.op === "audio-preload") {
        this.audio.preloadPlayer(request.count ?? 0);
      } else if (request.op === "audio-resume") {
        this.audio.resumePlayer(request.count ?? 0);
      } else if (request.op === "audio-pause") {
        this.audio.pausePlayer(request.count ?? 0);
      } else if (request.op === "audio-seek") {
        this.audio.seekPlayer(request.count ?? 0, request.positionMs ?? 0);
      } else if (request.op === "audio-set") {
        this.audio.setPlayer(
          request.count ?? 0,
          request.volume ?? 1,
          request.rate ?? 1,
          request.looped ?? false,
        );
      } else if (request.op === "audio-close") {
        this.audio.closePlayer(request.count ?? 0);
      } else {
        throw new Error(`unknown system operation: ${request.op}`);
      }
    } catch (error) {
      reply({
        error: String(error),
        code: "ERR_NSH_HOST_ERROR",
        phase:
          request.op.startsWith("speech-") || request.op === "audio-open"
            ? "end"
            : undefined,
        has: false,
        shown: false,
        permission: "unavailable",
      } as SystemReply);
    }
    this.active.delete(key);
  }
  cancel(key: number): void {
    this.active.delete(key);
    this.clipboard.cancel(key);
    if (this.listening === key) {
      this.notifications.listen();
      this.listening = undefined;
    }
    this.speech
      .stop(key, true)
      .catch((error: Error) => console.warn(`NSH speech cancel: ${error}`));
    this.audio.cancel(key);
  }
  close(): void {
    this.closed = true;
    this.active.clear();
    this.clipboard.close();
    this.notifications.close();
    this.speech.close();
    this.audio.close();
  }
}
