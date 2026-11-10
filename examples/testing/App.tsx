import { colorScheme, createAnimatedValue, mount, signal, Storage, Scroll, Text, View } from "nsh";
import { Input, RichEditor } from "nsh/base";

function App() {
  const draft = signal("initial");
  const action = signal("none");
  const seed = signal("loading");
  const response = signal("waiting");
  const timer = signal("pending");
  const panX = createAnimatedValue(0);
  Storage.getItem("seed").then((value) => seed.set(value ?? "empty"));
  fetch("https://api.test/probe").then((r) => r.text()).then(response.set).catch(() => response.set("offline"));
  setTimeout(() => timer.set("done"), 10000);
  return (
    <View style={{ flex: 1, padding: 24, paddingTop: 48, gap: 8, backgroundColor: "#ffffff" }}>
      <Text>Device test probe</Text>
      <Text>{`Scheme: ${colorScheme()}`}</Text>
      <Input
        testID="draft"
        value={draft()}
        onChange={({ value }) => draft.set(value)}
        onSubmit={({ text }) => action.set(`submit ${text}`)}
        style={{ height: 40, borderWidth: 1 }}
      />
      <Text>{`Draft: ${draft()}`}</Text>
      <RichEditor testID="rich" placeholder="Write..." style={{ height: 48, borderWidth: 1 }} />
      <View onPress={() => action.set("tap")} onLongPress={() => action.set("held")} delayLongPress={200} style={{ padding: 12 }}>
        <Text>Hold me</Text>
      </View>
      <View
        testID="pad"
        pan={{ x: panX }}
        onPanEnd={({ translation }) => action.set(`drag ${translation.x > 20 ? "right" : "left"}`)}
        style={{ height: 36, justifyContent: "center" }}
      >
        <Text>Drag pad</Text>
      </View>
      <Text>{`Action: ${action()}`}</Text>
      <Text>{`Seed: ${seed()}`}</Text>
      <Text>{`Response: ${response()}`}</Text>
      <Text>{`Timer: ${timer()}`}</Text>
      <View onPress={() => Storage.setItem("seed", "changed")} style={{ padding: 8 }}><Text>Save</Text></View>
      <Scroll testID="rows" style={{ height: 160 }}>
        <Each of={Array.from({ length: 30 }, (_, i) => i)}>{(number) => (
          <View onPress={() => action.set(`row ${number()}`)} style={{ height: 60, justifyContent: "center" }}>
            <Text>{`Row ${number()}`}</Text>
          </View>
        )}</Each>
      </Scroll>
    </View>
  );
}

mount(App);
