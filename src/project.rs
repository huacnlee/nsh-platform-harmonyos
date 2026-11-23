//! The DevEco project `package` writes: the template (`template/`, embedded
//! in this program) named from the app's `nsh.json`, the ArkTS package
//! `@nsh/harmonyos` as the module `nsh`, the native library in
//! `entry/libs/<abi>`, and a TSX app's bundle and the app's assets as raw
//! files.

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context as _, bail};
use serde_json::{Map, Value, json};

use crate::{
    module_meta::{HarmonyMeta, OhpmDep, Permission, UsedScene, permission_reason_key},
    paths::{join_all, join_relative},
    sdk::{ApiLevel, Arch},
};

/// What the project takes from the app's `nsh.json`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct AppConfig {
    /// `"name"`: the app's label.
    pub(crate) name: String,
    /// `"id"`: the bundle name.
    pub(crate) id: String,
    /// `"version"`: the version name.
    pub(crate) version: String,
    /// `"build"`: the version code.
    pub(crate) build: u64,
    /// `"icon"`: a PNG, relative to the app.
    pub(crate) icon: Option<PathBuf>,
    /// `"schemes"`: URL schemes that open the app (`myapp://...`).
    pub(crate) schemes: Vec<String>,
    /// Explicit `capabilities.speech` opt-in to `ohos.permission.MICROPHONE`.
    pub(crate) speech: bool,
    /// Standalone apps declaring WebView can initialize ArkWeb before accepting input.
    webview: bool,
    /// HarmonyOS packaging declarations from `nsh.json` `"harmonyos"` merged
    /// with crates' `[package.metadata.nsh.harmonyos]`.
    pub(crate) harmonyos: HarmonyMeta,
}

const RESERVED_SCHEMES: &[&str] = &[
    "http",
    "https",
    "ws",
    "wss",
    "file",
    "ftp",
    "mailto",
    "tel",
    "sms",
    "javascript",
    "vbscript",
    "data",
    "blob",
    "about",
    "asset",
    "content",
    "intent",
    "nsh",
];

fn valid_scheme(scheme: &str) -> bool {
    let mut chars = scheme.chars();
    let valid = chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'));
    valid && !RESERVED_SCHEMES.contains(&scheme.to_ascii_lowercase().as_str())
}

impl AppConfig {
    pub(crate) fn read(app: &Path) -> anyhow::Result<AppConfig> {
        let path = app.join("nsh.json");
        let text =
            fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        let json: Value = serde_json::from_str(&text)
            .with_context(|| format!("{} is not JSON", path.display()))?;
        Self::from_json(app, &json)
    }

    fn from_json(app: &Path, json: &Value) -> anyhow::Result<AppConfig> {
        let dir_name = app
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "app".into());
        let build = match &json["build"] {
            Value::Number(n) => n.as_u64(),
            Value::String(s) => s.parse().ok(),
            _ => None,
        };
        let mut schemes = Vec::new();
        if let Some(list) = json["schemes"].as_array() {
            for item in list.iter().filter_map(Value::as_str) {
                if valid_scheme(item) {
                    let lower = item.to_ascii_lowercase();
                    if !schemes.contains(&lower) {
                        schemes.push(lower);
                    }
                }
            }
        }
        let speech = json
            .get("capabilities")
            .and_then(|capabilities| capabilities.get("speech"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let harmonyos = HarmonyMeta::from_app_json(app, json)?;
        Ok(AppConfig {
            name: json["name"]
                .as_str()
                .map_or(dir_name.clone(), str::to_owned),
            id: json["id"]
                .as_str()
                .map_or_else(|| format!("com.example.{dir_name}"), str::to_owned),
            version: json["version"].as_str().unwrap_or("1.0.0").to_owned(),
            build: build.unwrap_or(1),
            icon: json["icon"]
                .as_str()
                .map(|icon| join_relative(app, icon))
                .filter(|icon| {
                    icon.extension()
                        .is_some_and(|e| e.eq_ignore_ascii_case("png"))
                }),
            schemes,
            speech,
            webview: json["capabilities"]["webview"].is_object(),
            harmonyos,
        })
    }
}

/// The OS the HAP targets: HarmonyOS (Huawei's devices, AppGallery) or
/// OpenHarmony.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum RuntimeOs {
    #[default]
    HarmonyOs,
    OpenHarmony,
}

impl RuntimeOs {
    pub(crate) fn parse(value: &str) -> anyhow::Result<RuntimeOs> {
        match value.to_ascii_lowercase().as_str() {
            "harmonyos" => Ok(RuntimeOs::HarmonyOs),
            "openharmony" => Ok(RuntimeOs::OpenHarmony),
            other => bail!("--runtime-os is harmonyos or openharmony, not {other}"),
        }
    }

