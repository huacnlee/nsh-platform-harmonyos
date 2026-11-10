import { SystemReply } from "./NativeSystem";
import { common, Want } from "@kit.AbilityKit";
import notification from "@ohos.notificationManager";
import wantAgent from "@ohos.app.ability.wantAgent";
import preferences from "@ohos.data.preferences";

const pending: Map<string, string[]> = new Map();
const receivers: Map<string, (name: string) => void> = new Map();
/** Forward the Ability's onCreate/onNewWant before loading its content. */
export function onNotificationWant(want: Want): void {
  const name = want.parameters?.["nsh.notification"];
  if (typeof name !== "string") {
    return;
  }
  const ability = want.abilityName ?? "";
  const receiver = receivers.get(ability);
  if (receiver) {
    receiver(name);
  } else {
    const queued = pending.get(ability) ?? [];
    queued.push(name);
    pending.set(ability, queued);
  }
}
export interface NotificationReply extends SystemReply {
  permission?: string;
  shown?: boolean;
  pressed?: string;
}
export class NativeNotifications {
  private readonly context: common.UIAbilityContext;
  private readonly settings: preferences.Preferences;
  private listener?: (reply: NotificationReply) => void;
  private readonly pressed = (name: string): void => {
    if (this.listener) {
      this.listener({ pressed: name });
    } else {
      const key = this.context.abilityInfo.name;
      const queued = pending.get(key) ?? [];
      queued.push(name);
      pending.set(key, queued);
    }
  };
  constructor(context: common.UIAbilityContext) {
    this.context = context;
    this.settings = preferences.getPreferencesSync(context, { name: "nsh-notifications" });
    receivers.set(context.abilityInfo.name, this.pressed);
  }
  async permission(ask: boolean): Promise<NotificationReply> {
    if (await notification.isNotificationEnabled()) {
      return { permission: "granted" };
    }
    if (ask) {
      this.settings.putSync("asked", true);
      await this.settings.flush();
      try {
        await notification.requestEnableNotification(this.context);
      } catch {}
    }
    return {
      permission: (await notification.isNotificationEnabled())
        ? "granted"
        : this.settings.getSync("asked", false)
          ? "denied"
          : "default",
    };
  }
  async show(name: string, title: string, body: string): Promise<NotificationReply> {
    const agent = await wantAgent.getWantAgent({
      wants: [
        {
          bundleName: this.context.abilityInfo.bundleName,
          abilityName: this.context.abilityInfo.name,
          parameters: { "nsh.notification": name },
        },
      ],
      operationType: wantAgent.OperationType.START_ABILITY,
      requestCode: this.id(name),
      wantAgentFlags: [wantAgent.WantAgentFlags.UPDATE_PRESENT_FLAG],
    });
    await notification.publish({
      id: this.id(name),
      label: name,
      content: {
        notificationContentType: notification.ContentType.NOTIFICATION_CONTENT_BASIC_TEXT,
        normal: { title, text: body },
      },
      wantAgent: agent,
    });
    return { shown: true };
  }
  private id(name: string): number {
    // A persisted allocation keeps replacement/cancel and cold starts identical,
    // including distinct application IDs whose hashes would collide.
    const key = `id:${name}`;
    const old = this.settings.getSync(key, 0) as number;
    if (old) {
      return old;
    }
    const id = (this.settings.getSync("next", 0) as number) + 1;
    this.settings.putSync(key, id);
    this.settings.putSync("next", id);
    this.settings.flush();
    return id;
  }
  async cancel(name: string): Promise<void> {
    await notification.cancel(this.id(name), name);
  }
  async badge(count: number): Promise<void> {
    await notification.setBadgeNumber(count);
  }
  listen(listener?: (reply: NotificationReply) => void): void {
    this.listener = listener;
    if (listener) {
      const key = this.context.abilityInfo.name;
      const queued = pending.get(key) ?? [];
      pending.delete(key);
      for (const name of queued) {
        listener({ pressed: name });
      }
    }
  }
  close(): void {
    this.listener = undefined;
    if (receivers.get(this.context.abilityInfo.name) === this.pressed) {
      receivers.delete(this.context.abilityInfo.name);
    }
  }
}
