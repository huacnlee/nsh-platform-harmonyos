//! Build-time feature and `nsh/base` part pruning for HarmonyOS runners.
//!
//! Unlike prebuilt runtimes, `nsh-platform-harmonyos` compiles a dedicated
//! `cdylib` runner per app. Before writing `.nsh/harmonyos/runner`:
//! - For a Rust root app (`App::Rust`), no JS bundle runs, so QuickJS,
//!   `nsh-js`, `nsh-package`, `zlib`, and `crypto` are omitted altogether and
//!   only `nsh_base::parts::core` plus the app's own Rust UI sets are
//!   registered (`nsh_rust_app::app_ui_sets`), letting fat LTO dead-strip
//!   every unused `nsh-base` widget and dependency. Optional `nsh-core` /
//!   `nsh-base` features enabled in the app's resolved Cargo dependency graph
//!   (`shader`, `updater`, `websocket`, `math`, `emoji-picker`) are preserved.
//! - For a TSX app (`App::Tsx` / `App::TsxWithCrates`), `nsh build` has already
//!   written `bundle.js` (and any split chunks) to `.nsh/harmonyos/build`.
//!   `nsh_bundler::NativeUse` statically inspects the bundled ES modules using
//!   the same conservative analysis as `nsh package --target web`: dynamic
//!   `import(expr)` or OTA `nsh/updater` falls back to the full `backend`
//!   feature set and full `nsh_base::registry()`, while closed-world bundles
//!   link only the `nsh/base` parts and native features they actually import.

use std::{collections::BTreeSet, fs, path::Path, sync::OnceLock};

use nsh_bundler::{Imported, NativeUse};

use crate::module_meta::CrateFeatures;

/// `nsh`'s exports whose views a shader draws (`nsh-cli/src/web.rs`).
const SHADER_EXPORTS: &[&str] = &[
    "Shader",
    "ShaderEffect",
    "ShaderTransition",
    "ScreenEffect",
    "ScreenTransition",
];

/// Canonical feature order emitted in `runner/Cargo.toml`.
const FEATURE_ORDER: &[&str] = &[
    "js",
    "shader",
    "updater",
    "editor",
    "emoji-picker",
    "highlight",
    "math",
    "plot",
    "rich-editor",
    "term",
    "zlib",
    "crypto",
    "websocket",
];

/// How `runner/lib.rs` registers `nsh/base` and the app's UI sets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum UiRegistration {
    /// Full `nsh/base` (`nsh_base::registry()`, or `nsh_rust_app::ui_sets` in `dev`).
    Full,
    /// Pruned `nsh/base` parts in `nsh_base::parts::ALL` order (`parts = [...]`).
    Parts(Vec<&'static str>),
    /// Pure Rust root app (`rust = nsh_rust_app::app_ui_sets, nsh_rust_app::root`).
    RustApp,
}

/// Selected backend Cargo features and UI registration for a runner build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Plan {
    /// Whether the umbrella `"backend"` feature is used verbatim.
    pub(crate) full_backend: bool,
    /// Granular backend features when `full_backend` is `false`.
    features: BTreeSet<&'static str>,
    /// UI set registration strategy in `runner/lib.rs`.
    pub(crate) ui: UiRegistration,
}

impl Default for Plan {
    fn default() -> Self {
        Self::full()
    }
}

impl Plan {
    /// Unpruned full runtime (`features = ["backend", ...]`, full `nsh/base`).
    pub(crate) fn full() -> Self {
        Self {
            full_backend: true,
            features: BTreeSet::new(),
            ui: UiRegistration::Full,
        }
    }

    /// Initial pruning plan for a pure Rust root app (`App::Rust` when `!dev`):
    /// no JS runtime, minimal `nsh_base::parts::core` + `nsh_rust_app::app_ui_sets`,
    /// plus any native features explicitly retained in `nsh.json` `"features"`.
    pub(crate) fn for_rust_app(keep: &[String]) -> Self {
        if keep.iter().any(|f| f == "backend") {
            return Self::full();
        }
        let mut features = BTreeSet::new();
        apply_direct_keep_features(&mut features, keep);
        Self {
            full_backend: false,
            features,
            ui: UiRegistration::RustApp,
        }
    }

