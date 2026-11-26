//! The DevEco project `package` writes: the template (`template/`, embedded
//! in this program) named from the app's `nsh.json`, the ArkTS package
//! `@nsh/harmonyos` as the module `nsh`, the native library in
//! `entry/libs/<abi>`, and a TSX app's bundle and the app's assets as raw
//! files.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs, io,
    path::{Path, PathBuf},
};

use anyhow::{Context as _, bail};
use percent_encoding::{AsciiSet, CONTROLS, utf8_percent_encode};
use serde_json::{Map, Value, json};

use crate::{
    module_meta::{HarmonyMeta, OhpmDep, Permission, permission_reason_key},
    paths::{join_all, join_relative},
    sdk::{ApiLevel, Arch},
};

/// Result of writing the DevEco Studio project.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ProjectOutput {
    /// Whether product `"default"` in `build-profile.json5` references a signing config.
    pub(crate) signed: bool,
}

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
    /// `"links"`: verified HTTPS domains and paths that open the app (`https://...`).
    pub(crate) links: Vec<nsh_core::url_scheme::WebLink>,
    /// `"fileTypes"`: file types the app declares for system Open With / share delivery.
    pub(crate) file_types: Vec<nsh_core::file_type::FileType>,
    /// Standalone apps declaring WebView can initialize ArkWeb before accepting input.
    webview: bool,
    /// HarmonyOS packaging declarations from `nsh.json` `"harmonyos"` merged
    /// with crates' `[package.metadata.nsh.harmonyos]`.
    pub(crate) harmonyos: HarmonyMeta,
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
        let app_links = nsh_core::url_scheme::AppLinks::from_json(json);
        let schemes = app_links.schemes().to_vec();
        let links = app_links.web().to_vec();
        let file_types = match json.get("fileTypes") {
            Some(raw) => {
                let (parsed, ignored) = nsh_core::file_type::parse_file_types(raw);
                for warning in ignored {
                    eprintln!("warning: {warning}");
                }
                parsed
            }
            None => Vec::new(),
        };
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
            links,
            file_types,
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

/// Signing settings preserved from an existing `build-profile.json5`.
#[derive(Debug, Clone, Default, PartialEq)]
struct PreservedSigning {
    signing_configs: Vec<Value>,
    product_signing_config: Option<String>,
}

/// Reads `app.signingConfigs` and the target product's `signingConfig`
/// reference from an existing `build-profile.json5`, distinguishing:
/// 1. First-time generation (`NotFound`) -> empty signing config.
/// 2. Valid existing profile -> preserved `signingConfigs` array and target
///    product's explicit `signingConfig` reference (if configured).
/// 3. Unparseable profile or invalid/dangling signing reference -> returns an
///    error naming the file, key, and product so the original file stays intact.
fn read_preserved_signing(
    profile_path: &Path,
    target_product: &str,
) -> anyhow::Result<PreservedSigning> {
    let text = match fs::read_to_string(profile_path) {
        Ok(text) => text,
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            return Ok(PreservedSigning::default());
        }
        Err(err) => {
            return Err(err).with_context(|| format!("reading {}", profile_path.display()));
        }
    };
    let root = json5::from_str::<Value>(&text)
        .map_err(|err| anyhow::anyhow!("invalid JSON5 in {}: {err}", profile_path.display()))?;
    let root_obj = root.as_object().with_context(|| {
        format!(
            "{}: root value must be an object (preserving signing config for product `{target_product}`)",
            profile_path.display()
        )
    })?;
    let Some(app_val) = root_obj.get("app") else {
        return Ok(PreservedSigning::default());
    };
    let app = app_val.as_object().with_context(|| {
        format!(
            "{}: key `app` must be an object (product `{target_product}`)",
            profile_path.display()
        )
    })?;

    let mut signing_configs = Vec::new();
    let mut known_names = BTreeSet::new();
    if let Some(raw_configs) = app.get("signingConfigs")
        && !raw_configs.is_null()
    {
        let list = raw_configs.as_array().with_context(|| {
            format!(
                "{}: key `app.signingConfigs` must be an array (product `{target_product}`)",
                profile_path.display()
            )
        })?;
        for (idx, item) in list.iter().enumerate() {
            let obj = item.as_object().with_context(|| {
                format!(
                    "{}: key `app.signingConfigs[{idx}]` must be an object (product `{target_product}`)",
                    profile_path.display()
                )
            })?;
            let name = obj
                .get("name")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .with_context(|| {
                    format!(
                        "{}: key `app.signingConfigs[{idx}].name` must be a non-empty string (product `{target_product}`)",
                        profile_path.display()
                    )
                })?;
            known_names.insert(name.to_owned());
            signing_configs.push(item.clone());
        }
    }

    let mut product_signing_config = None;
    if let Some(raw_products) = app.get("products")
        && !raw_products.is_null()
    {
        let products = raw_products.as_array().with_context(|| {
            format!(
                "{}: key `app.products` must be an array (product `{target_product}`)",
                profile_path.display()
            )
        })?;
        for (idx, item) in products.iter().enumerate() {
            let obj = item.as_object().with_context(|| {
                format!(
                    "{}: key `app.products[{idx}]` must be an object (product `{target_product}`)",
                    profile_path.display()
                )
            })?;
            if obj.get("name").and_then(Value::as_str) != Some(target_product) {
                continue;
            }
            if let Some(raw_ref) = obj.get("signingConfig")
                && !raw_ref.is_null()
            {
                let name = raw_ref
                    .as_str()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .with_context(|| {
                        format!(
                            "{}: key `app.products[name=\"{target_product}\"].signingConfig` must be a non-empty string",
                            profile_path.display()
                        )
                    })?;
                if !known_names.contains(name) {
                    bail!(
                        "{}: product `{target_product}` references `app.products[name=\"{target_product}\"].signingConfig` = `{name}`, which is not defined in `app.signingConfigs`",
                        profile_path.display()
                    );
                }
                product_signing_config = Some(name.to_owned());
            }
            break;
        }
    }

    Ok(PreservedSigning {
        signing_configs,
        product_signing_config,
    })
}

