//! Resolves the Rust crates linked into a HarmonyOS runner for a TSX app:
//!
//! 1. the app's own `"rust"` crates (`nsh.json` `"rust"`),
//! 2. installed Rust nshms pinned in `nsh.lock` (v2, entries with `"module"`),
//!    carrying any Cargo features enabled in `nsh.json` `"dependencies"`, and
//! 3. transitive Rust nshms reachable via normal Cargo dependencies of the
//!    app's own `"rust"` crates (`#[native_module("…")]` + `nsh-core` dep,
//!    matching upstream `nsh-cli`'s Cargo nshm discovery).

use std::{
    collections::{BTreeMap, HashSet},
    env, fs,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context as _, bail};
use serde::Deserialize;
use serde_json::Value;

/// Lock file written beside `nsh.json` by `nsh`.
const LOCK_FILE: &str = "nsh.lock";

/// Supported `nsh.lock` format version (matching `nsh-cli` / `nsh-bundler`).
const LOCK_VERSION: u64 = 2;

/// Internal NSH crates that never count as user/third-party nshms.
const NSH_CRATES: &[&str] = &[
    "nsh-core",
    "nsh-base",
    "nsh-macros",
    "nsh-engine",
    "nsh-js",
    "nsh-dev",
    "nsh-testing",
    "nsh-package",
    "nsh-bundler",
    "nsh-i18n",
    "nsh-updater",
    "nsh-platform-harmonyos-core",
    "nsh-platform-harmonyos",
    "nsh-platform-android",
    "nsh-platform-apple",
    "nsh-platform-ios",
    "nsh-platform-linux",
    "nsh-platform-macos",
    "nsh-platform-tui",
    "nsh-platform-web",
    "nsh-platform-windows",
    "nsh-platform-gpui",
    "nsh-ios-app",
    "nsh-ios-host",
    "nsh-linux-app",
    "nsh-linux-host",
    "nsh-macos-app",
    "nsh-macos-host",
    "nsh-windows-app",
    "nsh-windows-host",
    "nsh-tui-host",
    "gpui",
];

/// Git reference kind for a transitive Cargo nshm dependency.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum GitRef {
    Default,
    Branch(String),
    Tag(String),
    Rev(String),
}

/// Where the runner's `Cargo.toml` takes a linked crate from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CrateSource {
    /// Local path (app crate, local nshm, or installed nshm under `~/.nsh/nshms`).
    Path(PathBuf),
    /// Registry crate (for a transitive Cargo nshm of an app crate).
    Registry {
        version: String,
        registry: Option<String>,
    },
    /// Git crate (for a transitive Cargo nshm of an app crate).
    Git { url: String, reference: GitRef },
}

/// One Rust crate linked into the HarmonyOS runner's `ui_sets`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LinkedCrate {
    /// Cargo `[package] name`.
    pub(crate) package: String,
    /// Where Cargo resolves the crate from.
    pub(crate) source: CrateSource,
    /// Extra Cargo features to enable (from `nsh.json` `"dependencies"`).
    pub(crate) features: Vec<String>,
    /// Whether Cargo's default features stay enabled (`false` for transitive
    /// Cargo nshms so Cargo keeps only the features the app's own crate turned on).
    pub(crate) default_features: bool,
    /// Canonical directory on disk when known (for deduplication).
    dir: Option<PathBuf>,
}

impl LinkedCrate {
    /// Constructs a local-path linked crate with default features enabled.
    pub(crate) fn path(package: String, dir: PathBuf) -> Self {
        Self {
            package,
            source: CrateSource::Path(dir.clone()),
            features: Vec::new(),
            default_features: true,
            dir: Some(dir),
        }
    }

    /// Formats the `[dependencies]` line for `alias` in the runner's `Cargo.toml`.
    pub(crate) fn dependency_line(&self, alias: &str) -> String {
        use crate::runner::toml_string;
        let mut fields = vec![format!("package = {}", toml_string(&self.package))];
        match &self.source {
            CrateSource::Path(dir) => {
                fields.push(format!("path = {}", toml_string(&dir.to_string_lossy())));
            }
            CrateSource::Registry { version, registry } => {
                fields.push(format!("version = {}", toml_string(version)));
                if let Some(index) = registry {
                    fields.push(format!("registry-index = {}", toml_string(index)));
                }
            }
            CrateSource::Git { url, reference } => {
                fields.push(format!("git = {}", toml_string(url)));
                match reference {
                    GitRef::Default => {}
                    GitRef::Branch(b) => fields.push(format!("branch = {}", toml_string(b))),
                    GitRef::Tag(t) => fields.push(format!("tag = {}", toml_string(t))),
                    GitRef::Rev(r) => fields.push(format!("rev = {}", toml_string(r))),
                }
            }
        }
        if !self.default_features {
            fields.push("default-features = false".to_owned());
        }
        if !self.features.is_empty() {
            let list = self
                .features
                .iter()
                .map(|f| toml_string(f))
                .collect::<Vec<_>>()
                .join(", ");
            fields.push(format!("features = [{list}]"));
        }
        format!("{alias} = {{ {} }}\n", fields.join(", "))
    }
}