    /// Analyzes `nsh build`'s output directory (`.nsh/harmonyos/build`) for a
    /// TSX app (`App::Tsx` or `App::TsxWithCrates` when `!dev`).
    pub(crate) fn for_tsx_build(
        build_dir: &Path,
        has_linked_rust_crates: bool,
        keep: &[String],
    ) -> Self {
        let entry_js = build_dir.join("bundle.js");
        if !entry_js.is_file() {
            return Self::full();
        }
        // OTA updates via `nsh/updater` may download newer bundles that import
        // modules or UI parts not used by the initial shipped bundle.
        if build_dir.join("assets").join("nsh-updater.json").is_file() {
            return Self::full();
        }
        let Ok(entries) = fs::read_dir(build_dir) else {
            return Self::full();
        };
        let mut js_files = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file()
                && path
                    .extension()
                    .and_then(|e| e.to_str())
                    .is_some_and(|ext| ext == "js")
            {
                js_files.push(path);
            }
        }
        js_files.sort();
        let mut uses = NativeUse::default();
        for path in js_files {
            let Ok(code) = fs::read_to_string(&path) else {
                return Self::full();
            };
            // After rolldown bundling, every remaining bare module specifier is
            // a native NSH / stdlib / `#[native_module]` import.
            uses.add(&code, |spec| {
                !spec.starts_with('.') && !spec.starts_with('/')
            });
        }
        Self::from_native_use_with_keep(&uses, has_linked_rust_crates, keep)
    }

    /// Computes a pruned TSX plan from `NativeUse` and `nsh.json` `"features"`.
    pub(crate) fn from_native_use_with_keep(
        uses: &NativeUse,
        has_linked_rust_crates: bool,
        keep: &[String],
    ) -> Self {
        if uses.computed_import
            || uses.imports.contains_key("nsh/updater")
            || keep.iter().any(|f| f == "updater" || f == "backend")
        {
            return Self::full();
        }

        let mut features: BTreeSet<&'static str> = BTreeSet::from(["js"]);

        if uses.global_crypto
            || uses.imports.contains_key("crypto")
            || uses.imports.contains_key("node:crypto")
        {
            features.insert("crypto");
        }
        if uses.imports.contains_key("zlib") || uses.imports.contains_key("node:zlib") {
            features.insert("zlib");
        }
        if uses.global_websocket
            || uses.imports.contains_key("websocket")
            || uses.imports.contains_key("node:websocket")
        {
            features.insert("websocket");
        }
        if uses.shader
            || uses.imports.get("nsh").is_some_and(|imp| match imp {
                Imported::All => true,
                Imported::Names(names) => {
                    names.iter().any(|n| SHADER_EXPORTS.contains(&n.as_str()))
                }
            })
        {
            features.insert("shader");
        }

        apply_direct_keep_features(&mut features, keep);

        let whole_base = matches!(uses.imports.get(nsh_base::SPECIFIER), Some(Imported::All));
        if whole_base {
            for f in [
                "editor",
                "emoji-picker",
                "highlight",
                "math",
                "plot",
                "rich-editor",
                "term",
            ] {
                features.insert(f);
            }
            return Self {
                full_backend: false,
                features,
                ui: UiRegistration::Full,
            };
        }

        let mut wanted_parts: BTreeSet<&'static str> = BTreeSet::new();
        if needs_base_core(uses, has_linked_rust_crates) {
            wanted_parts.insert("core");
        }
        if uses.imports.contains_key("nsh/base/plot") {
            wanted_parts.insert("plot");
        }
        if let Some(Imported::Names(names)) = uses.imports.get(nsh_base::SPECIFIER) {
            for name in names {
                if let Some(part) = part_of(name) {
                    wanted_parts.insert(part);
                }
            }
        }
        apply_keep_parts(&mut wanted_parts, keep);
        if !wanted_parts.is_empty() {
            wanted_parts.insert("core");
        }

        // Map active `nsh/base` parts to their corresponding Cargo features
        // (`nsh-cli/src/native_features.rs` `BY_GATE`).
        if wanted_parts.contains("editor") || wanted_parts.contains("diff") {
            features.insert("editor");
            features.insert("highlight");
        }
        if wanted_parts.contains("rich_editor") {
            features.insert("rich-editor");
            features.insert("highlight");
            features.insert("math");
        }
        if wanted_parts.contains("plot") {
            features.insert("plot");
        }
        if wanted_parts.contains("term") {
            features.insert("term");
        }
        if wanted_parts.contains("emoji_picker") {
            features.insert("emoji-picker");
        }
        if wanted_parts.contains("text_view") {
            features.insert("highlight");
            features.insert("math");
        }

        let ordered_parts: Vec<&'static str> = nsh_base::parts::ALL
            .iter()
            .map(|(name, _)| *name)
            .filter(|name| wanted_parts.contains(name))
            .collect();

        Self {
            full_backend: false,
            features,
            ui: UiRegistration::Parts(ordered_parts),
        }
    }

    #[cfg(test)]
    pub(crate) fn from_native_use(uses: &NativeUse, has_linked_rust_crates: bool) -> Self {
        Self::from_native_use_with_keep(uses, has_linked_rust_crates, &[])
    }

    /// Merges features required by resolved Rust crates in the runner's OHOS
    /// dependency graph (from `cargo metadata`). Returns `true` if the plan
    /// changed and `runner/Cargo.toml` should be rewritten.
    pub(crate) fn merge_crate_features(&mut self, crates: &CrateFeatures) -> bool {
        if self.full_backend {
            return false;
        }
        let before = self.features.len();
        if crates.shader {
            self.features.insert("shader");
        }
        if crates.updater {
            self.features.insert("updater");
        }
        if crates.websocket {
            self.features.insert("websocket");
        }
        if crates.math {
            self.features.insert("math");
        }
        if crates.emoji_picker {
            self.features.insert("emoji-picker");
        }
        if !matches!(self.ui, UiRegistration::RustApp) {
            if crates.editor {
                self.features.insert("editor");
            }
            if crates.rich_editor {
                self.features.insert("rich-editor");
            }
            if crates.plot {
                self.features.insert("plot");
            }
            if crates.term {
                self.features.insert("term");
            }
            if crates.highlight {
                self.features.insert("highlight");
            }
        }
        self.features.len() != before
    }

    /// Ordered Cargo feature names to enable on `nsh-platform-harmonyos`.
    pub(crate) fn backend_features(&self) -> Vec<&'static str> {
        if self.full_backend {
            return vec!["backend"];
        }
        FEATURE_ORDER
            .iter()
            .copied()
            .filter(|f| self.features.contains(f))
            .collect()
    }
}

