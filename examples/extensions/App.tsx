// Demonstrates calling HarmonyOS ArkTS APIs and Ability lifecycle hooks from
// TSX through a Rust native module (`examples/extensions/native`, nsh.json `"rust": "native"`).
// TypeScript types are generated into `.nsh/types/extensions/harmony.d.ts`.
import { mount, Scroll, signal, Text, View } from "nsh";
import { type DeviceInfo, HarmonyBridge, type WantInfo } from "extensions/harmony";

function ActionButton(props: { label: string; color?: string; onPress: () => void }) {
  return (
    <View
      onPress={props.onPress}
      style={{
        backgroundColor: props.color ?? "#0a59f7",
        borderRadius: 10,
        paddingVertical: 10,
        paddingHorizontal: 16,
        alignItems: "center",
      }}
    >
      <Text style={{ color: "#ffffff", fontSize: 15, fontWeight: "600" }}>{props.label}</Text>
    </View>
  );
}

function Card(props: { title: string; children: any }) {
  return (
    <View style={{ backgroundColor: "#ffffff", borderRadius: 14, padding: 16, gap: 8 }}>
      <Text style={{ fontSize: 13, fontWeight: "600", color: "#57606a" }}>{props.title}</Text>
      {props.children}
    </View>
  );
}

function App() {
  const device = signal<DeviceInfo | null>(null);
  const paths = signal(HarmonyBridge.getStoragePaths());
  const toastStatus = signal("Tap to invoke ArkUI promptAction.showToast via run_on_ui_thread");
  const abilityStatus = signal("Tap to invoke UIAbilityContext.startAbilityForResult");
  const imageStatus = signal("Tap to re-encode a sample PNG via prepare_image_bytes");
  const wants = signal<WantInfo[]>(
    (() => {
      const launch = HarmonyBridge.getLaunchWant();
      return launch ? [launch] : [];
    })(),
  );

  // Subscribe to EntryAbility onCreate / onNewWant events from Rust.
  HarmonyBridge.onWant((event) => {
    wants.update((list) => [event, ...list].slice(0, 5));
  });

  const refreshDevice = async () => {
    try {
      device.set(await HarmonyBridge.getDeviceInfo());
      paths.set(HarmonyBridge.getStoragePaths());
    } catch (err) {
      toastStatus.set(`getDeviceInfo failed: ${(err as Error).message}`);
    }
  };

  void refreshDevice();

  return (
    <View
      style={{
        flex: 1,
        backgroundColor: "#f4f5f7",
        paddingTop: "env(safe-area-inset-top)",
        paddingBottom: "env(safe-area-inset-bottom)",
      }}
    >
      <Scroll contentStyle={{ padding: 16, gap: 14 }}>
        <Text style={{ fontSize: 24, fontWeight: "bold", color: "#1f2328" }}>
          HarmonyOS Extensions Bridge
        </Text>

        <Card title="1. ArkTS @ohos.deviceInfo (run_on_ui_thread + load_module)">
          <Text style={{ fontSize: 15, color: "#1f2328" }}>
            {device()
              ? `${device()!.deviceType} · ${device()!.brand || "unknown"} ${device()!.marketName || ""} (API ${device()!.sdkApiVersion})`
              : "Loading..."}
          </Text>
          <Text style={{ fontSize: 13, color: "#57606a" }}>
            {`Bundle: ${device()?.bundleName || "-"} | OS: ${device()?.osFullName || "-"}`}
          </Text>
          <Text style={{ fontSize: 12, color: "#6e7781" }}>
            {`cacheDir: ${paths().cacheDir ?? "-"}`}
          </Text>
          <ActionButton label="Refresh Device Info" onPress={() => void refreshDevice()} />
        </Card>

        <Card title="2. ArkTS UIContext Toast (run_on_ui_thread + ui_context)">
          <Text style={{ fontSize: 14, color: "#1f2328" }}>{toastStatus()}</Text>
          <ActionButton
            label="Show Native ArkTS Toast"
            color="#1f883d"
            onPress={async () => {
              try {
                await HarmonyBridge.showToast("Hello from TSX → Rust → ArkTS!", 2000);
                toastStatus.set("Toast shown via uiContext.getPromptAction().showToast");
              } catch (err) {
                toastStatus.set(`Toast failed: ${(err as Error).message}`);
              }
            }}
          />
        </Card>

        <Card title="3. startAbilityForResult (ability::start_for_result)">
          <Text style={{ fontSize: 14, color: "#1f2328" }}>{abilityStatus()}</Text>
          <ActionButton
            label="Open System Settings"
            color="#6f42c1"
            onPress={async () => {
              try {
                const res = await HarmonyBridge.startAbility({
                  bundleName: "com.huawei.hmos.settings",
                  abilityName: "com.huawei.hmos.settings.MainAbility",
                });
                abilityStatus.set(`Returned resultCode=${res.resultCode}, uri=${res.uri ?? "-"}`);
              } catch (err) {
                abilityStatus.set(`startAbility error: ${(err as Error).message}`);
              }
            }}
          />
        </Card>

        <Card title="4. Native Image Pipeline (prepare_image_bytes)">
          <Text style={{ fontSize: 14, color: "#1f2328" }}>{imageStatus()}</Text>
          <ActionButton
            label="Encode Sample Image (JPEG 80%)"
            color="#bf8700"
            onPress={async () => {
              try {
                const info = await HarmonyBridge.prepareSampleImage(512, 0.8);
                imageStatus.set(
                  `Prepared ${info.mime} (${info.width}×${info.height}, ${info.bytes} bytes)`,
                );
              } catch (err) {
                imageStatus.set(`prepareSampleImage failed: ${(err as Error).message}`);
              }
            }}
          />
        </Card>

        <Card title="5. EntryAbility Wants (launch_want + on_ability_want)">
          <Text style={{ fontSize: 14, color: "#1f2328" }}>
            {wants().length === 0
              ? "No Want events received yet"
              : wants()
                  .map((w) => `[${w.kind}] ${w.uri ?? w.action ?? w.abilityName ?? "(empty)"}`)
                  .join("\n")}
          </Text>
        </Card>
      </Scroll>
    </View>
  );
}

mount(App);