    /// `runtimeOS` and the SDK versions of `build-profile.json5`. Both
    /// need at least `min_api` (API 20 by default, or 23/26 when `--api` enables
    /// API-gated features). HarmonyOS names API 20..=25 by release and API 26+
    /// as `"<api>.0.0"`; OpenHarmony uses numeric API versions for 10..=25 and
    /// `"<api>.0.0"` strings from API 26 on.
    fn profile(self, compile_api: u32, min_api: ApiLevel) -> (&'static str, String) {
        match self {
            RuntimeOs::HarmonyOs => {
                let version = min_api.harmonyos_version();
                (
                    "HarmonyOS",
                    format!(
                        "\"compatibleSdkVersion\": \"{version}\",\n        \"targetSdkVersion\": \"{version}\","
                    ),
                )
            }
            RuntimeOs::OpenHarmony => {
                let min = min_api.number();
                let api = compile_api.max(min);
                let format_version = |level: u32| {
                    if level >= 26 {
                        format!("\"{level}.0.0\"")
                    } else {
                        level.to_string()
                    }
                };
                let compile = format_version(api);
                let compatible = format_version(min);
                (
                    "OpenHarmony",
                    format!(
                        "\"compileSdkVersion\": {compile},\n        \"compatibleSdkVersion\": \
                         {compatible},\n        \"targetSdkVersion\": {compile},"
                    ),
                )
            }
        }
    }
}

/// The oldest API the backend runs on (HarmonyOS 6.0).
pub(crate) const MIN_API: u32 = ApiLevel::Api20.number();

/// What goes into the project besides the template.
pub(crate) struct Contents<'a> {
    /// Target architecture (`entry/libs/<abi>`).
    pub(crate) arch: Arch,
    /// The native library (`libnsh_platform_harmonyos.so`).
    pub(crate) library: &'a Path,
    /// `nsh build`'s output: a TSX app's `bundle.js`, `app.json`,
    /// `capabilities.json`, and the app's `assets/`.
    pub(crate) build: &'a Path,
    /// The OS the HAP targets.
    pub(crate) runtime_os: RuntimeOs,
    /// The minimum API level the native library and HAP target.
    pub(crate) min_api: ApiLevel,
    /// The API of the OpenHarmony SDK hvigor builds with ([`MIN_API`]
    /// when unknown).
    pub(crate) compile_api: u32,
}

/// The `"signingConfigs": [...]` array of an existing `build-profile.json5`,
/// so signing set up in DevEco Studio survives packaging again.
fn signing_configs(profile: &str) -> Option<&str> {
    let start = profile.find("\"signingConfigs\"")?;
    let open = start + profile[start..].find('[')?;
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (i, c) in profile[open..].char_indices() {
        if in_string {
            match c {
                _ if escaped => escaped = false,
                '\\' => escaped = true,
                '"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match c {
            '"' => in_string = true,
            '[' | '{' => depth += 1,
            ']' | '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&profile[open..=open + i]);
                }
            }
            _ => {}
        }
    }
    None
}

/// The first signing config's `"name"`.
fn signing_name(configs: &str) -> Option<&str> {
    let rest = &configs[configs.find("\"name\"")? + 6..];
    let rest = &rest[rest.find('"')? + 1..];
    Some(&rest[..rest.find('"')?])
}

/// Escapes `value` for a JSON string's contents.
fn json_text(value: &str) -> String {
    let quoted = serde_json::to_string(value).unwrap_or_default();
    quoted[1..quoted.len() - 1].to_owned()
}

fn indent_json(value: &Value, indent: usize) -> String {
    let pretty = serde_json::to_string_pretty(value).expect("serializing valid JSON");
    let pad = " ".repeat(indent);
    let mut out = String::with_capacity(pretty.len() + 64);
    for (i, line) in pretty.lines().enumerate() {
        if i > 0 {
            out.push('\n');
            out.push_str(&pad);
        }
        out.push_str(line);
    }
    out
}