fn apply_direct_keep_features(features: &mut BTreeSet<&'static str>, keep: &[String]) {
    for item in keep {
        match item.as_str() {
            "js" => {
                features.insert("js");
            }
            "shader" => {
                features.insert("shader");
            }
            "updater" => {
                features.insert("updater");
            }
            "editor" | "diff" => {
                features.insert("editor");
                features.insert("highlight");
            }
            "emoji-picker" | "emoji_picker" => {
                features.insert("emoji-picker");
            }
            "highlight" => {
                features.insert("highlight");
            }
            "math" => {
                features.insert("math");
            }
            "plot" => {
                features.insert("plot");
            }
            "rich-editor" | "rich_editor" => {
                features.insert("rich-editor");
                features.insert("highlight");
                features.insert("math");
            }
            "text-view" | "text_view" => {
                features.insert("highlight");
                features.insert("math");
            }
            "term" => {
                features.insert("term");
            }
            "zlib" => {
                features.insert("zlib");
            }
            "crypto" => {
                features.insert("crypto");
            }
            "websocket" => {
                features.insert("websocket");
            }
            _ => {}
        }
    }
}

fn apply_keep_parts(wanted_parts: &mut BTreeSet<&'static str>, keep: &[String]) {
    for item in keep {
        match item.as_str() {
            "editor" | "diff" => {
                wanted_parts.insert("editor");
                wanted_parts.insert("diff");
            }
            "rich-editor" | "rich_editor" => {
                wanted_parts.insert("rich_editor");
            }
            "plot" => {
                wanted_parts.insert("plot");
            }
            "term" => {
                wanted_parts.insert("term");
            }
            "emoji-picker" | "emoji_picker" => {
                wanted_parts.insert("emoji_picker");
            }
            "text-view" | "text_view" => {
                wanted_parts.insert("text_view");
            }
            _ => {}
        }
    }
}

fn needs_base_core(uses: &NativeUse, has_linked_rust_crates: bool) -> bool {
    has_linked_rust_crates
        || uses.scrollbar_theme
        || uses.imports.keys().any(|module| {
            module == nsh_base::SPECIFIER
                || module.starts_with(&format!("{}/", nsh_base::SPECIFIER))
        })
}

fn set_exports(set: &nsh_core::element::Registry) -> BTreeSet<String> {
    let mut names: BTreeSet<String> = set.specs().iter().map(|s| s.export.to_string()).collect();
    names.extend(set.class_specs().iter().map(|c| c.name.to_owned()));
    names.extend(set.module_specs().iter().map(|m| m.name.to_owned()));
    names.extend(set.native_value_names().iter().map(|n| (*n).to_owned()));
    names
}

