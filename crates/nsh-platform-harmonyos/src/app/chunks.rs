//! Split modules and companion policy are scoped to the accepted HAP entry's directory.
//! Preloading is bounded and shared by source and trusted bytecode entries.
use std::collections::HashSet;
#[cfg(target_env = "ohos")]
use std::sync::{Arc, Weak};

use nsh_engine::{Bundle, BundleCode, Chunk};

#[cfg(target_env = "ohos")]
use crate::app::{
    runtime::{Dispatcher, Message},
    session::Services,
};
#[cfg(target_env = "ohos")]
pub(crate) struct ResourceChunks {
    host: Arc<Services>,
    prefix: String,
}
#[cfg(target_env = "ohos")]
impl ResourceChunks {
    pub(crate) fn new(host: Arc<Services>, entry: &str) -> Arc<Self> {
        let prefix = entry
            .rsplit_once('/')
            .map_or(String::new(), |(p, _)| format!("{p}/"));
        Arc::new(Self { host, prefix })
    }
    pub(crate) fn read_chunk(&self, id: &str) -> Result<nsh_engine::Chunk, String> {
        if !valid_chunk_name(id) {
            return Err("expected a chunk file name".into());
        }
        let name = format!("{}{id}", self.prefix);
        let bytes = self.host.resource(&name).map_err(|e| e.to_string())?;
        let code = String::from_utf8(bytes).map_err(|_| "chunk source must be UTF-8")?;
        let mut chunk = nsh_engine::Chunk::source(id.to_owned(), code);
        if let Ok(bytes) = self.host.resource(&format!("{name}.map"))
            && let Ok(map) = String::from_utf8(bytes)
            && !map.is_empty()
        {
            chunk.source_map = Some(map.into());
        }
        Ok(chunk)
    }
    pub(crate) fn preload(&self, bundle: &mut Bundle) -> Result<(), String> {
        preload(bundle, |name| {
            self.host
                .resource(&format!("{}{name}", self.prefix))
                .map_err(|e| e.to_string())
        })
    }
    pub(crate) fn load(self: &Arc<Self>, id: &str, route: Weak<Dispatcher>) -> bool {
        let id = id.to_owned();
        let loader = self.clone();
        let permit = match self.host.reserve_load() {
            Ok(p) => p,
            Err(e) => {
                if let Some(route) = route.upgrade() {
                    route.send(Message::Chunk(Err((id, e.to_string()))));
                }
                return true;
            }
        };
        let failed_route = route.clone();
        let failed_id = id.clone();
        let result = std::thread::Builder::new()
            .name("nsh-chunk".into())
            .spawn(move || {
                let _permit = permit;
                let result = loader.read_chunk(&id);
                if let Some(route) = route.upgrade() {
                    route.send(Message::Chunk(result.map_err(|e| (id, e))));
                }
            });
        if let Err(error) = result
            && let Some(route) = failed_route.upgrade()
        {
            route.send(Message::Chunk(Err((failed_id, error.to_string()))));
        }
        true
    }
}

const MAX_CHUNKS: usize = 1024;
const MAX_TOTAL_BYTES: usize = 64 << 20;

fn valid_chunk_name(name: &str) -> bool {
    name.strip_suffix(".js").is_some_and(|stem| {
        !stem.is_empty()
            && !stem.starts_with('.')
            && !stem
                .chars()
                .any(|c| c.is_control() || "/\\:?#%".contains(c))
    })
}

#[derive(Default)]
struct Preload {
    queued: Vec<String>,
    seen: HashSet<String>,
    bytes: usize,
}
impl Preload {
    fn queue(&mut self, name: String) -> Result<(), String> {
        if !valid_chunk_name(&name) {
            return Err(format!("invalid chunk name: {name:?}"));
        }
        if self.seen.insert(name.clone()) {
            if self.queued.len() == MAX_CHUNKS {
                return Err(format!("bundle exceeds {MAX_CHUNKS} preloaded chunks"));
            }
            self.queued.push(name);
        }
        Ok(())
    }
    fn account(&mut self, bytes: usize) -> Result<(), String> {
        self.bytes = self
            .bytes
            .checked_add(bytes)
            .filter(|&total| total <= MAX_TOTAL_BYTES)
            .ok_or("bundle preload exceeds 64 MiB")?;
        Ok(())
    }
}

