import { BuilderNode, FrameNode } from "@ohos.arkui.node";
import { UIContext } from "@ohos.arkui.UIContext";

export interface TabBarContext {
  readonly name: string;
  size(width: number, height: number): void;
  valueChanged(value: ESObject): void;
  change(index: number): void;
  height(height: number): void;
}
interface TabItem {
  title: string;
  icon?: string;
  badge?: string;
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
  tint?: string;
  colorMode: ThemeColorMode = ThemeColorMode.SYSTEM;
  change: (index: number) => void;
  constructor(change: (index: number) => void) {
    this.change = change;
  }
}

@Builder
function tabBadge(item: TabItem, selected: boolean, tint?: string) {
  if (item.icon && item.icon.length > 0) {
    Image(item.icon)
      .width(24)
      .height(24)
      .objectFit(ImageFit.Contain)
      .fillColor(
        selected ? (tint ?? $r("sys.color.icon_emphasize")) : $r("sys.color.icon_secondary"),
      );
  } else {
    Text(item.title)
      .fontSize(12)
      .fontWeight(selected ? FontWeight.Medium : FontWeight.Regular)
      .fontColor(
        selected ? (tint ?? $r("sys.color.font_emphasize")) : $r("sys.color.font_secondary"),
      )
      .maxLines(1)
      .textOverflow({ overflow: TextOverflow.Ellipsis })
      .padding({
        left: item.badge !== undefined && item.badge.length > 0 ? 16 : 10,
        right: item.badge !== undefined && item.badge.length > 0 ? 16 : 10,
        top: 4,
        bottom: 4,
      });
  }
}

@Builder
function tabBarItem(item: TabItem, index: number, settings: TabSettings) {
  Column({ space: 4 }) {
    if (item.badge !== undefined) {
      Badge({
        value: item.badge,
        position: BadgePosition.RightTop,
        style:
          item.badge.length === 0
            ? {
                badgeSize: 6,
                badgeColor: $r("sys.color.warning"),
              }
            : {
                badgeSize: 15,
                fontSize: 9,
                fontWeight: FontWeight.Medium,
                color: $r("sys.color.font_on_primary"),
                badgeColor: $r("sys.color.warning"),
              },
      }) {
        tabBadge(item, settings.selected === index, settings.tint)
      };
    } else if (item.icon && item.icon.length > 0) {
      Image(item.icon)
        .width(24)
        .height(24)
        .objectFit(ImageFit.Contain)
        .fillColor(
          settings.selected === index
            ? (settings.tint ?? $r("sys.color.icon_emphasize"))
            : $r("sys.color.icon_secondary"),
        );
    }
    if ((item.icon && item.icon.length > 0) || item.badge === undefined) {
      Text(item.title)
        .fontSize(item.icon && item.icon.length > 0 ? 10 : 12)
        .fontWeight(settings.selected === index ? FontWeight.Medium : FontWeight.Regular)
        .fontColor(
          settings.selected === index
            ? (settings.tint ?? $r("sys.color.font_emphasize"))
            : $r("sys.color.font_secondary"),
        )
        .maxLines(1)
        .textOverflow({ overflow: TextOverflow.Ellipsis });
    }
  }
  .width("100%")
  .height(56)
  .justifyContent(FlexAlign.Center)
  .alignItems(HorizontalAlign.Center)
  .accessibilityText(
    item.badge !== undefined && item.badge.length > 0 ? `${item.title}, ${item.badge}` : item.title,
  );
}

@Builder
function tabBar(settings: TabSettings) {
  WithTheme({ colorMode: settings.colorMode }) {
    Tabs({ barPosition: BarPosition.End, index: settings.selected }) {
      ForEach(settings.items, (item: TabItem, index: number) => {
        TabContent() {
          Column()
        }.tabBar(tabBarItem(item, index, settings));
      })
    }
    .barMode(BarMode.Fixed)
    .barHeight(56)
    .barBackgroundBlurStyle(BlurStyle.COMPONENT_THICK, { colorMode: settings.colorMode })
    .divider({ strokeWidth: "1px", color: $r("sys.color.comp_divider") })
    .width("100%")
    .height(settings.hidden ? 0 : 56)
    .visibility(settings.hidden ? Visibility.None : Visibility.Visible)
    .scrollable(false)
    .onChange(settings.change)
  };
}

export class NativeTabBar {
  readonly node: FrameNode;
  private readonly builder: BuilderNode<[TabSettings]>;
  private readonly context: TabBarContext;
  private readonly mounted?: Set<NativeTabBar>;
  private settings: TabSettings;
  private closed: boolean = false;
  constructor(
    ui: UIContext,
    context: TabBarContext,
    mounted?: Set<NativeTabBar>,
    colorMode: ThemeColorMode = ThemeColorMode.SYSTEM,
  ) {
    this.context = context;
    this.mounted = mounted;
    this.settings = new TabSettings((index: number) => {
      if (!this.closed && index !== this.settings.selected) {
        this.settings.selected = index;
        this.builder.update(this.cloneSettings());
        context.change(index);
      }
    });
    this.settings.colorMode = colorMode;
    this.builder = new BuilderNode<[TabSettings]>(ui);
    this.builder.build(wrapBuilder(tabBar), this.settings);
    this.node = this.builder.getFrameNode()!;
    this.mounted?.add(this);
    context.height(56);
  }
  private cloneSettings(): TabSettings {
    const next = new TabSettings(this.settings.change);
    next.items = this.settings.items;
    next.selected = this.settings.selected;
    next.hidden = this.settings.hidden;
    next.tint = this.settings.tint;
    next.colorMode = this.settings.colorMode;
    return next;
  }
  setColorMode(colorMode: ThemeColorMode): void {
    if (this.closed || this.settings.colorMode === colorMode) {
      return;
    }
    const next = this.cloneSettings();
    next.colorMode = colorMode;
    this.settings = next;
    this.builder.update(next);
  }
  refresh(): void {
    if (this.closed) {
      return;
    }
    this.builder.updateConfiguration();
  }
  updateProps(changes: TabProperty[]): void {
    if (this.closed) {
      return;
    }
    const next = this.cloneSettings();
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
          next.tint = change.removed || change.value == null ? undefined : (change.value as string);
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
      this.mounted?.delete(this);
      this.builder.dispose();
    }
  }
}