fn part_of(name: &str) -> Option<&'static str> {
    static PARTS: OnceLock<Vec<(&'static str, BTreeSet<String>)>> = OnceLock::new();
    let parts = PARTS.get_or_init(|| {
        let core = set_exports(&nsh_base::registry_of([
            nsh_base::parts::core as nsh_base::parts::Part
        ]));
        nsh_base::parts::ALL
            .iter()
            .filter(|(part_name, _)| *part_name != "core")
            .map(|(part_name, part)| {
                let set =
                    nsh_base::registry_of([nsh_base::parts::core as nsh_base::parts::Part, *part]);
                let names = set_exports(&set)
                    .into_iter()
                    .filter(|export| !core.contains(export))
                    .collect();
                (*part_name, names)
            })
            .collect()
    });
    parts
        .iter()
        .find(|(_, names)| names.contains(name))
        .map(|(part, _)| *part)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn analyze_code(code: &str) -> Plan {
        let mut uses = NativeUse::default();
        uses.add(code, |spec| {
            !spec.starts_with('.') && !spec.starts_with('/')
        });
        Plan::from_native_use(&uses, false)
    }

    #[test]
    fn minimal_tsx_bundle_links_only_js_and_empty_base_parts() {
        let plan = analyze_code(
            r#"import { element, mount } from "nsh:rt";
               import { signal } from "nsh";
               const s = signal(0);
               mount(() => element("view"));"#,
        );
        assert!(!plan.full_backend);
        assert_eq!(plan.backend_features(), vec!["js"]);
        assert_eq!(plan.ui, UiRegistration::Parts(vec![]));
    }

    #[test]
    fn selective_tsx_imports_enable_matching_parts_and_features() {
        let plan = analyze_code(
            r#"import { Button, Sheet, TextView, EmojiPicker } from "nsh/base";
               import { Plot } from "nsh/base/plot";
               import { gzipSync } from "zlib";
               import { Shader } from "nsh";
               const ws = new WebSocket("wss://example.com");
               const id = crypto.randomUUID();"#,
        );
        assert!(!plan.full_backend);
        assert_eq!(
            plan.backend_features(),
            vec![
                "js",
                "shader",
                "emoji-picker",
                "highlight",
                "math",
                "plot",
                "zlib",
                "crypto",
                "websocket",
            ]
        );
        assert_eq!(
            plan.ui,
            UiRegistration::Parts(vec![
                "core",
                "overlays",
                "emoji_picker",
                "text_view",
                "plot"
            ])
        );
    }

    #[test]
    fn dynamic_import_or_updater_falls_back_to_full_backend() {
        let dynamic = analyze_code(r#"const m = "zlib"; export const p = import(m);"#);
        assert!(dynamic.full_backend);
        assert_eq!(dynamic.backend_features(), vec!["backend"]);
        assert_eq!(dynamic.ui, UiRegistration::Full);

        let updater = analyze_code(r#"import { check } from "nsh/updater"; check();"#);
        assert!(updater.full_backend);
        assert_eq!(updater.backend_features(), vec!["backend"]);
    }

    #[test]
    fn namespace_base_import_keeps_all_base_parts_while_pruning_stdlib() {
        let plan = analyze_code(
            r#"import * as base from "nsh/base";
               console.log(Object.keys(base));"#,
        );
        assert!(!plan.full_backend);
        assert_eq!(plan.ui, UiRegistration::Full);
        assert!(!plan.backend_features().contains(&"zlib"));
        assert!(!plan.backend_features().contains(&"crypto"));
        assert!(!plan.backend_features().contains(&"shader"));
    }

    #[test]
    fn rust_app_plan_omits_js_and_merges_resolved_crate_features() {
        let mut plan = Plan::for_rust_app(&[]);
        assert_eq!(plan.backend_features(), Vec::<&str>::new());
        assert_eq!(plan.ui, UiRegistration::RustApp);

        let changed = plan.merge_crate_features(&CrateFeatures {
            shader: true,
            websocket: true,
            ..Default::default()
        });
        assert!(changed);
        assert_eq!(plan.backend_features(), vec!["shader", "websocket"]);
        assert!(!plan.merge_crate_features(&CrateFeatures {
            shader: true,
            websocket: true,
            ..Default::default()
        }));

        let kept = Plan::for_rust_app(&["math".into(), "rich-editor".into()]);
        assert_eq!(
            kept.backend_features(),
            vec!["highlight", "math", "rich-editor"]
        );
    }

    #[test]
    fn rich_editor_keeps_highlight_and_math_and_named_features_keep_parts() {
        let rich = analyze_code(r#"import { RichEditor } from "nsh/base"; RichEditor;"#);
        assert_eq!(
            rich.backend_features(),
            vec!["js", "highlight", "math", "rich-editor"]
        );
        assert_eq!(rich.ui, UiRegistration::Parts(vec!["core", "rich_editor"]));

        let mut uses = NativeUse::default();
        uses.add(
            r#"import { element, mount } from "nsh:rt"; mount(() => element("view"));"#,
            |s| !s.starts_with('.') && !s.starts_with('/'),
        );
        let kept = Plan::from_native_use_with_keep(
            &uses,
            false,
            &["plot".into(), "editor".into(), "crypto".into()],
        );
        assert_eq!(
            kept.backend_features(),
            vec!["js", "editor", "highlight", "plot", "crypto"]
        );
        assert_eq!(
            kept.ui,
            UiRegistration::Parts(vec!["core", "editor", "diff", "plot"])
        );
    }
}