/// Target `[patch]` table for an installed nshm.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum PatchTable {
    /// `[patch.crates-io]`
    CratesIo,
    /// `[patch."<git-url>"]`
    Git(String),
}

impl PatchTable {
    /// TOML section header string.
    pub(crate) fn header(&self) -> String {
        match self {
            PatchTable::CratesIo => "[patch.crates-io]".to_owned(),
            PatchTable::Git(url) => format!("[patch.{}]", crate::runner::toml_string(url)),
        }
    }
}

/// A `[patch]` entry pointing a registry or Git nshm to its locked local copy
/// in `~/.nsh/nshms`, matching `nsh-cli`'s workspace patches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PatchEntry {
    pub(crate) table: PatchTable,
    pub(crate) package: String,
    pub(crate) path: PathBuf,
}

/// Resolved Rust crates and `[patch]` entries for a TSX app.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct TsxCrates {
    pub(crate) crates: Vec<LinkedCrate>,
    pub(crate) patches: Vec<PatchEntry>,
}

#[derive(Debug, Deserialize)]
struct LockFile {
    #[serde(default)]
    packages: Vec<LockedPackage>,
}

#[derive(Debug, Deserialize)]
struct LockedPackage {
    name: String,
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    dir: Option<String>,
    #[serde(default)]
    module: Option<String>,
}

/// Resolves all Rust crates to link for a TSX app in `app`:
/// `"rust"` crates, `nsh.lock` Rust nshms (with `nsh.json` `"dependencies"`
/// features), and transitive Cargo nshms of the app's own `"rust"` crates
/// filtered for the OHOS target `triple`.
pub(crate) fn resolve_tsx_crates(app: &Path, triple: &str) -> anyhow::Result<TsxCrates> {
    let installed_root = installed_dir();
    resolve_tsx_crates_with_root(app, installed_root.as_deref(), Some(triple))
}

