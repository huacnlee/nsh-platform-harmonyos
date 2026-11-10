import { BuilderNode, FrameNode } from "@ohos.arkui.node";
import { UIContext } from "@ohos.arkui.UIContext";

export interface TabBarContext {
  change(index: number): void;
  height(height: number): void;
}
interface TabItem {
  title: string;
  icon?: string;
}
interface TabProperty {
  name: string;
  value: ESObject;
  removed: boolean;
}
class TabSettings {
  items: TabItem[] = [];
  selected: number = 0;
  hidden: boolean = false;
  tint: ResourceColor = $r("sys.color.ohos_id_color_emphasize");
  change: (index: number) => void;
  constructor(change: (index: number) => void) {
    this.change = change;
  }
}
@Builder
function tabBar(settings: TabSettings) {
  Tabs({ barPosition: BarPosition.Start, index: settings.selected }) {
    ForEach(settings.items, (item: TabItem) => {
      TabContent() {
        Column()
      }.tabBar(
        BottomTabBarStyle.of(item.icon ?? "", item.title)
          .labelStyle({ selectedColor: settings.tint })
          .iconStyle({ selectedColor: settings.tint }),
      );
    })
  }
  .barMode(BarMode.Fixed)
  .barHeight(56)
  .width("100%")
  .height(settings.hidden ? 0 : 56)
  .visibility(settings.hidden ? Visibility.None : Visibility.Visible)
  .scrollable(false)
  .onChange(settings.change);
}
export class NativeTabBar {
  readonly node: FrameNode;
  private readonly builder: BuilderNode<[TabSettings]>;
  private readonly context: TabBarContext;
  private settings: TabSettings;
  private closed: boolean = false;
  constructor(ui: UIContext, context: TabBarContext) {
    this.context = context;
    this.settings = new TabSettings((index: number) => {
      if (!this.closed && index !== this.settings.selected) {
        this.settings.selected = index;
        context.change(index);
      }
    });
    this.builder = new BuilderNode<[TabSettings]>(ui);
    this.builder.build(wrapBuilder(tabBar), this.settings);
    this.node = this.builder.getFrameNode()!;
    context.height(56);
  }
  updateProps(changes: TabProperty[]): void {
    if (this.closed) {
      return;
    }
    const next = new TabSettings(this.settings.change);
    next.items = this.settings.items;
    next.selected = this.settings.selected;
    next.hidden = this.settings.hidden;
    next.tint = this.settings.tint;
    for (const change of changes) {
      switch (change.name) {
        case "items":
          next.items = change.removed ? [] : (change.value as TabItem[]);
          break;
        case "selectedIndex":
          next.selected = change.removed ? 0 : (change.value as number);
          break;
        case "hidden":
          next.hidden = !change.removed && (change.value as boolean);
          break;
        case "tint":
          next.tint =
            change.removed || change.value == null
              ? $r("sys.color.ohos_id_color_emphasize")
              : (change.value as string);
          break;
      }
    }
    this.settings = next;
    this.builder.update(next);
    this.context.height(next.hidden ? 0 : 56);
  }
  dispose(): void {
    if (!this.closed) {
      this.closed = true;
      this.builder.dispose();
    }
  }
}