fn build_permissions_and_strings(speech: bool, harmonyos: &HarmonyMeta) -> (String, String) {
    // Haptics has no nsh capability; VIBRATE is a normal permission for every app.
    let mut permissions = vec![
        Permission {
            name: "ohos.permission.INTERNET".to_owned(),
            reason: None,
            used_scene: None,
        },
        Permission {
            name: "ohos.permission.VIBRATE".to_owned(),
            reason: None,
            used_scene: None,
        },
    ];
    if speech {
        permissions.push(Permission {
            name: "ohos.permission.MICROPHONE".to_owned(),
            reason: Some("$string:nsh_microphone_reason".to_owned()),
            used_scene: Some(UsedScene {
                abilities: vec!["EntryAbility".to_owned()],
                when: "inuse".to_owned(),
            }),
        });
    }
    for extra in &harmonyos.permissions {
        match permissions.iter_mut().find(|p| p.name == extra.name) {
            None => permissions.push(extra.clone()),
            Some(existing) => {
                if extra.reason.is_some() || extra.used_scene.is_some() {
                    existing.reason = extra.reason.clone();
                    existing.used_scene = extra.used_scene.clone();
                }
            }
        }
    }

    let mut strings: BTreeMap<String, String> = BTreeMap::new();
    strings.insert(
        "nsh_microphone_reason".to_owned(),
        "Recognize speech in the Native Shell app.".to_owned(),
    );

    let mut perm_values = Vec::with_capacity(permissions.len());
    for perm in permissions {
        let mut obj = Map::new();
        obj.insert("name".to_owned(), Value::String(perm.name.clone()));
        if let Some(reason) = perm.reason {
            let reason_ref = if reason.starts_with("$string:") {
                reason
            } else {
                let key = permission_reason_key(&perm.name);
                strings.entry(key.clone()).or_insert(reason);
                format!("$string:{key}")
            };
            obj.insert("reason".to_owned(), Value::String(reason_ref));
        }
        if let Some(scene) = perm.used_scene {
            obj.insert(
                "usedScene".to_owned(),
                json!({
                    "abilities": scene.abilities,
                    "when": scene.when,
                }),
            );
        }
        perm_values.push(Value::Object(obj));
    }

    // App/crate explicit `harmonyos.strings` override auto-generated defaults.
    for (k, v) in &harmonyos.strings {
        strings.insert(k.clone(), v.clone());
    }

    let string_values: Vec<Value> = strings
        .into_iter()
        .map(|(name, value)| json!({ "name": name, "value": value }))
        .collect();

    (
        indent_json(&Value::Array(perm_values), 4),
        indent_json(&Value::Array(string_values), 2),
    )
}

fn ability_skills_value(schemes: &[String]) -> Value {
    let mut skills = vec![json!({
        "entities": ["entity.system.home"],
        "actions": ["action.system.home"],
    })];
    if !schemes.is_empty() {
        let uris: Vec<Value> = schemes.iter().map(|s| json!({ "scheme": s })).collect();
        skills.push(json!({
            "entities": ["entity.system.browsable"],
            "actions": ["ohos.want.action.viewData"],
            "uris": uris,
        }));
    }
    Value::Array(skills)
}

fn build_abilities(schemes: &[String], extra: &[Map<String, Value>]) -> String {
    let mut abilities = vec![json!({
        "name": "EntryAbility",
        "srcEntry": "./ets/entryability/EntryAbility.ets",
        "icon": "$media:app_icon",
        "label": "$string:app_name",
        "startWindowIcon": "$media:app_icon",
        "startWindowBackground": "$color:start_window_background",
        "exported": true,
        "skills": ability_skills_value(schemes),
    })];
    for ability in extra {
        abilities.push(Value::Object(ability.clone()));
    }
    indent_json(&Value::Array(abilities), 4)
}

fn build_module_extras(harmonyos: &HarmonyMeta) -> String {
    let mut out = String::new();
    if !harmonyos.query_schemes.is_empty() {
        let schemes = Value::Array(
            harmonyos
                .query_schemes
                .iter()
                .cloned()
                .map(Value::String)
                .collect(),
        );
        out.push_str(",\n    \"querySchemes\": ");
        out.push_str(&indent_json(&schemes, 4));
    }
    if !harmonyos.metadata.is_empty() {
        let items: Vec<Value> = harmonyos
            .metadata
            .iter()
            .map(|item| {
                let mut obj = Map::new();
                obj.insert("name".to_owned(), Value::String(item.name.clone()));
                if let Some(v) = &item.value {
                    obj.insert("value".to_owned(), Value::String(v.clone()));
                }
                if let Some(r) = &item.resource {
                    obj.insert("resource".to_owned(), Value::String(r.clone()));
                }
                Value::Object(obj)
            })
            .collect();
        out.push_str(",\n    \"metadata\": ");
        out.push_str(&indent_json(&Value::Array(items), 4));
    }
    if !harmonyos.extension_abilities.is_empty() {
        let exts: Vec<Value> = harmonyos
            .extension_abilities
            .iter()
            .cloned()
            .map(Value::Object)
            .collect();
        out.push_str(",\n    \"extensionAbilities\": ");
        out.push_str(&indent_json(&Value::Array(exts), 4));
    }
    out
}

