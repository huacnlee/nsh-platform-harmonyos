import { SystemReply } from "./NativeSystem";
import { common, Want } from "@kit.AbilityKit";
import notification from "@ohos.notificationManager";
import wantAgent from "@ohos.app.ability.wantAgent";
import preferences from "@ohos.data.preferences";

const pendingByContext: Map<common.UIAbilityContext, string[]> = new Map();
const pendingByAbility: Map<string, string[]> = new Map();
const instancesByContext: Map<common.UIAbilityContext, NativeNotifications[]> = new Map();
const instancesByAbility: Map<string, NativeNotifications[]> = new Map();

function pickReceiver(list: NativeNotifications[] | undefined): NativeNotifications | undefined {
  if (!list || list.length === 0) {
    return undefined;
  }
  // Prefer the newest instance actively listening, else the newest live instance.
  for (let i = list.length - 1; i >= 0; i--) {
    if (list[i].hasListener()) {
      return list[i];
    }
  }
  return list[list.length - 1];
}

function enqueuePending<K>(map: Map<K, string[]>, key: K, name: string): void {
  const queued = map.get(key) ?? [];
  queued.push(name);
  map.set(key, queued);
}

/** Forward the Ability's onCreate/onNewWant before loading its content. */
export function onNotificationWant(want: Want, context?: common.UIAbilityContext): void {
  const name = want.parameters?.["nsh.notification"];
  if (typeof name !== "string") {
    return;
  }
  if (context !== undefined) {
    const target = pickReceiver(instancesByContext.get(context));
    if (target) {
      target.deliver(name);
    } else {
      enqueuePending(pendingByContext, context, name);
    }
    return;
  }
  const ability = want.abilityName ?? "";
  const target = pickReceiver(instancesByAbility.get(ability));
  if (target) {
    target.deliver(name);
  } else {
    enqueuePending(pendingByAbility, ability, name);
  }
}
export interface NotificationReply extends SystemReply {
  permission?: string;
  shown?: boolean;
  pressed?: string;
}
export class NativeNotifications {
  private readonly context: common.UIAbilityContext;
  private readonly abilityName: string;
  private readonly settings: preferences.Preferences;
  private listener?: (reply: NotificationReply) => void;
  hasListener(): boolean {
    return this.listener !== undefined;
  }
  deliver(name: string): void {
    if (this.listener) {
      this.listener({ pressed: name });
    } else {
      enqueuePending(pendingByContext, this.context, name);
    }
  }
  constructor(context: common.UIAbilityContext) {
    this.context = context;
    this.abilityName = context.abilityInfo?.name ?? "";
    this.settings = preferences.getPreferencesSync(context, { name: "nsh-notifications" });
    const ctxList = instancesByContext.get(context) ?? [];
    ctxList.push(this);
    instancesByContext.set(context, ctxList);
    const nameList = instancesByAbility.get(this.abilityName) ?? [];
    nameList.push(this);
    instancesByAbility.set(this.abilityName, nameList);
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
  async show(
    name: string,
    title: string,
    body: string,
    silent: boolean = false,
  ): Promise<NotificationReply> {
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
      // API 20 OTHER_TYPES is LEVEL_MIN: no status bar icon, banner, or sound.
      // Silent is quieter than iOS/Android, which still show an icon. Per-notification
      // notificationFlags can mute a visible slot only when writable at API 23.
      notificationSlotType: silent
        ? notification.SlotType.OTHER_TYPES
        : notification.SlotType.SERVICE_INFORMATION,
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
      const ctxQueued = pendingByContext.get(this.context) ?? [];
      pendingByContext.delete(this.context);
      for (let i = 0; i < ctxQueued.length; i++) {
        listener({ pressed: ctxQueued[i] });
      }
      const nameQueued = pendingByAbility.get(this.abilityName) ?? [];
      pendingByAbility.delete(this.abilityName);
      for (let i = 0; i < nameQueued.length; i++) {
        listener({ pressed: nameQueued[i] });
      }
    }
  }
  close(): void {
    this.listener = undefined;
    const ctxList = instancesByContext.get(this.context);
    if (ctxList) {
      const idx = ctxList.indexOf(this);
      if (idx >= 0) {
        ctxList.splice(idx, 1);
      }
      if (ctxList.length === 0) {
        instancesByContext.delete(this.context);
        pendingByContext.delete(this.context);
      }
    }
    const nameList = instancesByAbility.get(this.abilityName);
    if (nameList) {
      const idx = nameList.indexOf(this);
      if (idx >= 0) {
        nameList.splice(idx, 1);
      }
      if (nameList.length === 0) {
        instancesByAbility.delete(this.abilityName);
      }
    }
  }
}
