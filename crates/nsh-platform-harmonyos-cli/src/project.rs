//! The DevEco project `package` writes: the template (`template/`, embedded
//! in this program) named from the app's `nsh.json`, the ArkTS package
//! `@nsh/harmonyos` as the module `nsh`, the native library in
//! `entry/libs/arm64-v8a`, and a TSX app's bundle and the app's assets as
//! raw files.

use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context as _, bail};
use serde_json::Value;

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
    /// Whether the app may recognize speech (`ohos.permission.MICROPHONE`).
    pub(crate) speech: bool,
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
        Ok(Self::from_json(app, &json))
    }

    fn from_json(app: &Path, json: &Value) -> AppConfig {
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
        let speech = match json.get("capabilities") {
            None => true,
            Some(capabilities) => capabilities.get("speech") == Some(&Value::Bool(true)),
        };
        AppConfig {
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
                .map(|icon| app.join(icon))
                .filter(|icon| {
                    icon.extension()
                        .is_some_and(|e| e.eq_ignore_ascii_case("png"))
                }),
            schemes,
            speech,
        }
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
    /// need API 20. HarmonyOS names its SDK by release; an OpenHarmony
    /// project compiles against the SDK hvigor finds (`compile_api`), which
    /// from API 21 on hvigor wants as a `"<api>.0.0"` string.
    fn profile(self, compile_api: u32) -> (&'static str, String) {
        match self {
            RuntimeOs::HarmonyOs => (
                "HarmonyOS",
                "\"compatibleSdkVersion\": \"6.0.0(20)\",\n        \"targetSdkVersion\": \"6.0.0(20)\","
                    .to_owned(),
            ),
            RuntimeOs::OpenHarmony => {
                let api = compile_api.max(MIN_API);
                let version = if api > MIN_API {
                    format!("\"{api}.0.0\"")
                } else {
                    api.to_string()
                };
                (
                    "OpenHarmony",
                    format!(
                        "\"compileSdkVersion\": {version},\n        \"compatibleSdkVersion\": \
                         {MIN_API},\n        \"targetSdkVersion\": {version},"
                    ),
                )
            }
        }
    }
}

/// The oldest API the backend runs on (HarmonyOS 6.0).
pub(crate) const MIN_API: u32 = 20;

/// What goes into the project besides the template.
pub(crate) struct Contents<'a> {
    /// The native library (`libnsh_platform_harmonyos.so`).
    pub(crate) library: &'a Path,
    /// `nsh build`'s output: a TSX app's `bundle.js`, `app.json`,
    /// `capabilities.json`, and the app's `assets/`.
    pub(crate) build: &'a Path,
    /// The OS the HAP targets.
    pub(crate) runtime_os: RuntimeOs,
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

fn request_permissions(speech: bool) -> String {
    if speech {
        "[\n      {\n        \"name\": \"ohos.permission.INTERNET\"\n      },\n      {\n        \"name\": \"ohos.permission.VIBRATE\"\n      },\n      {\n        \"name\": \"ohos.permission.MICROPHONE\",\n        \"reason\": \"$string:nsh_microphone_reason\",\n        \"usedScene\": {\n          \"abilities\": [\n            \"EntryAbility\"\n          ],\n          \"when\": \"inuse\"\n        }\n      }\n    ]"
            .to_owned()
    } else {
        "[\n      {\n        \"name\": \"ohos.permission.INTERNET\"\n      },\n      {\n        \"name\": \"ohos.permission.VIBRATE\"\n      }\n    ]"
            .to_owned()
    }
}