fn sanitize_dep_dir(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn copy_ohpm_dir(from: &Path, to: &Path) -> anyhow::Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let name = entry.file_name();
        if matches!(
            name.to_str(),
            Some("oh_modules" | "node_modules" | "build" | ".hvigor" | ".git")
        ) {
            continue;
        }
        let target = to.join(&name);
        if entry.file_type()?.is_dir() {
            copy_ohpm_dir(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

fn stage_ohpm_dependencies(out: &Path, harmonyos: &HarmonyMeta) -> anyhow::Result<String> {
    let mut deps = Map::new();
    deps.insert(
        "libnsh_platform_harmonyos.so".to_owned(),
        Value::String("file:../nsh/src/main/cpp/types/libnsh_platform_harmonyos".to_owned()),
    );
    deps.insert(
        "@nsh/harmonyos".to_owned(),
        Value::String("file:../nsh".to_owned()),
    );
    for (index, (pkg, dep)) in harmonyos.dependencies.iter().enumerate() {
        let spec = match dep {
            OhpmDep::Version(version) => version.clone(),
            OhpmDep::LocalPath(src) => {
                if !src.exists() {
                    bail!(
                        "harmonyos.dependencies.{pkg}: local package not found at {}",
                        src.display()
                    );
                }
                let folder = format!("{index}-{}", sanitize_dep_dir(pkg));
                let dest_root = join_all(out, ["entry", "nshm-deps", &folder]);
                if src.is_file() {
                    let file_name = src
                        .file_name()
                        .context("local ohpm package has no file name")?
                        .to_string_lossy()
                        .into_owned();
                    fs::create_dir_all(&dest_root)?;
                    fs::copy(src, dest_root.join(&file_name)).with_context(|| {
                        format!(
                            "copying harmonyos.dependencies.{pkg} from {}",
                            src.display()
                        )
                    })?;
                    format!("file:./nshm-deps/{folder}/{file_name}")
                } else if src.is_dir() {
                    copy_ohpm_dir(src, &dest_root).with_context(|| {
                        format!(
                            "copying harmonyos.dependencies.{pkg} from {}",
                            src.display()
                        )
                    })?;
                    format!("file:./nshm-deps/{folder}")
                } else {
                    bail!(
                        "harmonyos.dependencies.{pkg}: {} is neither a file nor a directory",
                        src.display()
                    );
                }
            }
        };
        deps.insert(pkg.clone(), Value::String(spec));
    }
    Ok(indent_json(&Value::Object(deps), 2))
}

fn build_entry_build_option(harmonyos: &HarmonyMeta) -> String {
    if harmonyos.dependencies.is_empty() {
        return "{}".to_owned();
    }
    let packages: Vec<Value> = harmonyos
        .dependencies
        .keys()
        .cloned()
        .map(Value::String)
        .collect();
    indent_json(
        &json!({
            "arkOptions": {
                "runtimeOnly": {
                    "packages": packages,
                }
            }
        }),
        2,
    )
}

/// Writes the project to `out`.
pub(crate) fn write(out: &Path, config: &AppConfig, contents: &Contents<'_>) -> anyhow::Result<()> {
    if config.id.split('.').count() < 3 {
        println!(
            "! nsh.json \"id\" {:?} is the bundle name: HarmonyOS asks for a reverse domain of at \
             least three parts (com.example.app)",
            config.id
        );
    }
    let previous = fs::read_to_string(out.join("build-profile.json5")).unwrap_or_default();
    let signing = signing_configs(&previous).unwrap_or("[]").to_owned();
    let product_signing = match signing_name(&signing) {
        Some(name) => format!("\"signingConfig\": \"{}\",", json_text(name)),
        None => String::new(),
    };
    let (runtime_os, sdk_versions) = contents
        .runtime_os
        .profile(contents.compile_api, contents.min_api);

    // What a previous package wrote and this one replaces as a whole.
    for stale in [
        &["nsh"][..],
        &["entry", "libs"],
        &["entry", "nshm-deps"],
        &["entry", "src", "main", "resources", "rawfile"],
        &["AppScope", "resources", "base", "media"],
    ] {
        let path = join_all(out, stale.iter().copied());
        if path.exists() {
            fs::remove_dir_all(&path).with_context(|| format!("removing {}", path.display()))?;
        }
    }

    let (request_permissions, entry_strings) =
        build_permissions_and_strings(config.speech, &config.harmonyos);
    let abilities = build_abilities(&config.schemes, &config.harmonyos.abilities);
    let module_extras = build_module_extras(&config.harmonyos);
    let entry_dependencies = stage_ohpm_dependencies(out, &config.harmonyos)?;
    let entry_build_option = build_entry_build_option(&config.harmonyos);

    let substitutions = [
        ("{{bundleName}}", json_text(&config.id)),
        ("{{vendor}}", json_text(&config.name)),
        ("{{appName}}", json_text(&config.name)),
        ("{{versionName}}", json_text(&config.version)),
        ("{{versionCode}}", config.build.to_string()),
        ("{{sdkVersions}}", sdk_versions),
        ("{{runtimeOS}}", runtime_os.to_owned()),
        ("{{signingConfigs}}", signing),
        ("{{productSigning}}", product_signing),
        ("{{requestPermissions}}", request_permissions),
        ("{{usesWebView}}", config.webview.to_string()),
        ("{{abilities}}", abilities),
        ("{{moduleExtras}}", module_extras),
        ("{{entryStrings}}", entry_strings),
        ("{{entryDependencies}}", entry_dependencies),
        ("{{entryBuildOption}}", entry_build_option),
    ];

    for (path, bytes) in crate::embedded::TEMPLATE {
        if *path == "AppScope/resources/base/media/app_icon.svg" && config.icon.is_some() {
            continue;
        }
        let target = join_relative(out, path);
        let bytes = match std::str::from_utf8(bytes) {
            Ok(text) if text.contains("{{") => {
                let mut text = text.to_owned();
                for (key, value) in &substitutions {
                    text = text.replace(key, value);
                }
                text.into_bytes()
            }
            _ => bytes.to_vec(),
        };
        write_file(&target, &bytes)?;
    }
    if let Some(icon) = &config.icon {
        let target = join_all(
            out,
            ["AppScope", "resources", "base", "media", "app_icon.png"],
        );
        write_file(
            &target,
            &fs::read(icon).with_context(|| format!("reading the icon {}", icon.display()))?,
        )?;
    }
    let nsh_dir = out.join("nsh");
    for (path, bytes) in crate::embedded::ARKTS {
        write_file(&join_relative(&nsh_dir, path), bytes)?;
    }

    let libs = join_all(out, ["entry", "libs", contents.arch.abi()]);
    fs::create_dir_all(&libs)?;
    fs::copy(contents.library, libs.join(crate::runner::LIBRARY))
        .with_context(|| format!("copying {}", contents.library.display()))?;

    let rawfile = join_all(out, ["entry", "src", "main", "resources", "rawfile"]);
    fs::create_dir_all(&rawfile)?;
    for name in ["app.json", "capabilities.json"] {
        let source = contents.build.join(name);
        if source.is_file() {
            fs::copy(&source, rawfile.join(name))?;
        }
    }
    if contents.build.is_dir() {
        for entry in fs::read_dir(contents.build)? {
            let entry = entry?;
            if entry.file_type()?.is_file() {
                let path = entry.path();
                if path
                    .extension()
                    .and_then(|e| e.to_str())
                    .is_some_and(|ext| matches!(ext, "js" | "nshc" | "map"))
                {
                    fs::copy(&path, rawfile.join(entry.file_name()))?;
                }
            }
        }
    }
    let assets = contents.build.join("assets");
    if assets.is_dir() {
        copy_dir(&assets, &rawfile.join("assets"))?;
    }
    Ok(())
}

fn write_file(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, bytes).with_context(|| format!("writing {}", path.display()))
}

fn copy_dir(from: &Path, to: &Path) -> anyhow::Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_nsh_json() {
        let app_root = std::env::temp_dir().join("apps").join("counter");
        let json = serde_json::json!({
            "name": "Counter",
            "id": "com.example.counter",
            "version": "2.1.0",
            "build": 7,
            "icon": "assets/icon.png",
            "schemes": ["MyApp", "https", "my-app.2+x", "myapp"],
            "capabilities": { "speech": false }
        });
        let config = AppConfig::from_json(&app_root, &json).unwrap();
        assert_eq!(config.name, "Counter");
        assert_eq!(config.id, "com.example.counter");
        assert_eq!(config.version, "2.1.0");
        assert_eq!(config.build, 7);
        assert_eq!(config.icon, Some(app_root.join("assets").join("icon.png")));
        assert_eq!(config.schemes, vec!["myapp", "my-app.2+x"]);
        assert!(!config.speech);
        let bare = AppConfig::from_json(&app_root, &serde_json::json!({})).unwrap();
        assert_eq!(
            (
                bare.name.as_str(),
                bare.id.as_str(),
                bare.build,
                bare.speech
            ),
            ("counter", "com.example.counter", 1, false)
        );
        assert!(bare.schemes.is_empty());
        assert!(!bare.webview);
        let speech = AppConfig::from_json(
            &std::env::temp_dir().join("apps").join("speech"),
            &serde_json::json!({ "capabilities": { "speech": true, "webview": {} } }),
        )
        .unwrap();
        assert!(speech.speech);
        assert!(speech.webview);
    }

    #[test]
    fn names_sdk_versions_as_hvigor_reads_them() {
        let (os, versions) = RuntimeOs::OpenHarmony.profile(26, ApiLevel::Api20);
        assert_eq!(os, "OpenHarmony");
        assert!(
            versions.contains("\"compileSdkVersion\": \"26.0.0\","),
            "{versions}"
        );
        assert!(
            versions.contains("\"compatibleSdkVersion\": 20,"),
            "{versions}"
        );
        let (_, versions) = RuntimeOs::OpenHarmony.profile(26, ApiLevel::Api23);
        assert!(
            versions.contains("\"compileSdkVersion\": \"26.0.0\","),
            "{versions}"
        );
        assert!(
            versions.contains("\"compatibleSdkVersion\": 23,"),
            "{versions}"
        );
        let (_, versions) = RuntimeOs::OpenHarmony.profile(20, ApiLevel::Api26);
        assert!(
            versions.contains("\"compileSdkVersion\": \"26.0.0\","),
            "{versions}"
        );
        assert!(
            versions.contains("\"compatibleSdkVersion\": \"26.0.0\","),
            "{versions}"
        );
        let (_, versions) = RuntimeOs::OpenHarmony.profile(0, ApiLevel::Api20);
        assert!(
            versions.contains("\"compileSdkVersion\": 20,"),
            "{versions}"
        );

        let (os, versions) = RuntimeOs::HarmonyOs.profile(26, ApiLevel::Api20);
        assert_eq!(os, "HarmonyOS");
        assert!(
            versions.contains("\"compatibleSdkVersion\": \"6.0.0(20)\","),
            "{versions}"
        );
        let (_, versions) = RuntimeOs::HarmonyOs.profile(26, ApiLevel::Api23);
        assert!(
            versions.contains("\"compatibleSdkVersion\": \"6.1.0(23)\","),
            "{versions}"
        );
        assert!(
            versions.contains("\"targetSdkVersion\": \"6.1.0(23)\","),
            "{versions}"
        );
        let (_, versions) = RuntimeOs::HarmonyOs.profile(26, ApiLevel::Api26);
        assert!(
            versions.contains("\"compatibleSdkVersion\": \"26.0.0\","),
            "{versions}"
        );
        assert!(
            versions.contains("\"targetSdkVersion\": \"26.0.0\","),
            "{versions}"
        );
    }

    #[test]
    fn keeps_signing_set_up_in_deveco() {
        let profile = r#"{
  "app": {
    "signingConfigs": [
      {
        "name": "default",
        "type": "HarmonyOS",
        "material": { "storePassword": "a]b", "certpath": "/Users/me/.ohos/config/default.cer" }
      }
    ],
    "products": []
  }
}"#;
        let configs = signing_configs(profile).unwrap();
        assert!(
            configs.starts_with('[') && configs.ends_with(']'),
            "{configs}"
        );
        assert!(configs.contains("default.cer"));
        assert_eq!(signing_name(configs), Some("default"));
        assert_eq!(signing_configs("{}"), None);
        assert_eq!(signing_name("[]"), None);
    }

    #[test]
    fn writes_the_project() {
        let dir = std::env::temp_dir().join(format!("nsh-hm-project-{}", std::process::id()));
        let build = dir.join("build");
        fs::create_dir_all(build.join("assets")).unwrap();
        fs::write(build.join("bundle.js"), "export default 1").unwrap();
        fs::write(build.join("assets").join("a.txt"), "a").unwrap();
        let library = dir.join("lib.so");
        fs::write(&library, b"\x7fELF").unwrap();
        let config = AppConfig::from_json(
            &dir.join("counter"),
            &serde_json::json!({
                "name": "Counter \"1\"",
                "id": "com.example.counter",
                "schemes": ["counterapp"]
            }),
        )
        .unwrap();
        let out = dir.join("project");
        let contents = Contents {
            arch: Arch::Arm64,
            library: &library,
            build: &build,
            runtime_os: RuntimeOs::HarmonyOs,
            min_api: ApiLevel::Api20,
            compile_api: MIN_API,
        };
        write(&out, &config, &contents).unwrap();
        let index = join_relative(&out, "entry/src/main/ets/pages/Index.ets");
        assert!(
            fs::read_to_string(&index)
                .unwrap()
                .contains("if (false) {\n      prepareWebEngine();")
        );
        let app = fs::read_to_string(join_all(&out, ["AppScope", "app.json5"])).unwrap();
        assert!(
            app.contains("\"bundleName\": \"com.example.counter\""),
            "{app}"
        );
        assert!(
            app.contains("\"configuration\": \"$profile:configuration\""),
            "{app}"
        );
        let configuration = fs::read_to_string(join_all(
            &out,
            [
                "AppScope",
                "resources",
                "base",
                "profile",
                "configuration.json",
            ],
        ))
        .unwrap();
        assert!(
            configuration.contains("\"fontSizeScale\": \"followSystem\""),
            "{configuration}"
        );
        let strings = fs::read_to_string(join_all(
            &out,
            ["AppScope", "resources", "base", "element", "string.json"],
        ))
        .unwrap();
        assert!(strings.contains(r#""value": "Counter \"1\"""#), "{strings}");
        let profile = fs::read_to_string(out.join("build-profile.json5")).unwrap();
        assert!(profile.contains("\"runtimeOS\": \"HarmonyOS\""));
        assert!(profile.contains("\"targetSdkVersion\": \"6.0.0(20)\","));
        let module =
            fs::read_to_string(join_all(&out, ["entry", "src", "main", "module.json5"])).unwrap();
        assert!(module.contains("\"default\","), "{module}");
        assert!(!module.contains("ohos.permission.MICROPHONE"), "{module}");
        assert!(module.contains("ohos.permission.VIBRATE"), "{module}");
        assert!(module.contains("ohos.want.action.viewData"), "{module}");
        assert!(module.contains("\"scheme\": \"counterapp\""), "{module}");
        assert!(!module.contains("{{"), "{module}");
        assert!(profile.contains("\"signingConfigs\": []"));
        assert!(!profile.contains("{{"), "{profile}");
        assert!(
            join_all(
                &out,
                ["entry", "libs", "arm64-v8a", "libnsh_platform_harmonyos.so"]
            )
            .is_file()
        );
        for (arch, abi) in [(Arch::Armv7a, "armeabi-v7a"), (Arch::X86_64, "x86_64")] {
            write(&out, &config, &Contents { arch, ..contents }).unwrap();
            assert!(
                join_all(&out, ["entry", "libs", abi, "libnsh_platform_harmonyos.so"]).is_file(),
                "missing {abi}"
            );
            assert!(
                !join_all(&out, ["entry", "libs", "arm64-v8a"]).exists(),
                "stale arm64-v8a should be removed when packaging {abi}"
            );
        }
        assert!(
            join_all(
                &out,
                ["entry", "src", "main", "resources", "rawfile", "bundle.js"]
            )
            .is_file()
        );
        assert!(
            join_all(
                &out,
                [
                    "entry",
                    "src",
                    "main",
                    "resources",
                    "rawfile",
                    "assets",
                    "a.txt"
                ]
            )
            .is_file()
        );
        assert!(join_all(&out, ["nsh", "index.ets"]).is_file());
        assert!(
            join_all(
                &out,
                [
                    "nsh",
                    "src",
                    "main",
                    "cpp",
                    "types",
                    "libnsh_platform_harmonyos",
                    "Index.d.ts"
                ]
            )
            .is_file()
        );

        // Signing set up in DevEco survives packaging again.
        let signed = profile.replace(
            "\"signingConfigs\": []",
            "\"signingConfigs\": [{ \"name\": \"release\", \"material\": {} }]",
        );
        fs::write(out.join("build-profile.json5"), signed).unwrap();
        write(&out, &config, &contents).unwrap();
        let profile = fs::read_to_string(out.join("build-profile.json5")).unwrap();
        assert!(
            profile.contains("\"signingConfig\": \"release\","),
            "{profile}"
        );
        assert!(profile.contains("{ \"name\": \"release\", \"material\": {} }"));

        let speech = AppConfig::from_json(
            &dir.join("speech"),
            &serde_json::json!({ "capabilities": { "speech": true, "webview": {} } }),
        )
        .unwrap();
        write(&out, &speech, &contents).unwrap();
        assert!(
            fs::read_to_string(index)
                .unwrap()
                .contains("if (true) {\n      prepareWebEngine();")
        );
        let module =
            fs::read_to_string(join_all(&out, ["entry", "src", "main", "module.json5"])).unwrap();
        let parsed: Value = serde_json::from_str(&module).unwrap();
        assert!(module.contains("ohos.permission.MICROPHONE"), "{module}");
        assert!(module.contains("ohos.permission.VIBRATE"), "{module}");
        assert!(parsed["module"].get("querySchemes").is_none());
        assert!(parsed["module"].get("metadata").is_none());
        assert!(parsed["module"].get("extensionAbilities").is_none());
        assert!(!module.contains("{{"), "{module}");
        let entry_profile: Value = serde_json::from_str(
            &fs::read_to_string(join_all(&out, ["entry", "build-profile.json5"])).unwrap(),
        )
        .unwrap();
        assert_eq!(entry_profile["buildOption"], json!({}));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn writes_harmonyos_metadata_and_stages_local_ohpm_dependencies() {
        let dir = std::env::temp_dir().join(format!("nsh-hm-meta-project-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let app_dir = dir.join("app");
        let har_file = app_dir.join("vendor").join("scanner.har");
        let local_pkg = app_dir.join("vendor").join("local_kit");
        fs::create_dir_all(har_file.parent().unwrap()).unwrap();
        fs::write(&har_file, b"fake-har").unwrap();
        fs::create_dir_all(&local_pkg).unwrap();
        fs::write(local_pkg.join("oh-package.json5"), "{}").unwrap();
        let build = dir.join("build");
        fs::create_dir_all(&build).unwrap();
        let library = dir.join("lib.so");
        fs::write(&library, b"\x7fELF").unwrap();

        let config = AppConfig::from_json(
            &app_dir,
            &json!({
                "name": "MetaApp",
                "id": "com.example.metaapp",
                "harmonyos": {
                    "permissions": [
                        "GET_NETWORK_INFO",
                        { "name": "CAMERA", "reason": "Scan QR codes.", "when": "inuse" }
                    ],
                    "querySchemes": ["Weixin", "alipays"],
                    "metadata": [
                        { "name": "client_id", "value": "123456" }
                    ],
                    "abilities": [
                        { "name": "ShareAbility", "srcEntry": "./ets/share/ShareAbility.ets" }
                    ],
                    "extensionAbilities": [
                        { "name": "FormAbility", "srcEntry": "./ets/form/FormAbility.ets", "type": "form" }
                    ],
                    "dependencies": {
                        "@ohos/lottie": "^2.0.0",
                        "@acme/scanner": "file:vendor/scanner.har",
                        "@acme/local-kit": "file:vendor/local_kit"
                    },
                    "strings": {
                        "custom_label": "Custom"
                    }
                }
            }),
        )
        .unwrap();

        let out = dir.join("project");
        let contents = Contents {
            arch: Arch::Arm64,
            library: &library,
            build: &build,
            runtime_os: RuntimeOs::HarmonyOs,
            min_api: ApiLevel::Api20,
            compile_api: MIN_API,
        };
        write(&out, &config, &contents).unwrap();

        let module: Value = serde_json::from_str(
            &fs::read_to_string(join_all(&out, ["entry", "src", "main", "module.json5"])).unwrap(),
        )
        .unwrap();
        let perms = module["module"]["requestPermissions"].as_array().unwrap();
        assert!(
            perms
                .iter()
                .any(|p| p["name"] == "ohos.permission.GET_NETWORK_INFO")
        );
        let cam = perms
            .iter()
            .find(|p| p["name"] == "ohos.permission.CAMERA")
            .unwrap();
        assert_eq!(cam["reason"], "$string:nsh_perm_camera");
        assert_eq!(cam["usedScene"]["when"], "inuse");
        assert_eq!(
            module["module"]["querySchemes"],
            json!(["weixin", "alipays"])
        );
        assert_eq!(
            module["module"]["metadata"],
            json!([{ "name": "client_id", "value": "123456" }])
        );
        let abilities = module["module"]["abilities"].as_array().unwrap();
        assert_eq!(abilities.len(), 2);
        assert_eq!(abilities[0]["name"], "EntryAbility");
        assert_eq!(abilities[1]["name"], "ShareAbility");
        let exts = module["module"]["extensionAbilities"].as_array().unwrap();
        assert_eq!(exts.len(), 1);
        assert_eq!(exts[0]["name"], "FormAbility");

        let strings: Value = serde_json::from_str(
            &fs::read_to_string(join_all(
                &out,
                [
                    "entry",
                    "src",
                    "main",
                    "resources",
                    "base",
                    "element",
                    "string.json",
                ],
            ))
            .unwrap(),
        )
        .unwrap();
        let string_list = strings["string"].as_array().unwrap();
        assert!(
            string_list
                .iter()
                .any(|s| s["name"] == "nsh_perm_camera" && s["value"] == "Scan QR codes.")
        );
        assert!(
            string_list
                .iter()
                .any(|s| s["name"] == "custom_label" && s["value"] == "Custom")
        );

        let oh_pkg: Value = serde_json::from_str(
            &fs::read_to_string(join_all(&out, ["entry", "oh-package.json5"])).unwrap(),
        )
        .unwrap();
        let deps = oh_pkg["dependencies"].as_object().unwrap();
        assert_eq!(deps["@nsh/harmonyos"], "file:../nsh");
        assert_eq!(deps["@ohos/lottie"], "^2.0.0");
        let scanner_rel = deps["@acme/scanner"].as_str().unwrap();
        assert!(
            scanner_rel.starts_with("file:./nshm-deps/") && scanner_rel.ends_with("/scanner.har"),
            "{scanner_rel}"
        );
        assert!(
            join_relative(
                out.join("entry"),
                scanner_rel.strip_prefix("file:").unwrap()
            )
            .is_file()
        );
        let kit_rel = deps["@acme/local-kit"].as_str().unwrap();
        assert!(kit_rel.starts_with("file:./nshm-deps/"), "{kit_rel}");
        assert!(
            join_relative(out.join("entry"), kit_rel.strip_prefix("file:").unwrap())
                .join("oh-package.json5")
                .is_file()
        );
        let entry_profile: Value = serde_json::from_str(
            &fs::read_to_string(join_all(&out, ["entry", "build-profile.json5"])).unwrap(),
        )
        .unwrap();
        assert_eq!(
            entry_profile["buildOption"]["arkOptions"]["runtimeOnly"]["packages"],
            json!(["@acme/local-kit", "@acme/scanner", "@ohos/lottie"])
        );

        fs::remove_dir_all(&dir).unwrap();
    }
}