fn resolve_tsx_crates_with_root(
    app: &Path,
    installed_root: Option<&Path>,
    cargo_nshm_triple: Option<&str>,
) -> anyhow::Result<TsxCrates> {
    let text = fs::read_to_string(app.join("nsh.json")).context("reading nsh.json")?;
    let json: Value = serde_json::from_str(&text).context("nsh.json is not JSON")?;

    let own_dirs = crates_of(app, &json["rust"])?;
    let mut crates: Vec<LinkedCrate> = Vec::new();
    let mut seen_dirs: HashSet<PathBuf> = HashSet::new();
    let mut seen_pkgs: BTreeMap<String, (String, Option<PathBuf>)> = BTreeMap::new();

    for dir in &own_dirs {
        let package = read_package_name(dir)?;
        let norm = normalize_crate_name(&package);
        if let Some((prev_pkg, prev_dir)) = seen_pkgs.get(&norm)
            && prev_dir.as_ref() != Some(dir)
        {
            bail!(
                "nsh.json \"rust\": crate `{package}` ({}) collides with `{prev_pkg}` (same normalized crate name `{norm}`)",
                dir.display()
            );
        }
        seen_dirs.insert(dir.clone());
        seen_pkgs.insert(norm, (package.clone(), Some(dir.clone())));
        crates.push(LinkedCrate::path(package, dir.clone()));
    }

    let dep_features = read_dependency_features(&json)?;
    let mut patches = Vec::new();
    if let Some(lock) = read_lock(app)? {
        for pkg in lock.packages {
            // Only Rust nshms (those with `#[native_module]`) link into native hosts.
            // TSX-only nshms and platform nshms without a native module leave `module` unset.
            if pkg.module.is_none() {
                continue;
            }
            let dir = resolve_locked_dir(app, installed_root, &pkg)?;
            let package = read_package_name(&dir).unwrap_or_else(|_| pkg.name.clone());
            let features = dep_features.get(&pkg.name).cloned().unwrap_or_default();

            if let Some(table) = patch_table_of(pkg.source.as_deref()) {
                patches.push(PatchEntry {
                    table,
                    package: package.clone(),
                    path: dir.clone(),
                });
            }

            let norm = normalize_crate_name(&package);
            if seen_dirs.contains(&dir) {
                if let Some(existing) = crates.iter_mut().find(|c| c.dir.as_ref() == Some(&dir)) {
                    for feat in features {
                        if !existing.features.contains(&feat) {
                            existing.features.push(feat);
                        }
                    }
                }
                continue;
            }
            if let Some((prev_pkg, _)) = seen_pkgs.get(&norm) {
                bail!(
                    "{LOCK_FILE}: nshm `{package}` ({}) collides with `{prev_pkg}` (same normalized crate name `{norm}`)",
                    dir.display()
                );
            }

            seen_dirs.insert(dir.clone());
            seen_pkgs.insert(norm, (package.clone(), Some(dir.clone())));
            let mut linked = LinkedCrate::path(package, dir);
            linked.features = features;
            crates.push(linked);
        }
    }

    // Transitive Cargo nshms depended on directly by the app's own `"rust"` crates
    // (matching upstream PR #2874): linked after `"rust"` and `nsh.lock` entries.
    if let Some(triple) = cargo_nshm_triple
        && !own_dirs.is_empty()
    {
        for found in cargo_nshm(&own_dirs, triple)? {
            let norm = normalize_crate_name(&found.package);
            if found.dir.as_ref().is_some_and(|d| seen_dirs.contains(d)) {
                continue;
            }
            if let Some((prev_pkg, prev_dir)) = seen_pkgs.get(&norm) {
                if prev_pkg == &found.package || prev_dir == &found.dir {
                    continue;
                }
                bail!(
                    "transitive Cargo nshm `{}` collides with `{prev_pkg}` (same normalized crate name `{norm}`)",
                    found.package
                );
            }
            if let Some(d) = &found.dir {
                seen_dirs.insert(d.clone());
            }
            seen_pkgs.insert(norm, (found.package.clone(), found.dir.clone()));
            crates.push(found);
        }
    }

    Ok(TsxCrates { crates, patches })
}

/// Parses `nsh.json` `"rust"` (directory string or array of directories).
pub(crate) fn crates_of(app: &Path, rust: &Value) -> anyhow::Result<Vec<PathBuf>> {
    let dirs: Vec<&str> = match rust {
        Value::Null => Vec::new(),
        Value::String(dir) => vec![dir],
        Value::Array(dirs) => dirs
            .iter()
            .map(|d| {
                d.as_str()
                    .context("nsh.json \"rust\" entries must be crate directory paths")
            })
            .collect::<anyhow::Result<_>>()?,
        _ => bail!("nsh.json \"rust\" must be a crate directory or an array of them"),
    };
    dirs.into_iter()
        .map(|dir| {
            let path = app.join(dir);
            path.canonicalize()
                .with_context(|| format!("nsh.json \"rust\": no crate at {}", path.display()))
        })
        .collect()
}

/// Where `nsh` installs shared nshms: `<NSH_HOME>/nshms`, else `~/.nsh/nshms`.
pub(crate) fn installed_dir() -> Option<PathBuf> {
    let home = if let Some(dir) = env::var_os("NSH_HOME").filter(|d| !d.is_empty()) {
        Some(PathBuf::from(dir))
    } else if cfg!(windows) {
        env::var_os("USERPROFILE")
            .filter(|h| !h.is_empty())
            .map(|h| PathBuf::from(h).join(".nsh"))
    } else {
        env::var_os("HOME")
            .filter(|h| !h.is_empty())
            .map(|h| PathBuf::from(h).join(".nsh"))
    };
    home.map(|h| h.join("nshms"))
}

fn read_lock(app: &Path) -> anyhow::Result<Option<LockFile>> {
    let path = app.join(LOCK_FILE);
    let Ok(text) = fs::read_to_string(&path) else {
        return Ok(None);
    };
    let value: Value =
        serde_json::from_str(&text).with_context(|| format!("{LOCK_FILE} is not JSON"))?;
    match value.get("version").and_then(Value::as_u64) {
        Some(LOCK_VERSION) => {
            let lock: LockFile = serde_json::from_value(value)
                .with_context(|| format!("{LOCK_FILE} is not valid"))?;
            Ok(Some(lock))
        }
        Some(v) if v > LOCK_VERSION => {
            bail!("{LOCK_FILE} is of a newer nsh (format {v}): update nsh-platform-harmonyos")
        }
        _ => Ok(None),
    }
}