/// Verifies the rendered `build-profile.json5` in memory before touching the
/// existing file on disk.
fn validate_build_profile(
    rendered: &str,
    expected: &PreservedSigning,
    target_product: &str,
) -> anyhow::Result<()> {
    let parsed: Value = serde_json::from_str(rendered)
        .context("generated build-profile.json5 is not valid JSON")?;
    let actual_configs = parsed
        .pointer("/app/signingConfigs")
        .and_then(Value::as_array)
        .context("generated build-profile.json5 is missing `app.signingConfigs`")?;
    if actual_configs != &expected.signing_configs {
        bail!(
            "generated build-profile.json5 `app.signingConfigs` does not match preserved signing configs"
        );
    }
    let product = parsed
        .pointer("/app/products")
        .and_then(Value::as_array)
        .and_then(|products| {
            products
                .iter()
                .find(|p| p.get("name").and_then(Value::as_str) == Some(target_product))
        })
        .with_context(|| {
            format!("generated build-profile.json5 is missing product `{target_product}`")
        })?;
    let actual_ref = product.get("signingConfig").and_then(Value::as_str);
    if actual_ref != expected.product_signing_config.as_deref() {
        bail!("generated build-profile.json5 product `{target_product}` signingConfig mismatch");
    }
    Ok(())
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

fn build_permissions_and_strings(harmonyos: &HarmonyMeta) -> (String, String) {
    // Haptics has no nsh capability; VIBRATE is a normal permission for every app.
    let mut permissions = vec![
        Permission {
            name: "ohos.permission.INTERNET".to_owned(),
            reason: None,
            used_scene: None,
        },
        Permission {
            name: "ohos.permission.GET_NETWORK_INFO".to_owned(),
            reason: None,
            used_scene: None,
        },
        Permission {
            name: "ohos.permission.VIBRATE".to_owned(),
            reason: None,
            used_scene: None,
        },
    ];
    for extra in &harmonyos.permissions {
        match permissions.iter_mut().find(|p| p.name == extra.name) {
            None => permissions.push(extra.clone()),
            Some(existing) => {
                if extra.reason.is_some() {
                    existing.reason = extra.reason.clone();
                }
                if extra.used_scene.is_some() {
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

/// Characters percent-encoded in RFC 3986 URI path segments (along with all
/// non-ASCII UTF-8 bytes) for OpenHarmony `skill.cpp` raw byte matching.
const LINK_PATH_ENCODE_SET: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'%')
    .add(b'<')
    .add(b'>')
    .add(b'?')
    .add(b'`')
    .add(b'{')
    .add(b'}')
    .add(b'^')
    .add(b'|')
    .add(b'\\')
    .add(b'[')
    .add(b']');

/// Maximum number of `linkFeature` declarations allowed per OpenHarmony bundle.
const MAX_LINK_FEATURES: usize = 150;

fn push_unique_value(list: &mut Vec<Value>, value: Value) {
    if !list.contains(&value) {
        list.push(value);
    }
}

fn push_unique_string(list: &mut Vec<String>, value: String) {
    if !list.contains(&value) {
        list.push(value);
    }
}

/// Translates one NSH [`WebLink`](nsh_core::url_scheme::WebLink) into
/// OpenHarmony `module.json5` skill URI entries.
///
/// OpenHarmony's `Skill::MatchUri` concatenates `scheme://host` + `/` +
/// `path|pathStartWith|pathRegex` verbatim and compares `host` literally without
/// subdomain wildcard matching:
/// - Wildcard hosts (`*.example.com`) cannot be faithfully expressed and fail
///   packaging with a clear diagnostic naming the host.
/// - `paths: []` or `/*` emits a host-only URI matching every path on the host.
/// - Exact root `"/"` uses `"pathRegex": "$"` (never empty `"path": ""`, which
///   OpenHarmony treats as matching every path).
/// - Prefix and exact paths strip only the single leading `/` inserted by the
///   system matcher while preserving meaningful trailing `/` (so `/items/*`
///   becomes `pathStartWith: "items/"` and rejects `/items2/a`).
fn web_link_uris(link: &nsh_core::url_scheme::WebLink) -> anyhow::Result<Vec<Value>> {
    let host = link.host();
    if host.starts_with("*.") {
        bail!(
            "nsh.json links: HarmonyOS App Linking does not support wildcard subdomain host `{host}`; list each explicit subdomain host instead"
        );
    }
    if link.paths().is_empty() || link.paths().iter().any(|p| p == "/*") {
        return Ok(vec![json!({
            "scheme": "https",
            "host": host,
        })]);
    }
    let mut uris = Vec::new();
    for path in link.paths() {
        if path == "/" {
            push_unique_value(
                &mut uris,
                json!({
                    "scheme": "https",
                    "host": host,
                    "pathRegex": "$",
                }),
            );
            continue;
        }
        let (field, raw_rel) = if let Some(prefix) = path.strip_suffix('*') {
            ("pathStartWith", prefix.strip_prefix('/').unwrap_or(prefix))
        } else {
            ("path", path.strip_prefix('/').unwrap_or(path))
        };
        let encoded = utf8_percent_encode(raw_rel, LINK_PATH_ENCODE_SET).to_string();
        push_unique_value(
            &mut uris,
            json!({
                "scheme": "https",
                "host": host,
                field: encoded,
            }),
        );
        if encoded != raw_rel {
            push_unique_value(
                &mut uris,
                json!({
                    "scheme": "https",
                    "host": host,
                    field: raw_rel,
                }),
            );
        }
    }
    Ok(uris)
}

/// Maps a lowercase file extension (without leading dot) to an OpenHarmony
/// preset Uniform Type Descriptor (UTD) when one exists.
fn preset_utd_for_extension(ext: &str) -> Option<&'static str> {
    match ext {
        "txt" | "log" | "ini" | "cfg" | "conf" => Some("general.plain-text"),
        "md" | "markdown" => Some("general.markdown"),
        "html" | "htm" => Some("general.html"),
        "xml" => Some("general.xml"),
        "json" => Some("general.json"),
        "yaml" | "yml" => Some("general.yaml"),
        "csv" => Some("general.comma-separated-values-text"),
        "tsv" => Some("general.tab-separated-values-text"),
        "css" => Some("general.css"),
        "js" | "mjs" | "cjs" => Some("general.java-script"),
        "ts" | "mts" | "cts" => Some("general.type-script"),
        "rtf" => Some("general.rtf"),
        "pdf" => Some("com.adobe.pdf"),
        "doc" => Some("com.microsoft.word.doc"),
        "docx" => Some("org.openxmlformats.wordprocessingml.document"),
        "xls" => Some("com.microsoft.excel.xls"),
        "xlsx" => Some("org.openxmlformats.spreadsheetml.sheet"),
        "ppt" => Some("com.microsoft.powerpoint.ppt"),
        "pptx" => Some("org.openxmlformats.presentationml.presentation"),
        "epub" => Some("org.idpf.epub-container"),
        "png" => Some("general.png"),
        "jpg" | "jpeg" | "jpe" => Some("general.jpeg"),
        "gif" => Some("general.gif"),
        "webp" => Some("org.webmproject.webp"),
        "bmp" => Some("com.microsoft.bmp"),
        "svg" => Some("general.svg"),
        "ico" => Some("com.microsoft.ico"),
        "tif" | "tiff" => Some("general.tiff"),
        "heic" => Some("general.heic"),
        "heif" => Some("general.heif"),
        "avif" => Some("general.avif"),
        "mp4" | "m4v" => Some("general.mpeg-4"),
        "mov" => Some("com.apple.quicktime-movie"),
        "avi" => Some("general.avi"),
        "mkv" => Some("org.matroska.mkv"),
        "webm" => Some("org.webmproject.webm"),
        "3gp" => Some("general.3gpp"),
        "mp3" => Some("general.mp3"),
        "wav" => Some("com.microsoft.waveform-audio"),
        "aac" => Some("general.aac"),
        "m4a" => Some("general.mpeg-4-audio"),
        "flac" => Some("general.flac"),
        "ogg" | "oga" => Some("general.ogg"),
        "opus" => Some("general.opus"),
        "zip" => Some("general.zip-archive"),
        "tar" => Some("general.tar-archive"),
        "gz" | "gzip" => Some("org.gnu.gnu-zip-archive"),
        "bz2" => Some("general.bz2-archive"),
        "7z" => Some("general.7z-archive"),
        "rar" => Some("general.rar-archive"),
        _ => None,
    }
}

/// Maps a MIME type (including wildcards like `video/*`, which OpenHarmony's
/// `Skill::MatchUtd` rejects if passed literally) to a preset OpenHarmony UTD.
fn preset_utd_for_mime(mime: &str) -> Option<&'static str> {
    match mime.to_ascii_lowercase().as_str() {
        "image/*" => Some("general.image"),
        "video/*" => Some("general.video"),
        "audio/*" => Some("general.audio"),
        "text/*" => Some("general.text"),
        "*/*" | "application/octet-stream" => Some("general.file"),
        "text/plain" => Some("general.plain-text"),
        "text/markdown" => Some("general.markdown"),
        "text/html" => Some("general.html"),
        "text/xml" | "application/xml" => Some("general.xml"),
        "application/json" | "text/json" => Some("general.json"),
        "text/csv" => Some("general.comma-separated-values-text"),
        "text/css" => Some("general.css"),
        "text/javascript" | "application/javascript" => Some("general.java-script"),
        "application/pdf" => Some("com.adobe.pdf"),
        "image/png" => Some("general.png"),
        "image/jpeg" => Some("general.jpeg"),
        "image/gif" => Some("general.gif"),
        "image/webp" => Some("org.webmproject.webp"),
        "image/bmp" => Some("com.microsoft.bmp"),
        "image/svg+xml" => Some("general.svg"),
        "image/tiff" => Some("general.tiff"),
        "image/heic" => Some("general.heic"),
        "image/heif" => Some("general.heif"),
        "video/mp4" => Some("general.mpeg-4"),
        "video/quicktime" => Some("com.apple.quicktime-movie"),
        "video/x-msvideo" => Some("general.avi"),
        "video/x-matroska" => Some("org.matroska.mkv"),
        "video/webm" => Some("org.webmproject.webm"),
        "audio/mpeg" => Some("general.mp3"),
        "audio/wav" | "audio/x-wav" => Some("com.microsoft.waveform-audio"),
        "audio/aac" => Some("general.aac"),
        "audio/mp4" => Some("general.mpeg-4-audio"),
        "audio/flac" => Some("general.flac"),
        "audio/ogg" => Some("general.ogg"),
        "application/zip" => Some("general.zip-archive"),
        "application/gzip" => Some("org.gnu.gnu-zip-archive"),
        "application/epub+zip" => Some("org.idpf.epub-container"),
        _ => None,
    }
}

/// Maps common Apple UTIs (`contentTypes`) to OpenHarmony preset UTDs, and
/// passes through OpenHarmony `general.*` UTDs directly.
fn resolve_utd_for_content_type(uti: &str) -> Option<String> {
    let lowered = uti.trim().to_ascii_lowercase();
    let mapped = match lowered.as_str() {
        "public.plain-text" => Some("general.plain-text"),
        "public.text" | "public.utf8-plain-text" => Some("general.text"),
        "public.html" => Some("general.html"),
        "public.xml" => Some("general.xml"),
        "public.json" => Some("general.json"),
        "public.comma-separated-values-text" => Some("general.comma-separated-values-text"),
        "com.adobe.pdf" => Some("com.adobe.pdf"),
        "public.image" => Some("general.image"),
        "public.png" => Some("general.png"),
        "public.jpeg" => Some("general.jpeg"),
        "com.compuserve.gif" => Some("general.gif"),
        "public.svg-image" => Some("general.svg"),
        "public.heic" => Some("general.heic"),
        "public.heif" => Some("general.heif"),
        "public.movie" | "public.video" => Some("general.video"),
        "public.mpeg-4" => Some("general.mpeg-4"),
        "com.apple.quicktime-movie" => Some("com.apple.quicktime-movie"),
        "public.audio" => Some("general.audio"),
        "public.mp3" => Some("general.mp3"),
        "public.mpeg-4-audio" => Some("general.mpeg-4-audio"),
        "com.microsoft.waveform-audio" => Some("com.microsoft.waveform-audio"),
        "public.zip-archive" => Some("general.zip-archive"),
        "public.data" | "public.content" | "public.item" => Some("general.file"),
        _ => None,
    };
    if let Some(utd) = mapped {
        return Some(utd.to_owned());
    }
    if lowered.starts_with("general.") && lowered.len() > "general.".len() {
        return Some(lowered);
    }
    None
}

struct FileTypeArtifacts {
    skills: Vec<Value>,
    custom_utd_json: Option<String>,
    runtime_file_types_json: Option<String>,
}

fn sanitize_utd_prefix(bundle_id: &str) -> String {
    let sanitized: String = bundle_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if sanitized.is_empty() {
        "com.example.app".to_owned()
    } else {
        sanitized
    }
}

/// Builds OpenHarmony `viewData` / `sendData` skills, optional custom
/// `arkdata/utd/utd.json5` declarations, and the runtime `nsh-file-types.json`
/// manifest from `nsh.json` `"fileTypes"` so packaging and runtime launch
/// filtering share one exact UTD mapping.
fn build_file_type_skills(
    bundle_id: &str,
    file_types: &[nsh_core::file_type::FileType],
) -> anyhow::Result<FileTypeArtifacts> {
    if file_types.is_empty() {
        return Ok(FileTypeArtifacts {
            skills: Vec::new(),
            custom_utd_json: None,
            runtime_file_types_json: None,
        });
    }
    let utd_prefix = sanitize_utd_prefix(bundle_id);
    let mut view_types: Vec<String> = Vec::new();
    let mut send_utds: Vec<String> = Vec::new();
    let mut runtime_extensions: Vec<String> = Vec::new();
    let mut runtime_mime_types: Vec<String> = Vec::new();
    let mut custom_utds: Vec<Value> = Vec::new();

    for (index, ft) in file_types.iter().enumerate() {
        let mut custom_exts: Vec<String> = Vec::new();
        for ext in &ft.extensions {
            let cleaned = ext.trim().trim_start_matches('.').to_ascii_lowercase();
            if !cleaned.is_empty() {
                push_unique_string(&mut runtime_extensions, cleaned.clone());
            }
            if let Some(utd) = preset_utd_for_extension(&cleaned) {
                push_unique_string(&mut view_types, utd.to_owned());
                push_unique_string(&mut send_utds, utd.to_owned());
            } else if !cleaned.is_empty() {
                custom_exts.push(format!(".{cleaned}"));
            }
        }
        let mut exact_mimes: Vec<String> = Vec::new();
        for mime in &ft.mime_types {
            let lowered = mime.trim().to_ascii_lowercase();
            if !lowered.is_empty() {
                push_unique_string(&mut runtime_mime_types, lowered.clone());
            }
            if let Some(utd) = preset_utd_for_mime(&lowered) {
                push_unique_string(&mut view_types, utd.to_owned());
                push_unique_string(&mut send_utds, utd.to_owned());
            } else if !lowered.is_empty() && !lowered.contains('*') {
                push_unique_string(&mut exact_mimes, lowered.clone());
                if custom_exts.is_empty() {
                    push_unique_string(&mut view_types, lowered);
                }
            }
        }
        for uti in &ft.content_types {
            if let Some(utd) = resolve_utd_for_content_type(uti) {
                push_unique_string(&mut view_types, utd.clone());
                push_unique_string(&mut send_utds, utd);
            }
        }
        if !custom_exts.is_empty() {
            let type_id = format!("{utd_prefix}.nsh_filetype_{index}");
            let mut decl = Map::new();
            decl.insert("TypeId".to_owned(), Value::String(type_id.clone()));
            decl.insert("BelongingToTypes".to_owned(), json!(["general.file"]));
            decl.insert(
                "FilenameExtensions".to_owned(),
                Value::Array(custom_exts.into_iter().map(Value::String).collect()),
            );
            if !exact_mimes.is_empty() {
                decl.insert(
                    "MIMETypes".to_owned(),
                    Value::Array(exact_mimes.into_iter().map(Value::String).collect()),
                );
            }
            decl.insert("Description".to_owned(), Value::String(ft.name.clone()));
            decl.insert("ReferenceURL".to_owned(), Value::String(String::new()));
            custom_utds.push(Value::Object(decl));
            push_unique_string(&mut view_types, type_id.clone());
            push_unique_string(&mut send_utds, type_id);
        }
    }

    if view_types.len() > MAX_LINK_FEATURES {
        bail!(
            "nsh.json fileTypes expands to {} HarmonyOS FileOpen entries (limit is {MAX_LINK_FEATURES})",
            view_types.len()
        );
    }

    let mut skills = Vec::new();
    if !view_types.is_empty() {
        let uris: Vec<Value> = view_types
            .iter()
            .map(|ty| {
                json!({
                    "scheme": "file",
                    "type": ty,
                    "linkFeature": "FileOpen",
                })
            })
            .collect();
        skills.push(json!({
            "actions": ["ohos.want.action.viewData"],
            "uris": uris,
        }));
    }
    if !send_utds.is_empty() {
        let uris: Vec<Value> = send_utds
            .iter()
            .map(|utd| {
                json!({
                    "scheme": "file",
                    "utd": utd,
                    "maxFileSupported": 16,
                })
            })
            .collect();
        skills.push(json!({
            "actions": ["ohos.want.action.sendData"],
            "uris": uris,
        }));
    }
    let custom_utd_json = if custom_utds.is_empty() {
        None
    } else {
        Some(
            serde_json::to_string_pretty(&json!({
                "UniformDataTypeDeclarations": custom_utds,
            }))
            .expect("serializing custom utd.json5"),
        )
    };
    let runtime_file_types_json =
        if send_utds.is_empty() && runtime_extensions.is_empty() && runtime_mime_types.is_empty() {
            None
        } else {
            Some(
                serde_json::to_string_pretty(&json!({
                    "utds": send_utds,
                    "extensions": runtime_extensions,
                    "mimeTypes": runtime_mime_types,
                }))
                .expect("serializing runtime nsh-file-types.json"),
            )
        };
    Ok(FileTypeArtifacts {
        skills,
        custom_utd_json,
        runtime_file_types_json,
    })
}

fn ability_skills_value(
    bundle_id: &str,
    schemes: &[String],
    links: &[nsh_core::url_scheme::WebLink],
    file_types: &[nsh_core::file_type::FileType],
) -> anyhow::Result<(Value, FileTypeArtifacts)> {
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
    if !links.is_empty() {
        let mut uris = Vec::new();
        for link in links {
            for uri in web_link_uris(link)? {
                push_unique_value(&mut uris, uri);
            }
        }
        skills.push(json!({
            "entities": ["entity.system.browsable"],
            "actions": ["ohos.want.action.viewData"],
            "domainVerify": true,
            "uris": uris,
        }));
    }
    let mut file_artifacts = build_file_type_skills(bundle_id, file_types)?;
    skills.extend(std::mem::take(&mut file_artifacts.skills));
    Ok((Value::Array(skills), file_artifacts))
}

fn build_abilities(
    bundle_id: &str,
    schemes: &[String],
    links: &[nsh_core::url_scheme::WebLink],
    file_types: &[nsh_core::file_type::FileType],
    extra: &[Map<String, Value>],
) -> anyhow::Result<(String, FileTypeArtifacts)> {
    let (skills, file_artifacts) = ability_skills_value(bundle_id, schemes, links, file_types)?;
    let mut abilities = vec![json!({
        "name": "EntryAbility",
        "srcEntry": "./ets/entryability/EntryAbility.ets",
        "icon": "$media:app_icon",
        "label": "$string:app_name",
        "startWindowIcon": "$media:app_icon",
        "startWindowBackground": "$color:start_window_background",
        "exported": true,
        "skills": skills,
    })];
    for ability in extra {
        let mut obj = ability.clone();
        obj.entry("icon".to_owned())
            .or_insert_with(|| Value::String("$media:app_icon".to_owned()));
        obj.entry("label".to_owned())
            .or_insert_with(|| Value::String("$string:app_name".to_owned()));
        obj.entry("startWindowIcon".to_owned())
            .or_insert_with(|| Value::String("$media:app_icon".to_owned()));
        obj.entry("startWindowBackground".to_owned())
            .or_insert_with(|| Value::String("$color:start_window_background".to_owned()));
        abilities.push(Value::Object(obj));
    }
    Ok((indent_json(&Value::Array(abilities), 4), file_artifacts))
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

fn fnv1a_32(input: &str) -> u32 {
    let mut hash: u32 = 0x811c_9dc5;
    for b in input.bytes() {
        hash ^= u32::from(b);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

fn copy_ohpm_dir(from: &Path, to: &Path) -> anyhow::Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let name = entry.file_name();
        if matches!(
            name.to_str(),
            Some("oh_modules" | "node_modules" | "build" | ".hvigor" | ".git" | ".nsh" | "target")
        ) {
            continue;
        }
        let path = entry.path();
        let target = to.join(&name);
        if path.is_dir() {
            copy_ohpm_dir(&path, &target)?;
        } else {
            fs::copy(&path, target)?;
        }
    }
    Ok(())
}

fn read_existing_har_module_name(
    manifest: &Path,
    display_path: &Path,
    pkg: &str,
) -> anyhow::Result<Option<String>> {
    match fs::read_to_string(manifest) {
        Ok(text) => {
            let value = json5::from_str::<Value>(&text).map_err(|err| {
                anyhow::anyhow!(
                    "harmonyos.dependencies.{pkg}: invalid JSON5 in {}: {err}",
                    display_path.display()
                )
            })?;
            let module = value
                .get("module")
                .and_then(Value::as_object)
                .with_context(|| {
                    format!(
                        "harmonyos.dependencies.{pkg}: {} is missing object `module`",
                        display_path.display()
                    )
                })?;
            let name = module
                .get("name")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .with_context(|| {
                    format!(
                        "harmonyos.dependencies.{pkg}: {} has missing or empty `module.name`",
                        display_path.display()
                    )
                })?;
            Ok(Some(name.to_owned()))
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err).with_context(|| {
            format!(
                "harmonyos.dependencies.{pkg}: reading {}",
                display_path.display()
            )
        }),
    }
}

/// Pre-validates local ohpm dependencies (existence, HAR JSON5 `module.json5`
/// syntax/name, and module name collisions) before removing any generated files.
fn validate_ohpm_dependencies(harmonyos: &HarmonyMeta) -> anyhow::Result<()> {
    let mut seen_module_names: BTreeMap<String, String> = BTreeMap::from([
        ("entry".to_owned(), "built-in module `entry`".to_owned()),
        (
            "nsh_harmonyos".to_owned(),
            "built-in module `nsh_harmonyos`".to_owned(),
        ),
    ]);
    for (pkg, dep) in &harmonyos.dependencies {
        let OhpmDep::LocalPath(src) = dep else {
            continue;
        };
        if !src.exists() {
            bail!(
                "harmonyos.dependencies.{pkg}: local package not found at {}",
                src.display()
            );
        }
        if src.is_file() {
            continue;
        }
        if !src.is_dir() {
            bail!(
                "harmonyos.dependencies.{pkg}: {} is neither a file nor a directory",
                src.display()
            );
        }
        let hash = fnv1a_32(pkg);
        let default_mod_name = format!("nshm_{hash:08x}");
        let src_manifest = join_all(src, ["src", "main", "module.json5"]);
        let mod_name = read_existing_har_module_name(&src_manifest, &src_manifest, pkg)?
            .unwrap_or(default_mod_name);
        if let Some(other) = seen_module_names.get(&mod_name) {
            bail!(
                "harmonyos.dependencies.{pkg}: HAR module name `{mod_name}` collides with {other} in build-profile.json5"
            );
        }
        seen_module_names.insert(mod_name, format!("harmonyos.dependencies.{pkg}"));
    }
    Ok(())
}

fn ensure_har_module_files(
    dest_root: &Path,
    src_root: &Path,
    pkg: &str,
    default_name: &str,
) -> anyhow::Result<String> {
    let build_profile = dest_root.join("build-profile.json5");
    if !build_profile.exists() {
        write_file(
            &build_profile,
            b"{ \"apiType\": \"stageMode\", \"buildOption\": {}, \"targets\": [{ \"name\": \"default\" }] }\n",
        )?;
    }
    let hvigorfile = dest_root.join("hvigorfile.ts");
    if !hvigorfile.exists() {
        write_file(
            &hvigorfile,
            b"import { harTasks } from \"@ohos/hvigor-ohos-plugin\";\nexport default { system: harTasks, plugins: [] };\n",
        )?;
    }
    let module_json = join_all(dest_root, ["src", "main", "module.json5"]);
    let display_manifest = join_all(src_root, ["src", "main", "module.json5"]);
    if let Some(existing) = read_existing_har_module_name(&module_json, &display_manifest, pkg)? {
        return Ok(existing);
    }
    let content = format!(
        "{{ \"module\": {{ \"name\": \"{}\", \"type\": \"har\", \"deviceTypes\": [\"default\", \"tablet\", \"2in1\"] }} }}\n",
        json_text(default_name),
    );
    write_file(&module_json, content.as_bytes())?;
    Ok(default_name.to_owned())
}

fn stage_ohpm_dependencies(
    out: &Path,
    harmonyos: &HarmonyMeta,
) -> anyhow::Result<(String, String)> {
    let mut deps = Map::new();
    deps.insert(
        "libnsh_platform_harmonyos.so".to_owned(),
        Value::String("file:../nsh/src/main/cpp/types/libnsh_platform_harmonyos".to_owned()),
    );
    deps.insert(
        "@nsh/harmonyos".to_owned(),
        Value::String("file:../nsh".to_owned()),
    );
    let mut extra_modules = String::new();
    let mut seen_module_names: BTreeMap<String, String> = BTreeMap::from([
        ("entry".to_owned(), "built-in module `entry`".to_owned()),
        (
            "nsh_harmonyos".to_owned(),
            "built-in module `nsh_harmonyos`".to_owned(),
        ),
    ]);
    for (pkg, dep) in &harmonyos.dependencies {
        let spec = match dep {
            OhpmDep::Version(version) => version.clone(),
            OhpmDep::LocalPath(src) => {
                if !src.exists() {
                    bail!(
                        "harmonyos.dependencies.{pkg}: local package not found at {}",
                        src.display()
                    );
                }
                // Stage outside `entry/` (at project root `nshm-deps/`, parallel to `nsh/`)
                // and register directory HARs as project modules in root `build-profile.json5`
                // so Hvigor compiles them under their declared package name with
                // `useNormalizedOHMUrl: true`.
                let hash = fnv1a_32(pkg);
                let folder = format!("{}-{hash:08x}", sanitize_dep_dir(pkg));
                let dest_root = join_all(out, ["nshm-deps", &folder]);
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
                    format!("file:../nshm-deps/{folder}/{file_name}")
                } else if src.is_dir() {
                    copy_ohpm_dir(src, &dest_root).with_context(|| {
                        format!(
                            "copying harmonyos.dependencies.{pkg} from {}",
                            src.display()
                        )
                    })?;
                    let default_mod_name = format!("nshm_{hash:08x}");
                    let mod_name =
                        ensure_har_module_files(&dest_root, src, pkg, &default_mod_name)?;
                    if let Some(other) = seen_module_names.get(&mod_name) {
                        bail!(
                            "harmonyos.dependencies.{pkg}: HAR module name `{mod_name}` collides with {other} in build-profile.json5"
                        );
                    }
                    seen_module_names
                        .insert(mod_name.clone(), format!("harmonyos.dependencies.{pkg}"));
                    extra_modules.push_str(&format!(
                        "\n    {{\n      \"name\": \"{}\",\n      \"srcPath\": \"./nshm-deps/{}\"\n    }},",
                        json_text(&mod_name),
                        json_text(&folder),
                    ));
                    format!("file:../nshm-deps/{folder}")
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
    Ok((indent_json(&Value::Object(deps), 2), extra_modules))
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
pub(crate) fn write(
    out: &Path,
    config: &AppConfig,
    contents: &Contents<'_>,
) -> anyhow::Result<ProjectOutput> {
    if config.id.split('.').count() < 3 {
        println!(
            "! nsh.json \"id\" {:?} is the bundle name: HarmonyOS asks for a reverse domain of at \
             least three parts (com.example.app)",
            config.id
        );
    }
    // Validate existing signing config, links/fileTypes, and local HAR manifests
    // BEFORE removing any previously generated directories or files.
    let profile_path = out.join("build-profile.json5");
    let preserved_signing = read_preserved_signing(&profile_path, "default")?;
    let (abilities, file_artifacts) = build_abilities(
        &config.id,
        &config.schemes,
        &config.links,
        &config.file_types,
        &config.harmonyos.abilities,
    )?;
    validate_ohpm_dependencies(&config.harmonyos)?;

    let signing = indent_json(&Value::Array(preserved_signing.signing_configs.clone()), 4);
    let product_signing = match &preserved_signing.product_signing_config {
        Some(name) => format!("\n        \"signingConfig\": \"{}\",", json_text(name)),
        None => String::new(),
    };
    let (runtime_os, sdk_versions) = contents
        .runtime_os
        .profile(contents.compile_api, contents.min_api);

    // What a previous package wrote and this one replaces as a whole.
    for stale in [
        &["nsh"][..],
        &["nshm-deps"],
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

    let (request_permissions, entry_strings) = build_permissions_and_strings(&config.harmonyos);
    let module_extras = build_module_extras(&config.harmonyos);
    let (entry_dependencies, extra_modules) = stage_ohpm_dependencies(out, &config.harmonyos)?;
    let entry_build_option = build_entry_build_option(&config.harmonyos);
    let bundle_resource = if contents.build.join("bundle.nshc").is_file() {
        "bundle.nshc"
    } else {
        "bundle.js"
    };

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
        ("{{extraModules}}", extra_modules),
        ("{{requestPermissions}}", request_permissions),
        ("{{usesWebView}}", config.webview.to_string()),
        ("{{bundleResource}}", bundle_resource.to_owned()),
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
        let rendered = match std::str::from_utf8(bytes) {
            Ok(text) if text.contains("{{") => Some(render_template(text, &substitutions)),
            _ => None,
        };
        if *path == "build-profile.json5" {
            let text = rendered
                .as_deref()
                .context("build-profile.json5 template was not rendered")?;
            validate_build_profile(text, &preserved_signing, "default")?;
            write_file_atomic(&target, text.as_bytes())?;
            continue;
        }
        let out_bytes = match rendered {
            Some(text) => text.into_bytes(),
            None => bytes.to_vec(),
        };
        write_file(&target, &out_bytes)?;
    }
    for (dest_rel, src_file) in &config.harmonyos.ability_sources {
        let dest = join_relative(join_all(out, ["entry", "src", "main"]), dest_rel);
        let bytes = fs::read(src_file)
            .with_context(|| format!("reading ability source {}", src_file.display()))?;
        write_file(&dest, &bytes)?;
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
    if let Some(utd_json) = file_artifacts.custom_utd_json {
        let utd_path = join_all(&rawfile, ["arkdata", "utd", "utd.json5"]);
        write_file(&utd_path, utd_json.as_bytes())?;
    }
    if let Some(runtime_file_types_json) = file_artifacts.runtime_file_types_json {
        write_file(
            &rawfile.join("nsh-file-types.json"),
            runtime_file_types_json.as_bytes(),
        )?;
    }
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
    Ok(ProjectOutput {
        signed: preserved_signing.product_signing_config.is_some(),
    })
}

fn write_file_atomic(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension(format!("json5.tmp.{}", std::process::id()));
    fs::write(&tmp, bytes).with_context(|| format!("writing {}", tmp.display()))?;
    fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
}

fn render_template(template: &str, substitutions: &[(&str, String)]) -> String {
    let mut out = String::with_capacity(template.len() + 256);
    let mut rest = template;
    while let Some(open) = rest.find("{{") {
        out.push_str(&rest[..open]);
        let after_open = &rest[open..];
        if let Some(close) = after_open.find("}}") {
            let token = &after_open[..close + 2];
            if let Some((_, value)) = substitutions.iter().find(|(k, _)| *k == token) {
                out.push_str(value);
            } else {
                out.push_str(token);
            }
            rest = &after_open[close + 2..];
        } else {
            out.push_str(after_open);
            return out;
        }
    }
    out.push_str(rest);
    out
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
            "links": [
                { "host": "example.com" },
                { "host": "shop.example.com", "paths": ["/orders/*", "/checkout"] }
            ],
            "capabilities": { "speech": false }
        });
        let config = AppConfig::from_json(&app_root, &json).unwrap();
        assert_eq!(config.name, "Counter");
        assert_eq!(config.id, "com.example.counter");
        assert_eq!(config.version, "2.1.0");
        assert_eq!(config.build, 7);
        assert_eq!(config.icon, Some(app_root.join("assets").join("icon.png")));
        assert_eq!(config.schemes, vec!["myapp", "my-app.2+x"]);
        assert_eq!(config.links.len(), 2);
        assert_eq!(config.links[0].host(), "example.com");
        assert_eq!(config.links[1].paths(), ["/orders/*", "/checkout"]);
        assert!(config.file_types.is_empty());
        let bare = AppConfig::from_json(&app_root, &serde_json::json!({})).unwrap();
        assert_eq!(
            (bare.name.as_str(), bare.id.as_str(), bare.build),
            ("counter", "com.example.counter", 1)
        );
        assert!(bare.schemes.is_empty());
        assert!(!bare.webview);
        let speech = AppConfig::from_json(
            &std::env::temp_dir().join("apps").join("speech"),
            &serde_json::json!({ "capabilities": { "speech": true, "webview": {} } }),
        )
        .unwrap();
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
    fn keeps_signing_set_up_in_deveco_including_json5_and_selected_product() {
        let dir = std::env::temp_dir().join(format!("nsh-hm-signing-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let profile_path = dir.join("build-profile.json5");

        // Missing file -> first-time generation (empty, unsigned).
        let fresh = read_preserved_signing(&profile_path, "default").unwrap();
        assert!(fresh.signing_configs.is_empty());
        assert_eq!(fresh.product_signing_config, None);

        // JSON5 bare keys, single quotes, comments, trailing commas, `]` in string,
        // two signing configs (`debug`, `release`) and two products (`staging -> debug`, `default -> release`).
        let profile = r#"{
  // Top-level comment
  app: {
    signingConfigs: [
      {
        name: 'debug',
        type: 'HarmonyOS',
        material: { storePassword: 'a]b', certpath: '/Users/me/.ohos/debug.cer', },
      },
      {
        name: "release",
        type: "HarmonyOS",
        /* block comment */
        material: { storePassword: "secret]pass", certpath: "/Users/me/.ohos/release.cer", },
      },
    ],
    products: [
      { name: 'staging', signingConfig: 'debug', },
      { name: 'default', signingConfig: 'release', },
    ],
  },
}"#;
        fs::write(&profile_path, profile).unwrap();
        let preserved = read_preserved_signing(&profile_path, "default").unwrap();
        assert_eq!(preserved.signing_configs.len(), 2);
        assert_eq!(preserved.product_signing_config.as_deref(), Some("release"));

        // Dangling signingConfig reference errors with file + product + key, never leaking secrets.
        let dangling = r#"{
  app: {
    signingConfigs: [{ name: 'debug', material: { storePassword: 'TOP_SECRET_VALUE' } }],
    products: [{ name: 'default', signingConfig: 'release' }],
  }
}"#;
        fs::write(&profile_path, dangling).unwrap();
        let err = read_preserved_signing(&profile_path, "default")
            .unwrap_err()
            .to_string();
        assert!(err.contains("build-profile.json5"), "{err}");
        assert!(err.contains("default"), "{err}");
        assert!(err.contains("release"), "{err}");
        assert!(!err.contains("TOP_SECRET_VALUE"), "{err}");

        // Malformed JSON5 errors without leaking secrets.
        fs::write(&profile_path, "{ app: { signingConfigs: [ broken } }").unwrap();
        let err = read_preserved_signing(&profile_path, "default")
            .unwrap_err()
            .to_string();
        assert!(err.contains("build-profile.json5"), "{err}");

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn writes_the_project() {
        let dir = std::env::temp_dir().join(format!("nsh-hm-project-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
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
                "schemes": ["counterapp"],
                "links": [
                    { "host": "example.com" },
                    { "host": "shop.example.com", "paths": ["/items/*", "/exact", "/"] }
                ]
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
        let written = write(&out, &config, &contents).unwrap();
        assert!(!written.signed);
        let index = join_relative(&out, "entry/src/main/ets/pages/Index.ets");
        let index_source = fs::read_to_string(&index).unwrap();
        assert!(index_source.contains("if (false) {\n      prepareWebEngine();"));
        assert!(index_source.contains("KeyboardAvoidMode.NONE"));
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
        assert!(
            module.contains("ohos.permission.GET_NETWORK_INFO"),
            "{module}"
        );
        assert!(module.contains("ohos.want.action.viewData"), "{module}");
        assert!(module.contains("\"scheme\": \"counterapp\""), "{module}");
        assert!(module.contains("\"domainVerify\": true"), "{module}");
        assert!(module.contains("\"host\": \"example.com\""), "{module}");
        assert!(
            module.contains("\"host\": \"shop.example.com\""),
            "{module}"
        );
        assert!(module.contains("\"pathStartWith\": \"items/\""), "{module}");
        assert!(module.contains("\"path\": \"exact\""), "{module}");
        assert!(module.contains("\"pathRegex\": \"$\""), "{module}");
        assert!(!module.contains("\"pathStartWith\": \"/"), "{module}");
        assert!(!module.contains("\"path\": \"/"), "{module}");
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

        // Signing set up in DevEco (JSON5, two configs, default -> release) survives packaging twice.
        let signed = r#"{
  // DevEco signing
  app: {
    products: [
      { name: 'staging', signingConfig: 'debug' },
      { name: 'default', signingConfig: 'release', runtimeOS: 'HarmonyOS' },
    ],
    signingConfigs: [
      { name: 'debug', material: { cert: 'debug.cer' } },
      { name: 'release', material: { cert: 'release.cer' } },
    ],
  },
}"#;
        fs::write(out.join("build-profile.json5"), signed).unwrap();
        for _ in 0..2 {
            let result = write(&out, &config, &contents).unwrap();
            assert!(result.signed);
            let parsed: Value =
                serde_json::from_str(&fs::read_to_string(out.join("build-profile.json5")).unwrap())
                    .unwrap();
            assert_eq!(parsed["app"]["products"][0]["signingConfig"], "release");
            assert_eq!(parsed["app"]["signingConfigs"].as_array().unwrap().len(), 2);
            assert_eq!(
                parsed["app"]["signingConfigs"][1]["material"]["cert"],
                "release.cer"
            );
        }

        // Corrupt existing build-profile.json5 fails without overwriting the original file.
        let broken = "{ app: { signingConfigs: [ broken } }";
        fs::write(out.join("build-profile.json5"), broken).unwrap();
        assert!(write(&out, &config, &contents).is_err());
        assert_eq!(
            fs::read_to_string(out.join("build-profile.json5")).unwrap(),
            broken
        );
        fs::remove_file(out.join("build-profile.json5")).unwrap();

        // `capabilities.speech: true` alone does NOT emit MICROPHONE in NSH 0.7
        // (MICROPHONE comes from `nsh-extra`'s `speech` feature or explicit `harmonyos.permissions`).
        let webview_only = AppConfig::from_json(
            &dir.join("speech"),
            &serde_json::json!({ "capabilities": { "speech": true, "webview": {} } }),
        )
        .unwrap();
        write(&out, &webview_only, &contents).unwrap();
        assert!(
            fs::read_to_string(index)
                .unwrap()
                .contains("if (true) {\n      prepareWebEngine();")
        );
        let module =
            fs::read_to_string(join_all(&out, ["entry", "src", "main", "module.json5"])).unwrap();
        let parsed: Value = serde_json::from_str(&module).unwrap();
        assert!(!module.contains("ohos.permission.MICROPHONE"), "{module}");
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
            scanner_rel.starts_with("file:../nshm-deps/") && scanner_rel.ends_with("/scanner.har"),
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
        assert!(kit_rel.starts_with("file:../nshm-deps/"), "{kit_rel}");
        let kit_staged = join_relative(out.join("entry"), kit_rel.strip_prefix("file:").unwrap());
        assert!(kit_staged.join("oh-package.json5").is_file());
        assert!(kit_staged.join("build-profile.json5").is_file());
        assert!(kit_staged.join("hvigorfile.ts").is_file());
        assert!(join_all(&kit_staged, ["src", "main", "module.json5"]).is_file());
        let root_profile: Value =
            serde_json::from_str(&fs::read_to_string(out.join("build-profile.json5")).unwrap())
                .unwrap();
        let modules = root_profile["modules"].as_array().unwrap();
        assert!(
            modules.iter().any(|m| m["srcPath"]
                .as_str()
                .is_some_and(|p| p.starts_with("./nshm-deps/"))),
            "{root_profile}"
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

    #[test]
    fn preserves_microphone_reason_injects_ability_defaults_and_avoids_template_injection() {
        let dir = std::env::temp_dir().join(format!("nsh-hm-hardening-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let app_dir = dir.join("app");
        let build = dir.join("build");
        fs::create_dir_all(&app_dir).unwrap();
        fs::create_dir_all(&build).unwrap();
        fs::write(
            app_dir.join("EchoAbility.ets"),
            "export default class EchoAbility {}",
        )
        .unwrap();
        let library = dir.join("lib.so");
        fs::write(&library, b"\x7fELF").unwrap();

        let config = AppConfig::from_json(
            &app_dir,
            &json!({
                "name": "Demo {{abilities}}",
                "id": "com.example.hardening",
                "capabilities": { "speech": true },
                "harmonyos": {
                    "permissions": [
                        { "name": "MICROPHONE", "reason": "Custom mic reason", "when": "always" }
                    ],
                    "abilities": [
                        {
                            "name": "EchoAbility",
                            "srcEntry": "./ets/echo/EchoAbility.ets",
                            "srcFile": "EchoAbility.ets"
                        }
                    ]
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

        // Single-pass render_template must not expand {{abilities}} inside appName.
        let app_strings = fs::read_to_string(join_all(
            &out,
            ["AppScope", "resources", "base", "element", "string.json"],
        ))
        .unwrap();
        assert!(app_strings.contains("Demo {{abilities}}"), "{app_strings}");

        // Extra ability gets mandatory defaults and its .ets source is staged.
        let module: Value = serde_json::from_str(
            &fs::read_to_string(join_all(&out, ["entry", "src", "main", "module.json5"])).unwrap(),
        )
        .unwrap();
        let echo = module["module"]["abilities"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["name"] == "EchoAbility")
            .unwrap();
        assert_eq!(echo["icon"], "$media:app_icon");
        assert_eq!(echo["label"], "$string:app_name");
        assert_eq!(echo["startWindowIcon"], "$media:app_icon");
        assert_eq!(
            echo["startWindowBackground"],
            "$color:start_window_background"
        );
        assert!(
            join_all(
                &out,
                ["entry", "src", "main", "ets", "echo", "EchoAbility.ets"]
            )
            .is_file()
        );

        // MICROPHONE merges cleanly and preserves custom reason + when="always".
        let mic = module["module"]["requestPermissions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["name"] == "ohos.permission.MICROPHONE")
            .unwrap();
        assert_eq!(mic["reason"], "$string:nsh_perm_microphone");
        assert_eq!(mic["usedScene"]["when"], "always");

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn template_prefers_bytecode_bundle_when_present() {
        let dir = std::env::temp_dir().join(format!("nsh-hm-preboot-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let build = dir.join("build");
        fs::create_dir_all(&build).unwrap();
        fs::write(build.join("bundle.js"), "export default 1;").unwrap();
        let library = dir.join("lib.so");
        fs::write(&library, b"\x7fELF").unwrap();
        let config = AppConfig::from_json(
            &dir.join("app"),
            &json!({ "name": "Preboot", "id": "com.example.preboot" }),
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

        // Without bundle.nshc, the template falls back to bundle.js.
        write(&out, &config, &contents).unwrap();
        let index =
            fs::read_to_string(join_relative(&out, "entry/src/main/ets/pages/Index.ets")).unwrap();
        assert!(index.contains("this.surface.loadResource(\"bundle.js\")"));
        assert!(!index.contains("{{"));

        // When bundle.nshc is present, the template loads precompiled bytecode.
        fs::write(build.join("bundle.nshc"), b"\x02bytecode").unwrap();
        write(&out, &config, &contents).unwrap();
        let index =
            fs::read_to_string(join_relative(&out, "entry/src/main/ets/pages/Index.ets")).unwrap();
        assert!(index.contains("this.surface.loadResource(\"bundle.nshc\")"));
        assert!(
            join_all(
                &out,
                [
                    "entry",
                    "src",
                    "main",
                    "resources",
                    "rawfile",
                    "bundle.nshc"
                ]
            )
            .is_file()
        );

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn har_json5_module_name_errors_and_collisions() {
        let dir = std::env::temp_dir().join(format!("nsh-hm-har-json5-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let app_dir = dir.join("app");
        let har = app_dir.join("vendor").join("scanner_pkg");
        fs::create_dir_all(join_all(&har, ["src", "main"])).unwrap();
        fs::write(har.join("oh-package.json5"), "{ name: '@acme/scanner' }").unwrap();
        fs::write(
            join_all(&har, ["src", "main", "module.json5"]),
            r#"{
  // Comment and trailing comma in valid JSON5
  module: {
    name: 'scanner_har_mod',
    type: 'har',
  },
}"#,
        )
        .unwrap();
        let build = dir.join("build");
        fs::create_dir_all(&build).unwrap();
        let library = dir.join("lib.so");
        fs::write(&library, b"\x7fELF").unwrap();
        let config = AppConfig::from_json(
            &app_dir,
            &json!({
                "name": "HarTest",
                "id": "com.example.hartest",
                "harmonyos": {
                    "dependencies": {
                        "@acme/scanner": "file:vendor/scanner_pkg"
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
        let root_profile: Value =
            serde_json::from_str(&fs::read_to_string(out.join("build-profile.json5")).unwrap())
                .unwrap();
        let modules = root_profile["modules"].as_array().unwrap();
        assert!(
            modules.iter().any(|m| m["name"] == "scanner_har_mod"
                && m["srcPath"]
                    .as_str()
                    .is_some_and(|p| p.starts_with("./nshm-deps/"))),
            "{root_profile}"
        );

        // Two local HARs declaring the same module name fail naming both dependencies.
        let har2 = app_dir.join("vendor").join("dup_pkg");
        fs::create_dir_all(join_all(&har2, ["src", "main"])).unwrap();
        fs::write(
            join_all(&har2, ["src", "main", "module.json5"]),
            "{ module: { name: 'scanner_har_mod', type: 'har' } }",
        )
        .unwrap();
        let dup_config = AppConfig::from_json(
            &app_dir,
            &json!({
                "name": "DupHar",
                "id": "com.example.duphar",
                "harmonyos": {
                    "dependencies": {
                        "@acme/scanner": "file:vendor/scanner_pkg",
                        "@acme/dup": "file:vendor/dup_pkg"
                    }
                }
            }),
        )
        .unwrap();
        let err = write(&out, &dup_config, &contents).unwrap_err().to_string();
        assert!(err.contains("scanner_har_mod"), "{err}");
        assert!(
            err.contains("@acme/scanner") && err.contains("@acme/dup"),
            "{err}"
        );

        // Broken JSON5 in HAR manifest reports package and path.
        fs::write(
            join_all(&har, ["src", "main", "module.json5"]),
            "{ module: { name: } }",
        )
        .unwrap();
        let err = write(&out, &config, &contents).unwrap_err().to_string();
        assert!(err.contains("@acme/scanner"), "{err}");
        assert!(err.contains("module.json5"), "{err}");

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn https_links_match_openharmony_skill_semantics_and_reject_wildcards() {
        let dir = std::env::temp_dir().join(format!("nsh-hm-links-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let build = dir.join("build");
        fs::create_dir_all(&build).unwrap();
        let library = dir.join("lib.so");
        fs::write(&library, b"\x7fELF").unwrap();
        let contents = Contents {
            arch: Arch::Arm64,
            library: &library,
            build: &build,
            runtime_os: RuntimeOs::HarmonyOs,
            min_api: ApiLevel::Api20,
            compile_api: MIN_API,
        };

        // Wildcard subdomain host fails with a diagnostic naming the host.
        let wildcard = AppConfig::from_json(
            &dir.join("app"),
            &json!({
                "name": "Wildcard",
                "id": "com.example.wildcard",
                "links": [{ "host": "*.sub.example.com", "paths": ["/items/*"] }]
            }),
        )
        .unwrap();
        let err = write(&dir.join("out-wild"), &wildcard, &contents)
            .unwrap_err()
            .to_string();
        assert!(err.contains("*.sub.example.com"), "{err}");

        // Exact, prefix (with and without trailing /), root-only, all-paths, and UTF-8 paths.
        let config = AppConfig::from_json(
            &dir.join("app"),
            &json!({
                "name": "Links",
                "id": "com.example.links",
                "links": [
                    { "host": "all.example.com", "paths": ["/*"] },
                    { "host": "root.example.com", "paths": ["/"] },
                    { "host": "shop.example.com", "paths": ["/items/*", "/items*", "/exact", "/文档/*"] }
                ]
            }),
        )
        .unwrap();
        let out = dir.join("out");
        write(&out, &config, &contents).unwrap();
        let module: Value = serde_json::from_str(
            &fs::read_to_string(join_all(&out, ["entry", "src", "main", "module.json5"])).unwrap(),
        )
        .unwrap();
        let skills = module["module"]["abilities"][0]["skills"]
            .as_array()
            .unwrap();
        let link_skill = skills
            .iter()
            .find(|s| s.get("domainVerify") == Some(&Value::Bool(true)))
            .unwrap();
        let uris = link_skill["uris"].as_array().unwrap();
        assert!(uris.contains(&json!({ "scheme": "https", "host": "all.example.com" })));
        assert!(uris.contains(&json!({
            "scheme": "https",
            "host": "root.example.com",
            "pathRegex": "$"
        })));
        assert!(uris.contains(&json!({
            "scheme": "https",
            "host": "shop.example.com",
            "pathStartWith": "items/"
        })));
        assert!(uris.contains(&json!({
            "scheme": "https",
            "host": "shop.example.com",
            "pathStartWith": "items"
        })));
        assert!(uris.contains(&json!({
            "scheme": "https",
            "host": "shop.example.com",
            "path": "exact"
        })));
        assert!(uris.contains(&json!({
            "scheme": "https",
            "host": "shop.example.com",
            "pathStartWith": "%E6%96%87%E6%A1%A3/"
        })));

        // Simulate OpenHarmony 6.0 `Skill::MatchUri`: `scheme://host/` + pathStartWith.
        let items_prefix = format!("https://shop.example.com/{}", "items/");
        assert!("https://shop.example.com/items/a".starts_with(&items_prefix));
        assert!(!"https://shop.example.com/items2/a".starts_with(&items_prefix));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn writes_file_types_skills_and_custom_utd_declarations() {
        let dir = std::env::temp_dir().join(format!("nsh-hm-filetypes-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let build = dir.join("build");
        fs::create_dir_all(&build).unwrap();
        let library = dir.join("lib.so");
        fs::write(&library, b"\x7fELF").unwrap();
        let config = AppConfig::from_json(
            &dir.join("app"),
            &json!({
                "name": "Reader",
                "id": "com.example.reader",
                "fileTypes": [
                    { "name": "Documents", "extensions": ["md", "pdf"], "mimeTypes": ["video/*"] },
                    { "name": "Custom Note", "extensions": ["nshnote"], "mimeTypes": ["application/x-nsh-note"] },
                    { "name": "Movie UTI", "contentTypes": ["public.movie"] }
                ]
            }),
        )
        .unwrap();
        let out = dir.join("out");
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
        let skills = module["module"]["abilities"][0]["skills"]
            .as_array()
            .unwrap();
        let view_skill = skills
            .iter()
            .find(|s| {
                s["actions"] == json!(["ohos.want.action.viewData"])
                    && s["uris"]
                        .as_array()
                        .is_some_and(|u| u.iter().any(|x| x["scheme"] == "file"))
            })
            .unwrap();
        let view_uris = view_skill["uris"].as_array().unwrap();
        assert!(view_uris.contains(&json!({
            "scheme": "file",
            "type": "general.markdown",
            "linkFeature": "FileOpen"
        })));
        assert!(view_uris.contains(&json!({
            "scheme": "file",
            "type": "com.adobe.pdf",
            "linkFeature": "FileOpen"
        })));
        // Wildcard MIME `video/*` maps to preset UTD `general.video`.
        assert!(view_uris.contains(&json!({
            "scheme": "file",
            "type": "general.video",
            "linkFeature": "FileOpen"
        })));
        assert!(view_uris.contains(&json!({
            "scheme": "file",
            "type": "com.example.reader.nsh_filetype_1",
            "linkFeature": "FileOpen"
        })));
        let send_skill = skills
            .iter()
            .find(|s| s["actions"] == json!(["ohos.want.action.sendData"]))
            .unwrap();
        let send_uris = send_skill["uris"].as_array().unwrap();
        assert!(send_uris.contains(&json!({
            "scheme": "file",
            "utd": "com.example.reader.nsh_filetype_1",
            "maxFileSupported": 16
        })));

        let utd_path = join_all(
            &out,
            [
                "entry",
                "src",
                "main",
                "resources",
                "rawfile",
                "arkdata",
                "utd",
                "utd.json5",
            ],
        );
        let utd: Value = serde_json::from_str(&fs::read_to_string(&utd_path).unwrap()).unwrap();
        assert_eq!(
            utd["UniformDataTypeDeclarations"][0]["TypeId"],
            "com.example.reader.nsh_filetype_1"
        );
        assert_eq!(
            utd["UniformDataTypeDeclarations"][0]["FilenameExtensions"],
            json!([".nshnote"])
        );
        assert_eq!(
            utd["UniformDataTypeDeclarations"][0]["MIMETypes"],
            json!(["application/x-nsh-note"])
        );
        // HarmonyOS hvigor UTD schema rejects unknown keys such as `IconFile`.
        let allowed_utd_keys = [
            "TypeId",
            "BelongingToTypes",
            "FilenameExtensions",
            "MIMETypes",
            "Description",
            "ReferenceURL",
        ];
        for key in utd["UniformDataTypeDeclarations"][0]
            .as_object()
            .unwrap()
            .keys()
        {
            assert!(
                allowed_utd_keys.contains(&key.as_str()),
                "unexpected utd.json5 key: {key}"
            );
        }

        let runtime_types_path = join_all(
            &out,
            [
                "entry",
                "src",
                "main",
                "resources",
                "rawfile",
                "nsh-file-types.json",
            ],
        );
        let runtime_types: Value =
            serde_json::from_str(&fs::read_to_string(&runtime_types_path).unwrap()).unwrap();
        let runtime_utds = runtime_types["utds"].as_array().unwrap();
        assert!(runtime_utds.contains(&json!("general.markdown")));
        assert!(runtime_utds.contains(&json!("com.adobe.pdf")));
        assert!(runtime_utds.contains(&json!("general.video")));
        assert!(runtime_utds.contains(&json!("com.example.reader.nsh_filetype_1")));
        assert_eq!(runtime_types["extensions"], json!(["md", "pdf", "nshnote"]));
        assert_eq!(
            runtime_types["mimeTypes"],
            json!(["video/*", "application/x-nsh-note"])
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn writes_runtime_file_types_manifest_for_content_types_only() {
        let dir = std::env::temp_dir().join(format!(
            "nsh-hm-filetypes-content-types-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        let build = dir.join("build");
        fs::create_dir_all(&build).unwrap();
        let library = dir.join("lib.so");
        fs::write(&library, b"\x7fELF").unwrap();
        let config = AppConfig::from_json(
            &dir.join("app"),
            &json!({
                "name": "PdfOnly",
                "id": "com.example.pdfonly",
                "fileTypes": [
                    { "name": "PDF", "contentTypes": ["com.adobe.pdf"] },
                    { "name": "Images", "contentTypes": ["public.image"] }
                ]
            }),
        )
        .unwrap();
        let out = dir.join("out");
        let contents = Contents {
            arch: Arch::Arm64,
            library: &library,
            build: &build,
            runtime_os: RuntimeOs::HarmonyOs,
            min_api: ApiLevel::Api20,
            compile_api: MIN_API,
        };
        write(&out, &config, &contents).unwrap();

        let runtime_types_path = join_all(
            &out,
            [
                "entry",
                "src",
                "main",
                "resources",
                "rawfile",
                "nsh-file-types.json",
            ],
        );
        let runtime_types: Value =
            serde_json::from_str(&fs::read_to_string(&runtime_types_path).unwrap()).unwrap();
        assert_eq!(
            runtime_types["utds"],
            json!(["com.adobe.pdf", "general.image"])
        );
        assert_eq!(runtime_types["extensions"], json!([]));
        assert_eq!(runtime_types["mimeTypes"], json!([]));

        let _ = fs::remove_dir_all(&dir);
    }
}
