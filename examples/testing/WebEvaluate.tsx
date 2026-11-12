import { mount, signal, Text, View, WebView, type WebViewRef } from "nsh";

async function expectCode(run: () => Promise<unknown>, expected: string, label: string) {
  try {
    await run();
  } catch (error) {
    const code = (error as { code?: string })?.code ?? "none";
    if (code !== expected) {
      throw new Error(`${label}: expected ${expected}, got ${code}`);
    }
    return;
  }
  throw new Error(`${label}: resolved unexpectedly`);
}

function WebEvaluate() {
  const status = signal("loading");
  const pageHtml = signal(
    "<!doctype html><html><head><title>Eval Probe</title></head><body>ok</body></html>",
  );
  const jsEnabled = signal(true);
  let web!: WebViewRef;
  let disabledWeb!: WebViewRef;
  let toggleWeb!: WebViewRef;
  let webReady = false;
  let toggleReady = false;
  let started = false;
  let nextWebLoad: (() => void) | null = null;

  function maybeStart() {
    if (!started && webReady && toggleReady) {
      void runSuite();
    }
  }

  async function runSuite() {
    if (started) return;
    started = true;
    try {
      if (!(await WebView.evaluateAvailable())) {
        throw new Error("evaluateAvailable returned false");
      }

      const title = await web.evaluate("document.title");
      if (title.value !== "Eval Probe") {
        throw new Error(`unexpected title: ${JSON.stringify(title.value)}`);
      }

      const structured = await web.evaluate('({ ok: true, n: 42, nil: null, list: [1, "鸿蒙"] })');
      const obj = structured.value as { ok: boolean; n: number; nil: null; list: [number, string] };
      if (
        obj.ok !== true ||
        obj.n !== 42 ||
        obj.nil !== null ||
        obj.list[0] !== 1 ||
        obj.list[1] !== "鸿蒙"
      ) {
        throw new Error(`unexpected structured value: ${JSON.stringify(structured.value)}`);
      }

      web.injectJavaScript("globalThis.__injectedValue = 9");
      const injected = await web.evaluate("globalThis.__injectedValue");
      if (injected.value !== 9) {
        throw new Error(`injectJavaScript mismatch: ${JSON.stringify(injected.value)}`);
      }

      const [slow, fast] = await Promise.all([
        web.evaluate("new Promise(resolve => setTimeout(() => resolve('slow'), 80))"),
        web.evaluate("Promise.resolve('fast')"),
      ]);
      if (slow.value !== "slow" || fast.value !== "fast") {
        throw new Error(`promise order mismatch: ${slow.value}, ${fast.value}`);
      }

      const refused = web.evaluate("new Promise(resolve => setTimeout(() => resolve('survived'), 120))");
      web.injectJavaScript("window.location.href = 'https://forbidden.test/'");
      if ((await refused).value !== "survived") {
        throw new Error("refused navigation aborted pending evaluation");
      }

      await expectCode(() => web.evaluate("throw new Error('boom')"), "ERR_NSH_WEBVIEW_SCRIPT", "throw");
      await expectCode(() => web.evaluate("Promise.reject(new Error('nope'))"), "ERR_NSH_WEBVIEW_SCRIPT", "reject");
      await expectCode(() => web.evaluate("function ("), "ERR_NSH_WEBVIEW_SCRIPT", "syntax");
      await expectCode(() => web.evaluate("undefined"), "ERR_NSH_WEBVIEW_SCRIPT", "undefined");
      await expectCode(() => web.evaluate("NaN"), "ERR_NSH_WEBVIEW_SCRIPT", "NaN");
      await expectCode(
        () => web.evaluate("(() => { const a = {}; a.self = a; return a; })()"),
        "ERR_NSH_WEBVIEW_SCRIPT",
        "circular",
      );
      await expectCode(
        () => web.evaluate("'x'.repeat(1048577)"),
        "ERR_NSH_QUOTA_EXCEEDED",
        "quota",
      );

      await expectCode(
        () => web.evaluate("new Promise(() => {})", { timeout: 80 }),
        "ETIMEDOUT",
        "timeout",
      );
      await new Promise((resolve) => setTimeout(resolve, 60));
      const leaked = await web.evaluate(
        "Object.getOwnPropertyNames(globalThis).filter(k => k.startsWith('__nsh_evaluation_')).length",
      );
      if (leaked.value !== 0) {
        throw new Error(`leaked keys after cancel: ${String(leaked.value)}`);
      }

      await expectCode(
        () => disabledWeb.evaluate("1 + 1"),
        "ERR_NSH_WEBVIEW_SCRIPT",
        "disabled",
      );

      const toggled = toggleWeb.evaluate("new Promise(() => {})", { timeout: 5000 });
      await new Promise((resolve) => setTimeout(resolve, 40));
      jsEnabled.set(false);
      await expectCode(() => toggled, "ERR_NSH_WEBVIEW_SCRIPT", "disable mid-flight");

      const reloadedLoad = new Promise<void>((resolve) => {
        nextWebLoad = resolve;
      });
      const reloaded = web.evaluate("new Promise(() => {})", { timeout: 5000 });
      await new Promise((resolve) => setTimeout(resolve, 40));
      web.reload();
      await expectCode(() => reloaded, "ERR_NSH_WEBVIEW_ABORTED", "reload abort");
      await reloadedLoad;

      const sourceAbort = web.evaluate("new Promise(() => {})", { timeout: 5000 });
      await new Promise((resolve) => setTimeout(resolve, 40));
      pageHtml.set("<!doctype html><html><head><title>Next</title></head><body>next</body></html>");
      await expectCode(() => sourceAbort, "ERR_NSH_WEBVIEW_ABORTED", "source abort");

      status.set("pass");
    } catch (error) {
      status.set(`fail: ${error instanceof Error ? error.message : String(error)}`);
    }
  }

  return (
    <View style={{ flex: 1, padding: 24, paddingTop: 48, gap: 8, backgroundColor: "#ffffff" }}>
      <Text>{`Eval: ${status()}`}</Text>
      <WebView
        ref={(node) => {
          web = node;
        }}
        html={pageHtml()}
        onLoad={() => {
          webReady = true;
          const waiter = nextWebLoad;
          nextWebLoad = null;
          waiter?.();
          maybeStart();
        }}
        style={{ height: 60 }}
      />
      <WebView
        ref={(node) => {
          disabledWeb = node;
        }}
        javaScriptEnabled={false}
        html="<!doctype html><html><head><title>Disabled</title></head><body>nojs</body></html>"
        style={{ height: 30 }}
      />
      <WebView
        ref={(node) => {
          toggleWeb = node;
        }}
        javaScriptEnabled={jsEnabled()}
        html="<!doctype html><html><head><title>Toggle</title></head><body>toggle</body></html>"
        onLoad={() => {
          toggleReady = true;
          maybeStart();
        }}
        style={{ height: 30 }}
      />
    </View>
  );
}

mount(WebEvaluate);