fn read_dependency_features(config: &Value) -> anyhow::Result<BTreeMap<String, Vec<String>>> {
    let mut out = BTreeMap::new();
    let Some(deps) = config.get("dependencies").and_then(Value::as_object) else {
        return Ok(out);
    };
    for (name, spec) in deps {
        let Some(features) = spec.get("features") else {
            continue;
        };
        let list = features
            .as_array()
            .and_then(|arr| {
                arr.iter()
                    .map(|v| v.as_str().map(str::to_owned))
                    .collect::<Option<Vec<_>>>()
            })
            .with_context(|| {
                format!("nsh.json: dependencies.{name}.features must be an array of strings")
            })?;
        for feat in &list {
            if feat.is_empty()
                || !feat.bytes().all(|b| {
                    b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'/' | b'.' | b'+')
                })
            {
                bail!(
                    "nsh.json: dependencies.{name}.features contains invalid feature name {feat:?}"
                );
            }
        }
        if !list.is_empty() {
            out.insert(name.clone(), list);
        }
    }
    Ok(out)
}

fn resolve_locked_dir(
    app: &Path,
    installed_root: Option<&Path>,
    pkg: &LockedPackage,
) -> anyhow::Result<PathBuf> {
    let (raw, confine_to_root) = match (&pkg.path, &pkg.dir) {
        (Some(path), _) => (app.join(path), None),
        (None, Some(dir)) => {
            let root = installed_root
                .context("no home directory to locate installed nshms: set NSH_HOME")?;
            let rel = Path::new(dir);
            if rel.is_absolute()
                || rel.components().any(|c| {
                    matches!(
                        c,
                        std::path::Component::ParentDir
                            | std::path::Component::RootDir
                            | std::path::Component::Prefix(_)
                    )
                })
            {
                bail!(
                    "{LOCK_FILE}: nshm `{}` has invalid `dir` {dir:?} (must stay inside installed nshms directory)",
                    pkg.name
                );
            }
            (root.join(rel), Some(root))
        }
        (None, None) => bail!("{LOCK_FILE}: {} has neither \"path\" nor \"dir\"", pkg.name),
    };
    let canonical = raw.canonicalize().with_context(|| {
        format!(
            "{LOCK_FILE}: nshm `{}` is not installed at {} (run `nsh build` first)",
            pkg.name,
            raw.display()
        )
    })?;
    if let Some(root) = confine_to_root {
        let canonical_root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
        if !canonical.starts_with(&canonical_root) {
            bail!(
                "{LOCK_FILE}: nshm `{}` resolves outside {} ({})",
                pkg.name,
                canonical_root.display(),
                canonical.display()
            );
        }
    }
    if !canonical.join("Cargo.toml").is_file() {
        bail!(
            "{LOCK_FILE}: nshm `{}` at {} has no Cargo.toml",
            pkg.name,
            canonical.display()
        );
    }
    Ok(canonical)
}

fn patch_table_of(source: Option<&str>) -> Option<PatchTable> {
    let source = source?;
    if source == "crates.io" {
        return Some(PatchTable::CratesIo);
    }
    let rest = source.strip_prefix("git+")?;
    let without_hash = rest.split_once('#').map_or(rest, |(u, _)| u);
    let url = without_hash
        .split_once('?')
        .map_or(without_hash, |(u, _)| u);
    (!url.is_empty()).then(|| PatchTable::Git(url.to_owned()))
}

fn read_package_name(dir: &Path) -> anyhow::Result<String> {
    let manifest_path = dir.join("Cargo.toml");
    let manifest = fs::read_to_string(&manifest_path)
        .with_context(|| format!("reading {}", manifest_path.display()))?;
    crate::runner::package_name(&manifest)
        .map(str::to_owned)
        .with_context(|| format!("{} has no [package] name", manifest_path.display()))
}

fn normalize_crate_name(name: &str) -> String {
    name.replace('-', "_")
}