fn ability_skills(schemes: &[String]) -> String {
    let home = "          {\n            \"entities\": [\n              \"entity.system.home\"\n            ],\n            \"actions\": [\n              \"action.system.home\"\n            ]\n          }";
    if schemes.is_empty() {
        return format!("[\n{home}\n        ]");
    }
    let uris = schemes
        .iter()
        .map(|s| {
            format!(
                "              {{\n                \"scheme\": \"{}\"\n              }}",
                json_text(s)
            )
        })
        .collect::<Vec<_>>()
        .join(",\n");
    format!(
        "[\n{home},\n          {{\n            \"entities\": [\n              \"entity.system.browsable\"\n            ],\n            \"actions\": [\n              \"ohos.want.action.viewData\"\n            ],\n            \"uris\": [\n{uris}\n            ]\n          }}\n        ]"
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
    let (runtime_os, sdk_versions) = contents.runtime_os.profile(contents.compile_api);
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
        ("{{requestPermissions}}", request_permissions(config.speech)),
        ("{{skills}}", ability_skills(&config.schemes)),
    ];

    // What a previous package wrote and this one replaces as a whole.
    for stale in [
        "nsh",
        "entry/libs",
        "entry/src/main/resources/rawfile",
        "AppScope/resources/base/media",
    ] {
        let path = out.join(stale);
        if path.exists() {
            fs::remove_dir_all(&path).with_context(|| format!("removing {}", path.display()))?;
        }
    }

    for (path, bytes) in crate::embedded::TEMPLATE {
        if *path == "AppScope/resources/base/media/app_icon.svg" && config.icon.is_some() {
            continue;
        }
        let target = out.join(path);
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
        let target = out.join("AppScope/resources/base/media/app_icon.png");
        write_file(
            &target,
            &fs::read(icon).with_context(|| format!("reading the icon {}", icon.display()))?,
        )?;
    }
    for (path, bytes) in crate::embedded::ARKTS {
        write_file(&out.join("nsh").join(path), bytes)?;
    }

    let libs = out.join("entry/libs/arm64-v8a");
    fs::create_dir_all(&libs)?;
    fs::copy(contents.library, libs.join(crate::runner::LIBRARY))
        .with_context(|| format!("copying {}", contents.library.display()))?;

    let rawfile = out.join("entry/src/main/resources/rawfile");
    fs::create_dir_all(&rawfile)?;
    for name in ["bundle.js", "app.json", "capabilities.json"] {
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
        let json = serde_json::json!({
            "name": "Counter",
            "id": "com.example.counter",
            "version": "2.1.0",
            "build": 7,
            "icon": "icon.png",
            "schemes": ["MyApp", "https", "my-app.2+x", "myapp"],
            "capabilities": { "speech": false }
        });
        let config = AppConfig::from_json(Path::new("/apps/counter"), &json);
        assert_eq!(config.name, "Counter");
        assert_eq!(config.id, "com.example.counter");
        assert_eq!(config.version, "2.1.0");
        assert_eq!(config.build, 7);
        assert_eq!(config.icon, Some(PathBuf::from("/apps/counter/icon.png")));
        assert_eq!(config.schemes, vec!["myapp", "my-app.2+x"]);
        assert!(!config.speech);
        let bare = AppConfig::from_json(Path::new("/apps/counter"), &serde_json::json!({}));
        assert_eq!(
            (
                bare.name.as_str(),
                bare.id.as_str(),
                bare.build,
                bare.speech
            ),
            ("counter", "com.example.counter", 1, true)
        );
        assert!(bare.schemes.is_empty());
    }

    #[test]
    fn names_sdk_versions_as_hvigor_reads_them() {
        let (os, versions) = RuntimeOs::OpenHarmony.profile(26);
        assert_eq!(os, "OpenHarmony");
        assert!(
            versions.contains("\"compileSdkVersion\": \"26.0.0\","),
            "{versions}"
        );
        assert!(
            versions.contains("\"compatibleSdkVersion\": 20,"),
            "{versions}"
        );
        let (_, versions) = RuntimeOs::OpenHarmony.profile(0);
        assert!(
            versions.contains("\"compileSdkVersion\": 20,"),
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
        fs::write(build.join("assets/a.txt"), "a").unwrap();
        let library = dir.join("lib.so");
        fs::write(&library, b"\x7fELF").unwrap();
        let config = AppConfig::from_json(
            Path::new("/apps/counter"),
            &serde_json::json!({
                "name": "Counter \"1\"",
                "id": "com.example.counter",
                "schemes": ["counterapp"]
            }),
        );
        let out = dir.join("project");
        let contents = Contents {
            library: &library,
            build: &build,
            runtime_os: RuntimeOs::HarmonyOs,
            compile_api: MIN_API,
        };
        write(&out, &config, &contents).unwrap();
        let app = fs::read_to_string(out.join("AppScope/app.json5")).unwrap();
        assert!(
            app.contains("\"bundleName\": \"com.example.counter\""),
            "{app}"
        );
        let strings =
            fs::read_to_string(out.join("AppScope/resources/base/element/string.json")).unwrap();
        assert!(strings.contains(r#""value": "Counter \"1\"""#), "{strings}");
        let profile = fs::read_to_string(out.join("build-profile.json5")).unwrap();
        assert!(profile.contains("\"runtimeOS\": \"HarmonyOS\""));
        assert!(profile.contains("\"targetSdkVersion\": \"6.0.0(20)\","));
        let module = fs::read_to_string(out.join("entry/src/main/module.json5")).unwrap();
        assert!(module.contains("\"default\","), "{module}");
        assert!(module.contains("ohos.permission.MICROPHONE"), "{module}");
        assert!(module.contains("ohos.want.action.viewData"), "{module}");
        assert!(module.contains("\"scheme\": \"counterapp\""), "{module}");
        assert!(!module.contains("{{"), "{module}");
        assert!(profile.contains("\"signingConfigs\": []"));
        assert!(!profile.contains("{{"), "{profile}");
        assert!(
            out.join("entry/libs/arm64-v8a/libnsh_platform_harmonyos.so")
                .is_file()
        );
        assert!(
            out.join("entry/src/main/resources/rawfile/bundle.js")
                .is_file()
        );
        assert!(
            out.join("entry/src/main/resources/rawfile/assets/a.txt")
                .is_file()
        );
        assert!(out.join("nsh/index.ets").is_file());
        assert!(
            out.join("nsh/src/main/cpp/types/libnsh_platform_harmonyos/Index.d.ts")
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
        fs::remove_dir_all(&dir).unwrap();
    }
}
