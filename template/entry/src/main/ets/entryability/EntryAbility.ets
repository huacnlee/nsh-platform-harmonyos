import { UIAbility, Want } from "@kit.AbilityKit";
import { window } from "@kit.ArkUI";
import { onNotificationWant } from "@nsh/harmonyos";

export default class EntryAbility extends UIAbility {
  onCreate(want: Want): void {
    onNotificationWant(want);
    const devUrl = want.parameters?.["nsh.devUrl"];
    if (typeof devUrl === "string" && devUrl.length > 0) {
      AppStorage.setOrCreate("nsh.devUrl", devUrl);
    }
    if (typeof want.uri === "string" && want.uri.length > 0) {
      AppStorage.setOrCreate("nsh.launchUrl", want.uri);
    }
  }

  onNewWant(want: Want): void {
    onNotificationWant(want);
    const devUrl = want.parameters?.["nsh.devUrl"];
    if (typeof devUrl === "string" && devUrl.length > 0) {
      AppStorage.setOrCreate("nsh.devUrl", devUrl);
      this.context.eventHub.emit("nsh.devUrl", devUrl);
    }
    if (typeof want.uri === "string" && want.uri.length > 0) {
      this.context.eventHub.emit("nsh.openUrl", want.uri);
    }
    const rawArgs = want.parameters?.["nsh.args"];
    const args: string[] = [];
    if (Array.isArray(rawArgs)) {
      for (let i = 0; i < rawArgs.length; i++) {
        if (typeof rawArgs[i] === "string") {
          args.push(rawArgs[i] as string);
        }
      }
    } else if (typeof want.uri === "string" && want.uri.length > 0) {
      args.push(want.uri);
    }
    this.context.eventHub.emit("nsh.secondInstance", args);
  }

  onWindowStageCreate(windowStage: window.WindowStage): void {
    windowStage.loadContent("pages/Index");
  }
}