/// Reads optional companions and every static dependency before the VM links the entry.
fn preload(
    bundle: &mut Bundle,
    mut read: impl FnMut(&str) -> Result<Vec<u8>, String>,
) -> Result<(), String> {
    let mut load = Preload::default();
    load.account(match &bundle.code {
        BundleCode::Source(code) => code.len(),
        BundleCode::Bytecode(bytes) => bytes.len(),
    })?;
    if let Ok(bytes) = read("app.json") {
        load.account(bytes.len())?;
        let manifest: serde_json::Value =
            serde_json::from_slice(&bytes).map_err(|e| format!("app.json: {e}"))?;
        if let Some(id) = manifest.get("id").and_then(serde_json::Value::as_str) {
            bundle.app_id = Some(id.to_owned());
        }
        if let Some(schemes) = manifest.get("schemes") {
            bundle.url_schemes = nsh_core::url_scheme::schemes_from_json(schemes);
        }
        if let Some(value) = manifest.get("entryChunks") {
            let names: Vec<String> = serde_json::from_value(value.clone())
                .map_err(|e| format!("app.json entryChunks: {e}"))?;
            if names.len() > MAX_CHUNKS {
                return Err(format!("app.json exceeds {MAX_CHUNKS} entry chunks"));
            }
            for name in names {
                load.queue(name)?;
            }
        }
    }
    if bundle.policy.declared.is_none()
        && let Ok(bytes) = read("capabilities.json")
    {
        load.account(bytes.len())?;
        let value =
            serde_json::from_slice(&bytes).map_err(|e| format!("capabilities.json: {e}"))?;
        bundle.policy.declared = Some(nsh_engine::Capabilities::from_json(&value)?);
    }
    if let BundleCode::Source(code) = &bundle.code {
        for name in static_chunk_imports(code) {
            load.queue(name)?;
        }
    }
    let mut chunks = Vec::new();
    let mut next = 0;
    while next < load.queued.len() {
        let id = load.queued[next].clone();
        next += 1;
        let bytes = read(&id).map_err(|e| format!("chunk {id:?}: {e}"))?;
        load.account(bytes.len())?;
        let code = String::from_utf8(bytes).map_err(|_| "chunk source must be UTF-8")?;
        for name in static_chunk_imports(&code) {
            load.queue(name)?;
        }
        let mut chunk = Chunk::source(id.clone(), code);
        if let Ok(bytes) = read(&format!("{id}.map")) {
            load.account(bytes.len())?;
            if let Ok(map) = String::from_utf8(bytes)
                && !map.is_empty()
            {
                chunk.source_map = Some(map.into());
            }
        }
        chunks.push(chunk);
    }
    bundle.chunks = std::sync::Arc::new(chunks);
    Ok(())
}

/// Compatibility with older source bundles: the bundler emits static imports
/// before the module body, including several imports on one minified line.
fn static_chunk_imports(mut source: &str) -> Vec<String> {
    let mut specs = Vec::new();
    loop {
        source = trivia(source);
        source = source.strip_prefix(';').map_or(source, trivia);
        let (keyword, rest) = word(source);
        if !matches!(keyword, "import" | "export") {
            break;
        }
        source = trivia(rest);
        if source.starts_with(['(', '.'])
            || (keyword == "export" && !source.starts_with(['{', '*']))
        {
            break;
        }
        let mut specifier = keyword == "import" && source.starts_with(['\'', '"']);
        loop {
            source = trivia(source);
            if source.is_empty() || source.starts_with(';') {
                return specs;
            }
            if source.starts_with(['\'', '"']) {
                let Some((value, rest)) = quoted(source) else {
                    return specs;
                };
                source = rest;
                if specifier {
                    if let Some(name) = value.strip_prefix("./") {
                        specs.push(name.to_owned());
                    }
                    break;
                }
            } else {
                let (token, rest) = word(source);
                if token.is_empty() {
                    source = &source[source.chars().next().unwrap().len_utf8()..];
                } else {
                    specifier = token == "from";
                    source = rest;
                }
            }
        }
    }
    specs
}