/// Reads the `#[native_module("…")]` specifier at the crate root in `dir`, if any.
fn specifier_of(dir: &Path) -> Option<String> {
    let manifest = fs::read_to_string(dir.join("Cargo.toml")).ok()?;
    let mut in_lib = false;
    let mut lib_path = None;
    for line in manifest.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_lib = trimmed == "[lib]";
        } else if in_lib
            && let Some(rest) = trimmed.strip_prefix("path")
            && let Some(rest) = rest.trim_start().strip_prefix('=')
        {
            let rest = rest.trim_start();
            if let Some(quote) = rest.chars().next().filter(|c| *c == '"' || *c == '\'') {
                let rest = &rest[1..];
                if let Some(end) = rest.find(quote) {
                    lib_path = Some(rest[..end].to_owned());
                    break;
                }
            }
        }
    }
    let lib = lib_path.unwrap_or_else(|| "src/lib.rs".to_owned());
    let code = fs::read_to_string(dir.join(lib)).ok()?;
    code.lines().find_map(|line| {
        const ATTR: &str = "native_module";
        let attr = line.trim().strip_prefix("#[")?;
        let rest = attr[attr.find(ATTR)? + ATTR.len()..].trim_start();
        let rest = rest.strip_prefix('(')?.trim_start().strip_prefix('"')?;
        Some(rest[..rest.find('"')?].to_owned())
    })
}

