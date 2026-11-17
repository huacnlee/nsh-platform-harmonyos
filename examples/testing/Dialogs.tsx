import { mount, signal, Text, View } from "nsh";
import { AlertDialog } from "nsh/base";

function App() {
  const result = signal("pending");
  const show = async (sheet = false, cancelable = true, title = "关闭全部连接？") => {
    const answer = await AlertDialog.show({
      title,
      description: "将关闭当前所有连接。标题、正文和操作按钮均使用系统默认样式。",
      presentation: sheet ? "actionSheet" : "alert",
      closeOnEscape: cancelable,
      closeOnBackdropPress: cancelable,
      actions: [
        { id: "cancel", label: "取消", role: "cancel" },
        { id: "close", label: "关闭全部", role: "destructive" },
        ...(sheet ? [{ id: "details", label: "查看详情" }] : []),
      ],
    });
    result.set(`${title}: ${answer.action ?? "dismissed"}`);
  };
  return (
    <View style={{ flex: 1, padding: 24, gap: 16 }}>
      <Text>System dialog probe</Text>
      <Text>{result()}</Text>
      <View onPress={() => show()} style={{ padding: 12 }}><Text>Open alert</Text></View>
      <View onPress={() => show(true)} style={{ padding: 12 }}><Text>Open sheet</Text></View>
      <View onPress={() => show(false, false)} style={{ padding: 12 }}><Text>Require choice</Text></View>
      <View onPress={() => { void show(false, true, "First"); void show(false, true, "Second"); }} style={{ padding: 12 }}>
        <Text>Queue two alerts</Text>
      </View>
    </View>
  );
}
mount(App);
