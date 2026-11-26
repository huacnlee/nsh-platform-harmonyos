import { UIAbility, Want } from "@kit.AbilityKit";
import { window } from "@kit.ArkUI";
import { onAbilityWant, onLaunchWant, onNotificationWant } from "@nsh/harmonyos";

export default class EntryAbility extends UIAbility {
  onCreate(want: Want): void {
    onAbilityWant(want, "create");
    onNotificationWant(want, this.context);
    const devUrl = want.parameters?.["nsh.devUrl"];
    if (typeof devUrl === "string" && devUrl.length > 0) {
      AppStorage.setOrCreate("nsh.devUrl", devUrl);
      return;
    }
    if (typeof want.parameters?.["nsh.notification"] !== "string") {
      onLaunchWant(this.context, want, true);
    }
  }

  onNewWant(want: Want): void {
    onAbilityWant(want, "new-want");
    const isNotification = typeof want.parameters?.["nsh.notification"] === "string";
    onNotificationWant(want, this.context);
    if (isNotification) {
      return;
    }
    const devUrl = want.parameters?.["nsh.devUrl"];
    if (typeof devUrl === "string" && devUrl.length > 0) {
      AppStorage.setOrCreate("nsh.devUrl", devUrl);
      this.context.eventHub.emit("nsh.devUrl", devUrl);
      return;
    }
    const hasLaunch = onLaunchWant(this.context, want, false);
    const rawArgs = want.parameters?.["nsh.args"];
    if (hasLaunch && !Array.isArray(rawArgs)) {
      return;
    }
    const args: string[] = [];
    if (Array.isArray(rawArgs)) {
      for (let i = 0; i < rawArgs.length; i++) {
        if (typeof rawArgs[i] === "string") {
          args.push(rawArgs[i] as string);
        }
      }
    }
    this.context.eventHub.emit("nsh.secondInstance", args);
  }

  onWindowStageCreate(windowStage: window.WindowStage): void {
    windowStage.loadContent("pages/Index");
  }
}