fn trivia(mut source: &str) -> &str {
    loop {
        source = source.trim_start();
        if source.starts_with("//") {
            source = source.find('\n').map_or("", |end| &source[end + 1..]);
        } else if let Some(comment) = source.strip_prefix("/*") {
            source = comment.find("*/").map_or("", |end| &comment[end + 2..]);
        } else {
            return source;
        }
    }
}

fn word(source: &str) -> (&str, &str) {
    let end = source
        .find(|c: char| !c.is_alphanumeric() && c != '_' && c != '$')
        .unwrap_or(source.len());
    source.split_at(end)
}

fn quoted(source: &str) -> Option<(&str, &str)> {
    let quote = source.as_bytes()[0];
    let mut escaped = false;
    for (i, byte) in source.bytes().enumerate().skip(1) {
        if escaped {
            escaped = false;
        } else if byte == b'\\' {
            escaped = true;
        } else if byte == quote {
            return Some((&source[1..i], &source[i + 1..]));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use serde_json::json;

    use super::*;

    fn resources(files: &[(&str, &str)]) -> impl FnMut(&str) -> Result<Vec<u8>, String> {
        let files: HashMap<_, _> = files
            .iter()
            .map(|(name, value)| (name.to_string(), value.as_bytes().to_vec()))
            .collect();
        move |name| {
            files
                .get(name)
                .cloned()
                .ok_or_else(|| format!("missing {name}"))
        }
    }

    #[test]
    fn manifest_preloads_source_and_bytecode_entries() {
        for code in [
            BundleCode::Source("export {};".into()),
            BundleCode::Bytecode(Vec::new()),
        ] {
            let mut bundle = Bundle::default();
            // Only dependency discovery reads this variant; no bytecode is evaluated.
            bundle.code = code;
            preload(
                &mut bundle,
                resources(&[
                    (
                        "app.json",
                        r#"{"id":"com.test.app","schemes":["myapp"],"entryChunks":["a.js","b.js"]}"#,
                    ),
                    ("a.js", "export const a = 1;"),
                    ("b.js", "export const b = 2;"),
                ]),
            )
            .unwrap();
            assert_eq!(bundle.app_id.as_deref(), Some("com.test.app"));
            assert_eq!(bundle.url_schemes, vec!["myapp".to_owned()]);
            assert_eq!(
                bundle
                    .chunks
                    .iter()
                    .map(|c| c.id.as_str())
                    .collect::<Vec<_>>(),
                ["a.js", "b.js"]
            );
        }
    }

    #[test]
    fn source_dependencies_are_recursive_and_deduplicated_against_manifest() {
        let mut bundle = Bundle::source("import{a}from'./a.js';import{b}from'./b.js';a+b;".into());
        let mut read = resources(&[
            ("app.json", r#"{"entryChunks":["a.js","a.js"]}"#),
            ("a.js", "import './shared.js';export const a = 1;"),
            ("b.js", "export { shared as b } from './shared.js';"),
            ("shared.js", "import './a.js';export const shared = 2;"),
        ]);
        let mut reads = HashMap::<String, usize>::new();
        preload(&mut bundle, |name| {
            *reads.entry(name.to_owned()).or_default() += 1;
            read(name)
        })
        .unwrap();
        assert_eq!(bundle.chunks.len(), 3);
        for name in ["a.js", "b.js", "shared.js"] {
            assert_eq!(reads[name], 1, "{name}");
        }
    }

    #[test]
    fn optional_manifest_preserves_single_file_and_legacy_source_loading() {
        for code in [
            BundleCode::Source("export {};".into()),
            BundleCode::Bytecode(Vec::new()),
        ] {
            let mut bundle = Bundle::default();
            bundle.code = code;
            preload(&mut bundle, resources(&[])).unwrap();
            assert!(bundle.chunks.is_empty());
        }
        let mut bundle = Bundle::source("import './a.js';import './b.js';".into());
        preload(
            &mut bundle,
            resources(&[
                ("a.js", "import './shared.js';"),
                ("b.js", "export {};"),
                ("shared.js", "export {};"),
            ]),
        )
        .unwrap();
        assert_eq!(bundle.chunks.len(), 3);
    }

    #[test]
    fn invalid_manifest_names_and_types_fail_before_reading_chunks() {
        for name in [
            "../a.js", "/a.js", "sub/a.js", "a\\b.js", "a.js?x", "a%2f.js", ".js", "x.nshc",
            "a\0.js",
        ] {
            let manifest = json!({"entryChunks": [name]}).to_string();
            let mut bundle = Bundle::default();
            let error = preload(&mut bundle, |name| {
                assert_eq!(name, "app.json");
                Ok(manifest.as_bytes().to_vec())
            })
            .unwrap_err();
            assert!(error.contains("invalid chunk name"), "{error}");
        }
        for manifest in [r#"{"entryChunks":42}"#, r#"{"entryChunks":[1]}"#, "{"] {
            assert!(preload(&mut Bundle::default(), resources(&[("app.json", manifest)])).is_err());
        }
    }

    #[test]
    fn preload_limits_apply_to_manifests_transitive_chunks_and_bytes() {
        let manifest = json!({"entryChunks": vec!["a.js"; MAX_CHUNKS + 1]}).to_string();
        assert!(
            preload(
                &mut Bundle::default(),
                resources(&[("app.json", &manifest)])
            )
            .is_err()
        );
        let mut bundle = Bundle::source("import './c0.js';".into());
        let error = preload(&mut bundle, |name| {
            if let Some(index) = name.strip_prefix('c').and_then(|s| s.strip_suffix(".js")) {
                let index = index.parse::<usize>().unwrap();
                return Ok(format!("import './c{}.js';", index + 1).into_bytes());
            }
            Err("missing".into())
        })
        .unwrap_err();
        assert!(error.contains("preloaded chunks"), "{error}");
        let mut budget = Preload::default();
        budget.account(MAX_TOTAL_BYTES - 1).unwrap();
        budget.account(1).unwrap();
        assert!(budget.account(1).is_err());
        assert!(Preload::default().account(usize::MAX).is_err());
    }

    #[test]
    fn missing_chunk_or_invalid_utf8_fails_the_load() {
        let mut bundle = Bundle::source("import './missing.js';".into());
        assert!(
            preload(&mut bundle, resources(&[]))
                .unwrap_err()
                .contains("missing.js")
        );
        assert!(
            preload(&mut bundle, |name| {
                if name == "missing.js" {
                    Ok(vec![0xff])
                } else {
                    Err("missing".into())
                }
            })
            .unwrap_err()
            .contains("UTF-8")
        );
    }

    #[test]
    fn scanner_handles_minified_multiline_and_reexported_imports() {
        assert_eq!(
            static_chunk_imports(
                "/* header */import\n{ a }\nfrom './a.js';import'./b.js';export{c}from'./c.js';const text = `import './fake.js'`;"
            ),
            ["a.js", "b.js", "c.js"]
        );
        assert!(static_chunk_imports("import('./lazy.js')").is_empty());
        assert!(static_chunk_imports("import.meta.url").is_empty());
        assert!(static_chunk_imports("// import './fake.js'\nexport const x = 1").is_empty());
    }

    #[test]
    fn companion_capabilities_intersect_host_grants() {
        let mut bundle = Bundle::default();
        bundle.policy.granted = Some(
            nsh_engine::Capabilities::from_json(&json!({
                "network": {"hosts": ["shared.test", "host-only.test"]}, "clipboard": true,
            }))
            .unwrap(),
        );
        preload(
            &mut bundle,
            resources(&[(
                "capabilities.json",
                r#"{"network":{"hosts":["shared.test","app-only.test"]},"picker":true}"#,
            )]),
        )
        .unwrap();
        let access = bundle.policy.access();
        assert!(access.allows_http("GET", "https://shared.test/path"));
        assert!(!access.allows_http("GET", "https://host-only.test/path"));
        assert!(!access.allows_http("GET", "https://app-only.test/path"));
        assert!(!access.allows_clipboard());
        assert!(!access.allows_picker());
    }

    #[test]
    fn existing_declaration_is_preserved_and_malformed_companion_is_rejected() {
        let mut bundle = Bundle::default();
        bundle.policy.declared = Some(nsh_engine::Capabilities::from_json(&json!({})).unwrap());
        preload(
            &mut bundle,
            resources(&[("capabilities.json", r#"{"clipboard":true}"#)]),
        )
        .unwrap();
        assert!(!bundle.policy.access().allows_clipboard());
        assert!(
            preload(
                &mut Bundle::default(),
                resources(&[("capabilities.json", "{")])
            )
            .is_err()
        );
    }
}
