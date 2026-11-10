import "./src/main/ets/NshHost";

export {
  NshCacheOptions,
  NshCapabilities,
  NshEnvironment,
  NshHostReply,
  NshNativeProperty,
  NshOptions,
  NshPackage,
  NshPackageInfo,
  NshScrollHandle,
  NshSurface,
  NshTrust,
  getCapabilities,
  initLogging,
  onBackPressIntercept,
} from "libnsh_platform_harmonyos.so";
export { onNotificationWant } from "./src/main/ets/NativeNotifications";