/// Discovers Rust nshms reachable as normal Cargo dependencies of `own_crates`
/// for the OHOS target `triple`.
fn cargo_nshm(own_crates: &[PathBuf], triple: &str) -> anyhow::Result<Vec<LinkedCrate>> {
    let cargo = env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let mut found: Vec<(String, LinkedCrate)> = Vec::new();
    for dir in own_crates {
        let manifest = dir.join("Cargo.toml");
        let output = Command::new(&cargo)
            .args([
                "metadata",
                "--format-version",
                "1",
                "--filter-platform",
                triple,
                "--manifest-path",
            ])
            .arg(&manifest)
            .output()
            .context("running cargo metadata to discover Cargo nshms")?;
        if !output.status.success() {
            bail!(
                "cargo metadata failed for {}:\n{}",
                manifest.display(),
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        let metadata: Value =
            serde_json::from_slice(&output.stdout).context("parsing cargo metadata JSON")?;
        for (id, krate) in nshm_in(&metadata, &manifest)? {
            if !found.iter().any(|(seen, _)| *seen == id) {
                found.push((id, krate));
            }
        }
    }
    found.sort_by(|(a, x), (b, y)| (&x.package, a).cmp(&(&y.package, b)));
    Ok(found.into_iter().map(|(_, k)| k).collect())
}

fn nshm_in(metadata: &Value, manifest: &Path) -> anyhow::Result<Vec<(String, LinkedCrate)>> {
    let packages = metadata["packages"]
        .as_array()
        .context("cargo metadata packages")?;
    let manifest = manifest
        .canonicalize()
        .unwrap_or_else(|_| manifest.to_path_buf());
    let root = packages
        .iter()
        .find(|p| {
            p["manifest_path"]
                .as_str()
                .and_then(|m| Path::new(m).canonicalize().ok())
                .is_some_and(|m| m == manifest)
        })
        .with_context(|| format!("cargo metadata has no package at {}", manifest.display()))?;
    let root_id = root["id"].as_str().context("cargo metadata package id")?;
    let nodes = metadata["resolve"]["nodes"]
        .as_array()
        .context("cargo metadata dependency graph")?;

    let mut reached = vec![root_id];
    let mut next = 0;
    while let Some(id) = reached.get(next).copied() {
        next += 1;
        let deps = nodes
            .iter()
            .find(|n| n["id"] == id)
            .and_then(|n| n["deps"].as_array())
            .into_iter()
            .flatten();
        for dep in deps {
            let normal = dep["dep_kinds"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|k| k["kind"].is_null());
            if let Some(pkg) = dep["pkg"].as_str()
                && normal
                && !reached.contains(&pkg)
            {
                reached.push(pkg);
            }
        }
    }

    let mut found = Vec::new();
    for id in &reached[1..] {
        let Some(p) = packages.iter().find(|p| p["id"] == *id) else {
            continue;
        };
        let Some(name) = p["name"].as_str() else {
            continue;
        };
        let on_core = p["dependencies"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|d| d["name"] == "nsh-core" && d["kind"].is_null());
        if NSH_CRATES.contains(&name) || !on_core {
            continue;
        }
        let Some(dir) = p["manifest_path"]
            .as_str()
            .and_then(|m| Path::new(m).parent())
        else {
            continue;
        };
        let dir = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
        if specifier_of(&dir).is_none() {
            continue;
        }
        let source = cargo_source(p, &dir)?;
        found.push((
            (*id).to_owned(),
            LinkedCrate {
                package: name.to_owned(),
                source,
                features: Vec::new(),
                default_features: false,
                dir: Some(dir),
            },
        ));
    }
    Ok(found)
}

fn cargo_source(pkg: &Value, dir: &Path) -> anyhow::Result<CrateSource> {
    const CRATES_IO: &[&str] = &[
        "registry+https://github.com/rust-lang/crates.io-index",
        "sparse+https://index.crates.io/",
    ];
    let Some(source) = pkg["source"].as_str() else {
        return Ok(CrateSource::Path(dir.to_path_buf()));
    };
    let version = pkg["version"]
        .as_str()
        .context("cargo metadata package has no version")?;
    if CRATES_IO.contains(&source) {
        return Ok(CrateSource::Registry {
            version: format!("={version}"),
            registry: None,
        });
    }
    if let Some(index) = source.strip_prefix("registry+") {
        return Ok(CrateSource::Registry {
            version: format!("={version}"),
            registry: Some(index.to_owned()),
        });
    }
    if source.starts_with("sparse+") {
        return Ok(CrateSource::Registry {
            version: format!("={version}"),
            registry: Some(source.to_owned()),
        });
    }
    if let Some(rest) = source.strip_prefix("git+") {
        let before_hash = rest.split_once('#').map_or(rest, |(a, _)| a);
        let (url, query) = before_hash
            .split_once('?')
            .map_or((before_hash, None), |(a, b)| (a, Some(b)));
        let reference = match query.and_then(|q| q.split_once('=')) {
            Some(("branch", b)) => GitRef::Branch(b.to_owned()),
            Some(("tag", t)) => GitRef::Tag(t.to_owned()),
            Some(("rev", r)) => GitRef::Rev(r.to_owned()),
            // Cargo metadata always appends `#<resolved-commit>` even for default-branch
            // git deps; emitting `rev = "<commit>"` creates a distinct Cargo SourceId.
            _ => GitRef::Default,
        };
        return Ok(CrateSource::Git {
            url: url.to_owned(),
            reference,
        });
    }
    Ok(CrateSource::Path(dir.to_path_buf()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::join_all;

    fn write_crate(dir: &Path, name: &str, module: Option<&str>) {
        fs::create_dir_all(dir.join("src")).unwrap();
        fs::write(
            dir.join("Cargo.toml"),
            format!("[package]\nname = {name:?}\nversion = \"0.1.0\"\nedition = \"2024\"\n"),
        )
        .unwrap();
        let lib = match module {
            Some(spec) => format!("#[native_module({spec:?})]\nfn native() {{}}\n"),
            None => "pub fn helper() {}\n".to_owned(),
        };
        fs::write(dir.join("src/lib.rs"), lib).unwrap();
    }

    #[test]
    fn resolves_rust_crates_and_locked_rust_nshms_with_features() {
        let base = env::temp_dir().join(format!("nsh-hm-nshm-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let app = base.join("app");
        let nshms = base.join("nshms");
        fs::create_dir_all(&app).unwrap();
        fs::create_dir_all(&nshms).unwrap();

        let own = app.join("native");
        write_crate(&own, "app-native", Some("app/native"));

        let extra = join_all(&nshms, ["nsh-extra", "0.6.0"]);
        write_crate(&extra, "nsh-extra", Some("nsh-extra"));

        let git_mod = join_all(&nshms, ["github.com", "acme", "kit", "1.0.0"]);
        write_crate(&git_mod, "acme-kit", Some("acme-kit"));

        let local_mod = base.join("local-mod");
        write_crate(&local_mod, "local-mod", Some("local-mod"));

        let tsx_only = join_all(&nshms, ["acme-ui", "1.0.0"]);
        fs::create_dir_all(&tsx_only).unwrap();

        let platform_only = join_all(&nshms, ["nsh-platform-harmonyos", "0.1.0"]);
        write_crate(&platform_only, "nsh-platform-harmonyos", None);

        fs::write(
            app.join("nsh.json"),
            r#"{
              "name": "Demo",
              "rust": "native",
              "dependencies": {
                "nsh-extra": { "version": "^0.6", "features": ["camera", "push"] },
                "acme-kit": { "github": "acme/kit", "version": "^1.0" },
                "local-mod": { "path": "../local-mod", "features": ["scan"] },
                "acme-ui": { "github": "acme/ui", "version": "^1.0" },
                "nsh-platform-harmonyos": { "github": "huacnlee/nsh-platform-harmonyos" }
              }
            }"#,
        )
        .unwrap();
        fs::write(
            app.join("nsh.lock"),
            r#"{
              "version": 2,
              "packages": [
                {
                  "name": "acme-kit",
                  "version": "1.0.0",
                  "source": "git+https://github.com/acme/kit?tag=v1.0.0",
                  "dir": "github.com/acme/kit/1.0.0",
                  "module": "acme-kit"
                },
                {
                  "name": "acme-ui",
                  "version": "1.0.0",
                  "source": "git+https://github.com/acme/ui",
                  "dir": "acme-ui/1.0.0",
                  "exports": { "acme-ui": "src/index.tsx" }
                },
                {
                  "name": "local-mod",
                  "version": "0.1.0",
                  "path": "../local-mod",
                  "module": "local-mod"
                },
                {
                  "name": "nsh-extra",
                  "version": "0.6.0",
                  "source": "crates.io",
                  "dir": "nsh-extra/0.6.0",
                  "module": "nsh-extra"
                },
                {
                  "name": "nsh-platform-harmonyos",
                  "version": "0.1.0",
                  "source": "git+https://github.com/huacnlee/nsh-platform-harmonyos",
                  "dir": "nsh-platform-harmonyos/0.1.0",
                  "platform": "harmonyos"
                }
              ]
            }"#,
        )
        .unwrap();

        let resolved = resolve_tsx_crates_with_root(&app, Some(&nshms), None).unwrap();
        let names: Vec<&str> = resolved.crates.iter().map(|c| c.package.as_str()).collect();
        assert_eq!(names, ["app-native", "acme-kit", "local-mod", "nsh-extra"]);
        assert_eq!(resolved.crates[0].features, Vec::<String>::new());
        assert_eq!(resolved.crates[1].features, Vec::<String>::new());
        assert_eq!(resolved.crates[2].features, vec!["scan"]);
        assert_eq!(resolved.crates[3].features, vec!["camera", "push"]);
        assert_eq!(resolved.patches.len(), 2);
        assert_eq!(
            resolved.patches[0],
            PatchEntry {
                table: PatchTable::Git("https://github.com/acme/kit".into()),
                package: "acme-kit".into(),
                path: git_mod.canonicalize().unwrap(),
            }
        );
        assert_eq!(
            resolved.patches[1],
            PatchEntry {
                table: PatchTable::CratesIo,
                package: "nsh-extra".into(),
                path: extra.canonicalize().unwrap(),
            }
        );

        let dep_line = resolved.crates[3].dependency_line("app_3");
        assert!(dep_line.contains("package = \"nsh-extra\""), "{dep_line}");
        assert!(
            dep_line.contains("features = [\"camera\", \"push\"]"),
            "{dep_line}"
        );

        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn deduplicates_crates_listed_in_both_rust_and_lock() {
        let base = env::temp_dir().join(format!("nsh-hm-nshm-dedup-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let app = base.join("app");
        let shared = app.join("shared");
        write_crate(&shared, "app-shared", Some("app/shared"));
        fs::write(
            app.join("nsh.json"),
            r#"{
              "rust": "shared",
              "dependencies": {
                "app-shared": { "path": "shared", "features": ["extra"] }
              }
            }"#,
        )
        .unwrap();
        fs::write(
            app.join("nsh.lock"),
            r#"{
              "version": 2,
              "packages": [
                { "name": "app-shared", "version": "0.1.0", "path": "shared", "module": "app/shared" }
              ]
            }"#,
        )
        .unwrap();
        let resolved = resolve_tsx_crates_with_root(&app, None, None).unwrap();
        assert_eq!(resolved.crates.len(), 1);
        assert_eq!(resolved.crates[0].package, "app-shared");
        assert_eq!(resolved.crates[0].features, vec!["extra"]);
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn rejects_newer_lock_version_and_finds_transitive_cargo_nshms() {
        let base = env::temp_dir().join(format!("nsh-hm-nshm-trans-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let app = base.join("app");
        let root_crate = app.join("native");
        let trans_nshm = base.join("trans-nshm");
        let plain_dep = base.join("plain-dep");
        write_crate(&root_crate, "app-native", Some("app/native"));
        write_crate(&trans_nshm, "trans-nshm", Some("trans-nshm"));
        write_crate(&plain_dep, "plain-dep", None);

        fs::write(app.join("nsh.json"), "{}").unwrap();
        fs::write(app.join("nsh.lock"), r#"{ "version": 99, "packages": [] }"#).unwrap();
        let err = resolve_tsx_crates_with_root(&app, None, None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("format 99"), "{err}");

        let root_manifest = root_crate.join("Cargo.toml").canonicalize().unwrap();
        let trans_manifest = trans_nshm.join("Cargo.toml").canonicalize().unwrap();
        let plain_manifest = plain_dep.join("Cargo.toml").canonicalize().unwrap();
        let metadata = serde_json::json!({
            "packages": [
                {
                    "id": "path+root#0.1.0",
                    "name": "app-native",
                    "version": "0.1.0",
                    "source": null,
                    "manifest_path": root_manifest.to_string_lossy(),
                    "dependencies": [
                        { "name": "plain-dep", "kind": null },
                        { "name": "nsh-core", "kind": null }
                    ]
                },
                {
                    "id": "path+plain#0.1.0",
                    "name": "plain-dep",
                    "version": "0.1.0",
                    "source": null,
                    "manifest_path": plain_manifest.to_string_lossy(),
                    "dependencies": [
                        { "name": "trans-nshm", "kind": null }
                    ]
                },
                {
                    "id": "registry+trans#0.2.0",
                    "name": "trans-nshm",
                    "version": "0.2.0",
                    "source": "registry+https://github.com/rust-lang/crates.io-index",
                    "manifest_path": trans_manifest.to_string_lossy(),
                    "dependencies": [
                        { "name": "nsh-core", "kind": null }
                    ]
                }
            ],
            "resolve": {
                "nodes": [
                    {
                        "id": "path+root#0.1.0",
                        "deps": [
                            { "pkg": "path+plain#0.1.0", "dep_kinds": [{ "kind": null }] }
                        ]
                    },
                    {
                        "id": "path+plain#0.1.0",
                        "deps": [
                            { "pkg": "registry+trans#0.2.0", "dep_kinds": [{ "kind": null }] }
                        ]
                    },
                    {
                        "id": "registry+trans#0.2.0",
                        "deps": []
                    }
                ]
            }
        });
        let found = nshm_in(&metadata, &root_manifest).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].1.package, "trans-nshm");
        assert!(!found[0].1.default_features);
        assert_eq!(
            found[0].1.source,
            CrateSource::Registry {
                version: "=0.2.0".into(),
                registry: None,
            }
        );
        let line = found[0].1.dependency_line("app_1");
        assert!(
            line.contains("version = \"=0.2.0\", default-features = false"),
            "{line}"
        );

        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn rejects_path_traversal_strips_git_fragment_and_uses_default_git_ref() {
        let base = env::temp_dir().join(format!("nsh-hm-nshm-sec-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let app = base.join("app");
        let nshms = base.join("nshms");
        fs::create_dir_all(&app).unwrap();
        fs::create_dir_all(&nshms).unwrap();

        assert_eq!(
            patch_table_of(Some(
                "git+https://github.com/acme/kit?branch=main#0123456789abcdef"
            )),
            Some(PatchTable::Git("https://github.com/acme/kit".into()))
        );
        let git_pkg = serde_json::json!({
            "version": "0.1.0",
            "source": "git+https://github.com/acme/kit#0123456789abcdef"
        });
        assert_eq!(
            cargo_source(&git_pkg, Path::new("/tmp/kit")).unwrap(),
            CrateSource::Git {
                url: "https://github.com/acme/kit".into(),
                reference: GitRef::Default,
            }
        );

        // Lock dir escaping ~/.nsh/nshms must be rejected.
        fs::write(app.join("nsh.json"), "{}").unwrap();
        fs::write(
            app.join("nsh.lock"),
            r#"{
              "version": 2,
              "packages": [
                {
                  "name": "evil",
                  "version": "0.1.0",
                  "source": "crates.io",
                  "dir": "../escape",
                  "module": "evil"
                }
              ]
            }"#,
        )
        .unwrap();
        let err = resolve_tsx_crates_with_root(&app, Some(&nshms), None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("installed nshms directory"), "{err}");

        // Normalized crate-name collisions (foo-bar vs foo_bar) must fail loudly.
        let one = app.join("one");
        let two = app.join("two");
        write_crate(&one, "foo-bar", Some("foo/one"));
        write_crate(&two, "foo_bar", Some("foo/two"));
        fs::remove_file(app.join("nsh.lock")).unwrap();
        fs::write(app.join("nsh.json"), r#"{ "rust": ["one", "two"] }"#).unwrap();
        let err = resolve_tsx_crates_with_root(&app, Some(&nshms), None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("foo-bar") && err.contains("foo_bar"), "{err}");

        fs::remove_dir_all(&base).unwrap();
    }
}
