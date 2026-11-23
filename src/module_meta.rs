//! HarmonyOS package metadata declared by the app (`nsh.json` `"harmonyos"`)
//! and by crates / nshms in the runner's resolved Cargo dependency graph
//! (`[package.metadata.nsh.harmonyos]` and
//! `[package.metadata.nsh.features.<feature>.harmonyos]`):
//!
//! ```toml
//! [package.metadata.nsh.features.camera.harmonyos]
//! querySchemes = ["weixin"]
//!
//! [[package.metadata.nsh.features.camera.harmonyos.permissions]]
//! name = "ohos.permission.CAMERA" # or bare "CAMERA"
//! reason = "Take photos with the camera."
//! when = "inuse" # optional: "inuse" (default) or "always"
//! abilities = ["EntryAbility"] # optional
//!
//! [[package.metadata.nsh.features.push.harmonyos.metadata]]
//! name = "client_id"
//! value = "123456"
//!
//! [[package.metadata.nsh.features.push.harmonyos.extensionAbilities]]
//! name = "PushExtAbility"
//! srcEntry = "./ets/push/PushExtAbility.ets"
//! type = "remoteNotification"
//!
//! [package.metadata.nsh.features.lottie.harmonyos.dependencies]
//! "@ohos/lottie" = "^2.0.0"
//! "@acme/kit" = "file:ohos/kit.har"
//! ```
//!
//! Precedence matches upstream `nsh`'s `module_meta.rs`:
//! - Only sections for Cargo features actually enabled for the OHOS target are merged.
//! - Identical entries across crates are deduplicated.
//! - Conflicting values set by two crates are recorded as clashes; setting the
//!   entry in the app's `nsh.json` `"harmonyos"` wins and settles the clash,
//!   otherwise packaging fails naming both crates.
//! - Reserved keys owned by NSH (`EntryAbility`, `@nsh/harmonyos`,
//!   `libnsh_platform_harmonyos.so`, `app_name`) are rejected.

use std::{
    collections::BTreeMap,
    env,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context as _, bail};
use serde_json::{Map, Value};

/// HarmonyOS permission entry for `module.json5` `requestPermissions`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Permission {
    /// Full permission name (e.g. `ohos.permission.CAMERA`).
    pub(crate) name: String,
    /// Optional reason: either a `$string:<name>` resource reference or plain
    /// text (which `project::write` turns into an entry in `string.json`).
    pub(crate) reason: Option<String>,
    /// Optional `usedScene` (`abilities` and `when`).
    pub(crate) used_scene: Option<UsedScene>,
}

/// `usedScene` block of a HarmonyOS `user_grant` permission.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UsedScene {
    pub(crate) abilities: Vec<String>,
    pub(crate) when: String,
}

/// One entry of `module.json5` `module.metadata`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MetadataItem {
    pub(crate) name: String,
    pub(crate) value: Option<String>,
    pub(crate) resource: Option<String>,
}

/// One ohpm dependency for `entry/oh-package.json5`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum OhpmDep {
    /// Registry version requirement (e.g. `"^2.0.0"`).
    Version(String),
    /// Local `.har` / `.tgz` archive or HAR module directory on disk, copied
    /// into the generated DevEco project under `entry/nshm-deps/`.
    LocalPath(PathBuf),
}

/// Merged HarmonyOS packaging declarations (from `nsh.json` and/or crates).
#[derive(Debug, Default, Clone, PartialEq)]
pub(crate) struct HarmonyMeta {
    /// `module.json5` `requestPermissions` additions/overrides.
    pub(crate) permissions: Vec<Permission>,
    /// `module.json5` `querySchemes`.
    pub(crate) query_schemes: Vec<String>,
    /// `module.json5` `metadata`.
    pub(crate) metadata: Vec<MetadataItem>,
    /// Extra `module.json5` `abilities` objects (besides `EntryAbility`).
    pub(crate) abilities: Vec<Map<String, Value>>,
    /// `module.json5` `extensionAbilities` objects.
    pub(crate) extension_abilities: Vec<Map<String, Value>>,
    /// Extra `entry/oh-package.json5` `dependencies`.
    pub(crate) dependencies: BTreeMap<String, OhpmDep>,
    /// Extra `entry/src/main/resources/base/element/string.json` entries.
    pub(crate) strings: BTreeMap<String, String>,
    /// Ability / extensionAbility `.ets` source files to stage into
    /// `entry/src/main/<rel_path>` (keyed by normalized `ets/...` path).
    pub(crate) ability_sources: BTreeMap<String, PathBuf>,
}

impl HarmonyMeta {
    /// Reads the app's own `"harmonyos"` section from `nsh.json` in `app_dir`.
    pub(crate) fn from_app_json(app_dir: &Path, json: &Value) -> anyhow::Result<Self> {
        let Some(section) = json.get("harmonyos") else {
            return Ok(Self::default());
        };
        let mut collector = ModuleMeta::default();
        let mut warnings = Vec::new();
        collector.merge_harmonyos(
            "nsh.json",
            Some(app_dir),
            "nsh.json",
            section,
            true,
            &mut warnings,
        )?;
        for warning in warnings {
            eprintln!("warning: {warning}");
        }
        if let Some(clash) = collector.clashes.first() {
            bail!("{clash}");
        }
        Ok(collector.meta)
    }
}

/// A conflicting entry declared differently by two crates.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Clash {
    kind: &'static str,
    key: String,
    first: String,
    second: String,
}

impl std::fmt::Display for Clash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} and {} set `{}` ({}) to different values: set it in the app's nsh.json `{}` to choose",
            self.first, self.second, self.key, self.kind, self.kind
        )
    }
}

/// What the resolved crates of a HarmonyOS runner declare under
/// `[package.metadata.nsh]`.
#[derive(Debug, Default, Clone, PartialEq)]
pub(crate) struct ModuleMeta {
    pub(crate) meta: HarmonyMeta,
    owners: BTreeMap<String, String>,
    clashes: Vec<Clash>,
}

impl ModuleMeta {
    /// Reads `[package.metadata.nsh]` from `cargo metadata` for the runner
    /// manifest at `manifest`, filtered to the OHOS target `triple`.
    pub(crate) fn of_manifest(manifest: &Path, triple: &str) -> anyhow::Result<Self> {
        let (meta, warnings) = Self::read_manifest(manifest, triple)?;
        for warning in warnings {
            eprintln!("warning: {warning}");
        }
        Ok(meta)
    }

    fn read_manifest(manifest: &Path, triple: &str) -> anyhow::Result<(Self, Vec<String>)> {
        let cargo = env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
        let out = Command::new(cargo)
            .args([
                "metadata",
                "--format-version",
                "1",
                "--filter-platform",
                triple,
                "--manifest-path",
            ])
            .arg(manifest)
            .output()
            .context("running cargo metadata for HarmonyOS crate metadata")?;
        if !out.status.success() {
            bail!(
                "cargo metadata failed for {}:\n{}",
                manifest.display(),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        let json: Value =
            serde_json::from_slice(&out.stdout).context("cargo metadata printed no JSON")?;
        Self::from_metadata(&json)
    }

    /// Parses `cargo metadata --format-version 1` JSON and merges every
    /// resolved package's `[package.metadata.nsh.harmonyos]` and active
    /// `[package.metadata.nsh.features.<feat>.harmonyos]` tables.
    pub(crate) fn from_metadata(json: &Value) -> anyhow::Result<(Self, Vec<String>)> {
        let packages: BTreeMap<&str, &Value> = json["packages"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|p| Some((p["id"].as_str()?, p)))
            .collect();
        let mut collected = ModuleMeta::default();
        let mut warnings = Vec::new();
        let mut nodes: Vec<&Value> = json["resolve"]["nodes"]
            .as_array()
            .into_iter()
            .flatten()
            .collect();
        nodes.sort_by_key(|n| n["id"].as_str().unwrap_or_default());
        for node in nodes {
            let Some(package) = node["id"].as_str().and_then(|id| packages.get(id)) else {
                continue;
            };
            let Some(nsh) = package["metadata"]["nsh"].as_object() else {
                continue;
            };
            let name = package["name"].as_str().unwrap_or("?");
            let dir = package["manifest_path"]
                .as_str()
                .and_then(|manifest| Path::new(manifest).parent());
            if let Some(hm) = nsh.get("harmonyos") {
                collected.merge_harmonyos(
                    name,
                    dir,
                    "[package.metadata.nsh]",
                    hm,
                    false,
                    &mut warnings,
                )?;
            }
            let features = nsh.get("features").and_then(Value::as_object);
            let enabled = node["features"].as_array().into_iter().flatten();
            for feature in enabled.filter_map(Value::as_str) {
                if let Some(hm) = features
                    .and_then(|f| f.get(feature))
                    .and_then(|f| f.get("harmonyos"))
                {
                    let at = format!("[package.metadata.nsh.features.{feature}]");
                    collected.merge_harmonyos(name, dir, &at, hm, false, &mut warnings)?;
                }
            }
        }
        Ok((collected, warnings))
    }

    /// Merges crate declarations under the app's own `nsh.json` `"harmonyos"`:
    /// app entries always win and settle clashes between crates; any remaining
    /// clash stops packaging with an error naming both crates.
    pub(crate) fn apply(&self, app: &mut HarmonyMeta) -> anyhow::Result<()> {
        for clash in &self.clashes {
            let settled = match clash.kind {
                "harmonyos.permissions" => app
                    .permissions
                    .iter()
                    .any(|p| p.name == clash.key && (p.reason.is_some() || p.used_scene.is_some())),
                "harmonyos.metadata" => app.metadata.iter().any(|m| m.name == clash.key),
                "harmonyos.abilities" => app
                    .abilities
                    .iter()
                    .any(|a| a.get("name").and_then(Value::as_str) == Some(clash.key.as_str())),
                "harmonyos.extensionAbilities" => app
                    .extension_abilities
                    .iter()
                    .any(|a| a.get("name").and_then(Value::as_str) == Some(clash.key.as_str())),
                "harmonyos.dependencies" => app.dependencies.contains_key(&clash.key),
                "harmonyos.strings" => app.strings.contains_key(&clash.key),
                _ => false,
            };
            if !settled {
                bail!("{clash}");
            }
        }

        for perm in &self.meta.permissions {
            match app.permissions.iter_mut().find(|p| p.name == perm.name) {
                None => app.permissions.push(perm.clone()),
                Some(existing) => {
                    // If the app only listed the bare permission name, inherit
                    // the crate's reason / usedScene; otherwise the app wins.
                    if existing.reason.is_none() && existing.used_scene.is_none() {
                        existing.reason = perm.reason.clone();
                        existing.used_scene = perm.used_scene.clone();
                    }
                }
            }
        }
        for scheme in &self.meta.query_schemes {
            if !app.query_schemes.contains(scheme) {
                app.query_schemes.push(scheme.clone());
            }
        }
        for item in &self.meta.metadata {
            if !app.metadata.iter().any(|m| m.name == item.name) {
                app.metadata.push(item.clone());
            }
        }
        for ability in &self.meta.abilities {
            let name = ability.get("name").and_then(Value::as_str);
            if !app
                .abilities
                .iter()
                .any(|a| a.get("name").and_then(Value::as_str) == name)
            {
                app.abilities.push(ability.clone());
            }
        }
        for ext in &self.meta.extension_abilities {
            let name = ext.get("name").and_then(Value::as_str);
            if !app
                .extension_abilities
                .iter()
                .any(|a| a.get("name").and_then(Value::as_str) == name)
            {
                app.extension_abilities.push(ext.clone());
            }
        }
        for (pkg, dep) in &self.meta.dependencies {
            app.dependencies
                .entry(pkg.clone())
                .or_insert_with(|| dep.clone());
        }
        for (key, value) in &self.meta.strings {
            app.strings
                .entry(key.clone())
                .or_insert_with(|| value.clone());
        }
        for (dest, src) in &self.meta.ability_sources {
            app.ability_sources
                .entry(dest.clone())
                .or_insert_with(|| src.clone());
        }
        if app.query_schemes.len() > 50 {
            bail!(
                "harmonyos.querySchemes has {} entries (HarmonyOS allows at most 50)",
                app.query_schemes.len()
            );
        }
        Ok(())
    }

    fn merge_harmonyos(
        &mut self,
        owner: &str,
        dir: Option<&Path>,
        at: &str,
        table: &Value,
        is_app: bool,
        warnings: &mut Vec<String>,
    ) -> anyhow::Result<()> {
        let table = table
            .as_object()
            .with_context(|| format!("{owner}: {at} `harmonyos` must be a table"))?;
        for (key, value) in table {
            match key.as_str() {
                "permissions" => self.merge_permissions(owner, at, value, warnings)?,
                "querySchemes" => self.merge_query_schemes(owner, at, value)?,
                "metadata" => self.merge_metadata(owner, at, value)?,
                "abilities" => self.merge_abilities(owner, dir, at, value, false, is_app)?,
                "extensionAbilities" => self.merge_abilities(owner, dir, at, value, true, is_app)?,
                "dependencies" => self.merge_dependencies(owner, dir, at, value, is_app)?,
                "strings" => self.merge_strings(owner, at, value, is_app)?,
                _ => warnings.push(format!(
                    "{owner}: {at} `harmonyos.{key}` is not something this nsh-platform-harmonyos reads: left out"
                )),
            }
        }
        Ok(())
    }

    fn merge_permissions(
        &mut self,
        owner: &str,
        at: &str,
        value: &Value,
        warnings: &mut Vec<String>,
    ) -> anyhow::Result<()> {
        let list = value
            .as_array()
            .with_context(|| format!("{owner}: {at} `harmonyos.permissions` must be an array"))?;
        for item in list {
            let perm = parse_permission(owner, at, item, warnings)?;
            let owner_key = format!("harmonyos.permissions.{}", perm.name);
            match self
                .meta
                .permissions
                .iter_mut()
                .find(|p| p.name == perm.name)
            {
                None => {
                    self.owners.insert(owner_key, owner.to_owned());
                    self.meta.permissions.push(perm);
                }
                Some(existing) if *existing == perm => {}
                Some(existing) if existing.reason.is_none() && existing.used_scene.is_none() => {
                    *existing = perm;
                    self.owners.insert(owner_key, owner.to_owned());
                }
                Some(_) if perm.reason.is_none() && perm.used_scene.is_none() => {}
                Some(_) => {
                    let first = self
                        .owners
                        .get(&owner_key)
                        .map_or("another crate", String::as_str)
                        .to_owned();
                    self.clashes.push(Clash {
                        kind: "harmonyos.permissions",
                        key: perm.name,
                        first,
                        second: owner.to_owned(),
                    });
                }
            }
        }
        Ok(())
    }

    fn merge_query_schemes(&mut self, owner: &str, at: &str, value: &Value) -> anyhow::Result<()> {
        let list = value
            .as_array()
            .with_context(|| format!("{owner}: {at} `harmonyos.querySchemes` must be an array"))?;
        for item in list {
            let raw = item.as_str().with_context(|| {
                format!("{owner}: {at} `harmonyos.querySchemes` entries must be strings")
            })?;
            let scheme = normalize_query_scheme(raw).with_context(|| {
                format!("{owner}: {at} `harmonyos.querySchemes`: invalid scheme {raw:?}")
            })?;
            if !self.meta.query_schemes.contains(&scheme) {
                self.meta.query_schemes.push(scheme);
            }
            if self.meta.query_schemes.len() > 50 {
                bail!(
                    "{owner}: {at} `harmonyos.querySchemes` exceeds HarmonyOS limit of 50 schemes"
                );
            }
        }
        Ok(())
    }

    fn merge_metadata(&mut self, owner: &str, at: &str, value: &Value) -> anyhow::Result<()> {
        let list = value
            .as_array()
            .with_context(|| format!("{owner}: {at} `harmonyos.metadata` must be an array"))?;
        for item in list {
            let obj = item.as_object().with_context(|| {
                format!("{owner}: {at} `harmonyos.metadata` entries must be tables")
            })?;
            let name = obj
                .get("name")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty() && s.len() <= 255 && !s.contains('\0'))
                .with_context(|| {
                    format!("{owner}: {at} `harmonyos.metadata` entry requires a valid non-empty `name` (<= 255 chars)")
                })?
                .to_owned();
            let val = match obj.get("value") {
                None => None,
                Some(v) => Some(
                    v.as_str()
                        .with_context(|| {
                            format!("{owner}: {at} `harmonyos.metadata` `{name}`: `value` must be a string")
                        })?
                        .to_owned(),
                ),
            };
            let resource = match obj.get("resource") {
                None => None,
                Some(v) => Some(
                    v.as_str()
                        .with_context(|| {
                            format!("{owner}: {at} `harmonyos.metadata` `{name}`: `resource` must be a string")
                        })?
                        .to_owned(),
                ),
            };
            if val.is_none() && resource.is_none() {
                bail!("{owner}: {at} `harmonyos.metadata` `{name}` must set `value` or `resource`");
            }
            for k in obj.keys() {
                if !matches!(k.as_str(), "name" | "value" | "resource") {
                    bail!("{owner}: {at} `harmonyos.metadata` `{name}`: unknown key `{k}`");
                }
            }
            let entry = MetadataItem {
                name: name.clone(),
                value: val,
                resource,
            };
            let owner_key = format!("harmonyos.metadata.{name}");
            match self.meta.metadata.iter().find(|m| m.name == name) {
                None => {
                    self.owners.insert(owner_key, owner.to_owned());
                    self.meta.metadata.push(entry);
                }
                Some(existing) if *existing == entry => {}
                Some(_) => {
                    let first = self
                        .owners
                        .get(&owner_key)
                        .map_or("another crate", String::as_str)
                        .to_owned();
                    self.clashes.push(Clash {
                        kind: "harmonyos.metadata",
                        key: name,
                        first,
                        second: owner.to_owned(),
                    });
                }
            }
        }
        Ok(())
    }

    fn merge_abilities(
        &mut self,
        owner: &str,
        dir: Option<&Path>,
        at: &str,
        value: &Value,
        extension: bool,
        is_app: bool,
    ) -> anyhow::Result<()> {
        let kind = if extension {
            "harmonyos.extensionAbilities"
        } else {
            "harmonyos.abilities"
        };
        let list = value
            .as_array()
            .with_context(|| format!("{owner}: {at} `{kind}` must be an array"))?;
        let target = if extension {
            &mut self.meta.extension_abilities
        } else {
            &mut self.meta.abilities
        };
        for item in list {
            let obj = item
                .as_object()
                .with_context(|| format!("{owner}: {at} `{kind}` entries must be tables"))?;
            let name = obj
                .get("name")
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty())
                .with_context(|| {
                    format!("{owner}: {at} `{kind}` entry requires a non-empty `name`")
                })?;
            if !extension && name == "EntryAbility" {
                bail!("{owner}: {at} `{kind}`: `EntryAbility` is owned by NSH");
            }
            let src_entry = obj
                .get("srcEntry")
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty())
                .with_context(|| {
                    format!("{owner}: {at} `{kind}` `{name}` requires a non-empty `srcEntry`")
                })?;
            let dest_rel = normalize_ets_entry(src_entry).with_context(|| {
                format!("{owner}: {at} `{kind}` `{name}`: invalid `srcEntry` {src_entry:?}")
            })?;
            if extension {
                let ext_type = obj
                    .get("type")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .with_context(|| {
                        format!("{owner}: {at} `{kind}` `{name}` requires a non-empty `type`")
                    })?;
                if ext_type == "dataShare"
                    && obj
                        .get("uri")
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .is_none()
                {
                    bail!(
                        "{owner}: {at} `{kind}` `{name}`: `dataShare` extensionAbility requires a non-empty `uri`"
                    );
                }
            }
            let mut cleaned = obj.clone();
            let staged_src = if let Some(raw_file) = cleaned.remove("srcFile") {
                let rel = raw_file
                    .as_str()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .with_context(|| {
                        format!(
                            "{owner}: {at} `{kind}` `{name}`: `srcFile` must be a non-empty path string"
                        )
                    })?;
                Some(resolve_local_file(
                    owner, at, kind, name, "srcFile", rel, dir, is_app,
                )?)
            } else {
                dir.and_then(|d| {
                    let direct = d.join(&dest_rel);
                    if direct.is_file() {
                        return Some(direct.canonicalize().unwrap_or(direct));
                    }
                    let under_ohos = d.join("ohos").join(&dest_rel);
                    if under_ohos.is_file() {
                        return Some(under_ohos.canonicalize().unwrap_or(under_ohos));
                    }
                    None
                })
            };

            let owner_key = format!("{kind}.{name}");
            match target
                .iter()
                .find(|a| a.get("name").and_then(Value::as_str) == Some(name))
            {
                None => {
                    self.owners.insert(owner_key, owner.to_owned());
                    target.push(cleaned);
                    if let Some(src_path) = staged_src {
                        self.meta.ability_sources.insert(dest_rel, src_path);
                    }
                }
                Some(existing) if existing == &cleaned => {
                    if let Some(src_path) = staged_src {
                        self.meta
                            .ability_sources
                            .entry(dest_rel)
                            .or_insert(src_path);
                    }
                }
                Some(_) => {
                    let first = self
                        .owners
                        .get(&owner_key)
                        .map_or("another crate", String::as_str)
                        .to_owned();
                    self.clashes.push(Clash {
                        kind,
                        key: name.to_owned(),
                        first,
                        second: owner.to_owned(),
                    });
                }
            }
        }
        Ok(())
    }

    fn merge_dependencies(
        &mut self,
        owner: &str,
        dir: Option<&Path>,
        at: &str,
        value: &Value,
        is_app: bool,
    ) -> anyhow::Result<()> {
        let table = value.as_object().with_context(|| {
            format!(
                "{owner}: {at} `harmonyos.dependencies` must be a table of package -> version/path"
            )
        })?;
        for (pkg, spec) in table {
            validate_ohpm_package_name(pkg)
                .with_context(|| format!("{owner}: {at} `harmonyos.dependencies`"))?;
            if matches!(
                pkg.as_str(),
                "@nsh/harmonyos" | "libnsh_platform_harmonyos.so"
            ) {
                bail!("{owner}: {at} `harmonyos.dependencies.{pkg}` is set by NSH itself");
            }
            let spec = spec
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .with_context(|| {
                    format!(
                        "{owner}: {at} `harmonyos.dependencies.{pkg}` must be a non-empty string"
                    )
                })?;
            let dep = parse_ohpm_dep(owner, at, pkg, spec, dir, is_app)?;
            let owner_key = format!("harmonyos.dependencies.{pkg}");
            match self.meta.dependencies.get(pkg) {
                None => {
                    self.owners.insert(owner_key, owner.to_owned());
                    self.meta.dependencies.insert(pkg.clone(), dep);
                }
                Some(existing) if *existing == dep => {}
                Some(_) => {
                    let first = self
                        .owners
                        .get(&owner_key)
                        .map_or("another crate", String::as_str)
                        .to_owned();
                    self.clashes.push(Clash {
                        kind: "harmonyos.dependencies",
                        key: pkg.clone(),
                        first,
                        second: owner.to_owned(),
                    });
                }
            }
        }
        Ok(())
    }

    fn merge_strings(
        &mut self,
        owner: &str,
        at: &str,
        value: &Value,
        is_app: bool,
    ) -> anyhow::Result<()> {
        let table = value.as_object().with_context(|| {
            format!("{owner}: {at} `harmonyos.strings` must be a table of name -> text")
        })?;
        for (key, text) in table {
            validate_resource_name(key)
                .with_context(|| format!("{owner}: {at} `harmonyos.strings`"))?;
            if key == "app_name"
                || (!is_app && (key == "nsh_microphone_reason" || key.starts_with("nsh_perm_")))
            {
                bail!("{owner}: {at} `harmonyos.strings.{key}` is a key NSH sets itself");
            }
            let text = text.as_str().with_context(|| {
                format!("{owner}: {at} `harmonyos.strings.{key}` must be a string")
            })?;
            let owner_key = format!("harmonyos.strings.{key}");
            match self.meta.strings.get(key) {
                None => {
                    self.owners.insert(owner_key, owner.to_owned());
                    self.meta.strings.insert(key.clone(), text.to_owned());
                }
                Some(existing) if existing == text => {}
                Some(_) => {
                    let first = self
                        .owners
                        .get(&owner_key)
                        .map_or("another crate", String::as_str)
                        .to_owned();
                    self.clashes.push(Clash {
                        kind: "harmonyos.strings",
                        key: key.clone(),
                        first,
                        second: owner.to_owned(),
                    });
                }
            }
        }
        Ok(())
    }
}

/// Deterministic `string.json` resource key for a permission's plain-text reason.
pub(crate) fn permission_reason_key(permission_name: &str) -> String {
    let suffix = permission_name
        .strip_prefix("ohos.permission.")
        .unwrap_or(permission_name);
    let mut out = String::from("nsh_perm_");
    for c in suffix.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else {
            out.push('_');
        }
    }
    out
}

fn parse_permission(
    owner: &str,
    at: &str,
    value: &Value,
    warnings: &mut Vec<String>,
) -> anyhow::Result<Permission> {
    if let Some(raw) = value.as_str() {
        let name = normalize_permission_name(owner, at, raw, warnings)?;
        return Ok(Permission {
            name,
            reason: None,
            used_scene: None,
        });
    }
    let obj = value.as_object().with_context(|| {
        format!("{owner}: {at} `harmonyos.permissions` entries must be strings or tables")
    })?;
    for k in obj.keys() {
        if !matches!(
            k.as_str(),
            "name" | "reason" | "usedScene" | "abilities" | "when"
        ) {
            bail!("{owner}: {at} `harmonyos.permissions`: unknown key `{k}`");
        }
    }
    let raw_name = obj
        .get("name")
        .and_then(Value::as_str)
        .with_context(|| format!("{owner}: {at} `harmonyos.permissions` entry requires `name`"))?;
    let name = normalize_permission_name(owner, at, raw_name, warnings)?;

    let reason = match obj.get("reason") {
        None => None,
        Some(v) => {
            let s = v
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .with_context(|| {
                    format!("{owner}: {at} `harmonyos.permissions` `{name}`: `reason` must be a non-empty string")
                })?;
            if let Some(res_key) = s.strip_prefix("$string:") {
                validate_resource_name(res_key).with_context(|| {
                    format!("{owner}: {at} `harmonyos.permissions` `{name}`: invalid `reason` resource {s:?}")
                })?;
            }
            Some(s.to_owned())
        }
    };

    let used_scene = parse_used_scene(owner, at, &name, obj, reason.is_some())?;
    Ok(Permission {
        name,
        reason,
        used_scene,
    })
}

fn parse_used_scene(
    owner: &str,
    at: &str,
    name: &str,
    obj: &Map<String, Value>,
    has_reason: bool,
) -> anyhow::Result<Option<UsedScene>> {
    let scene_obj = match obj.get("usedScene") {
        None => None,
        Some(v) => Some(v.as_object().with_context(|| {
            format!("{owner}: {at} `harmonyos.permissions` `{name}`: `usedScene` must be a table")
        })?),
    };
    if scene_obj.is_some() && (obj.contains_key("abilities") || obj.contains_key("when")) {
        bail!(
            "{owner}: {at} `harmonyos.permissions` `{name}`: use either `usedScene` or top-level `abilities`/`when`, not both"
        );
    }
    let abilities_val = scene_obj
        .and_then(|s| s.get("abilities"))
        .or_else(|| obj.get("abilities"));
    let when_val = scene_obj
        .and_then(|s| s.get("when"))
        .or_else(|| obj.get("when"));

    if scene_obj.is_none() && abilities_val.is_none() && when_val.is_none() {
        return Ok(has_reason.then(|| UsedScene {
            abilities: vec!["EntryAbility".to_owned()],
            when: "inuse".to_owned(),
        }));
    }
    if !has_reason {
        bail!(
            "{owner}: {at} `harmonyos.permissions` `{name}`: `usedScene` / `abilities` / `when` requires a `reason`"
        );
    }

    if let Some(s) = scene_obj {
        for k in s.keys() {
            if !matches!(k.as_str(), "abilities" | "when") {
                bail!(
                    "{owner}: {at} `harmonyos.permissions` `{name}`: unknown `usedScene` key `{k}`"
                );
            }
        }
    }

    let raw_abilities = match abilities_val {
        None => vec!["EntryAbility".to_owned()],
        Some(v) => v
            .as_array()
            .and_then(|a| {
                a.iter()
                    .map(|item| {
                        item.as_str()
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                            .map(str::to_owned)
                    })
                    .collect::<Option<Vec<_>>>()
            })
            .filter(|list| !list.is_empty())
            .with_context(|| {
                format!("{owner}: {at} `harmonyos.permissions` `{name}`: `abilities` must be a non-empty list of strings")
            })?,
    };
    let mut abilities = Vec::with_capacity(raw_abilities.len());
    for ability in raw_abilities {
        if !abilities.contains(&ability) {
            abilities.push(ability);
        }
    }
    let when = match when_val {
        None => "inuse".to_owned(),
        Some(v) => {
            let w = v.as_str().with_context(|| {
                format!("{owner}: {at} `harmonyos.permissions` `{name}`: `when` must be \"inuse\" or \"always\"")
            })?;
            if !matches!(w, "inuse" | "always") {
                bail!(
                    "{owner}: {at} `harmonyos.permissions` `{name}`: `when` must be \"inuse\" or \"always\", not {w:?}"
                );
            }
            w.to_owned()
        }
    };
    Ok(Some(UsedScene { abilities, when }))
}

fn normalize_permission_name(
    owner: &str,
    at: &str,
    raw: &str,
    warnings: &mut Vec<String>,
) -> anyhow::Result<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty()
        || trimmed.starts_with('.')
        || trimmed.ends_with('.')
        || trimmed.contains("..")
        || !trimmed
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_')
    {
        bail!("{owner}: {at} `harmonyos.permissions`: invalid permission name {raw:?}");
    }
    if trimmed.contains('.') {
        Ok(trimmed.to_owned())
    } else {
        if trimmed.chars().any(|c| c.is_ascii_lowercase()) {
            warnings.push(format!(
                "{owner}: {at} `harmonyos.permissions`: bare permission `{trimmed}` contains lowercase letters (expected e.g. `CAMERA` or `ohos.permission.CAMERA`)"
            ));
        }
        Ok(format!("ohos.permission.{trimmed}"))
    }
}

fn normalize_query_scheme(raw: &str) -> anyhow::Result<String> {
    let trimmed = raw.trim();
    if trimmed.len() > 128 {
        bail!("scheme exceeds 128 characters");
    }
    let mut chars = trimmed.chars();
    let valid = chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'));
    if !valid {
        bail!(
            "scheme must start with an ASCII letter and contain only letters, digits, `+`, `-`, `.`"
        );
    }
    Ok(trimmed.to_ascii_lowercase())
}

fn valid_ohpm_segment(seg: &str) -> bool {
    let mut bytes = seg.bytes();
    bytes.next().is_some_and(|b| b.is_ascii_alphanumeric())
        && bytes.all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        && !seg.contains("..")
}

fn validate_ohpm_package_name(name: &str) -> anyhow::Result<()> {
    if name.is_empty()
        || name.len() > 214
        || name.contains('\0')
        || name.contains('\\')
        || name.contains("..")
        || name.starts_with('/')
        || name.ends_with('/')
    {
        bail!("invalid ohpm package name {name:?}");
    }
    let slashes = name.matches('/').count();
    if let Some(rest) = name.strip_prefix('@') {
        if slashes != 1 {
            bail!("scoped ohpm package name {name:?} must be `@scope/name`");
        }
        let (scope, pkg) = rest.split_once('/').unwrap();
        if !valid_ohpm_segment(scope) || !valid_ohpm_segment(pkg) {
            bail!("invalid scoped ohpm package name {name:?}");
        }
    } else if slashes > 0 || !valid_ohpm_segment(name) {
        bail!("invalid ohpm package name {name:?}");
    }
    Ok(())
}

fn normalize_ets_entry(src_entry: &str) -> anyhow::Result<String> {
    let trimmed = src_entry.trim();
    if trimmed.is_empty() || trimmed.contains('\0') || trimmed.contains('\\') {
        bail!("must be a clean `./ets/.../*.ets` path");
    }
    let rel = trimmed.strip_prefix("./").unwrap_or(trimmed);
    if !rel.starts_with("ets/") || !rel.ends_with(".ets") {
        bail!("must point to an `.ets` file under `./ets/`");
    }
    let path = Path::new(rel);
    if path.is_absolute()
        || path.components().any(|c| {
            matches!(
                c,
                std::path::Component::ParentDir
                    | std::path::Component::RootDir
                    | std::path::Component::Prefix(_)
            )
        })
    {
        bail!("must not escape `entry/src/main/ets/`");
    }
    Ok(rel.to_owned())
}

#[allow(clippy::too_many_arguments)]
fn resolve_local_file(
    owner: &str,
    at: &str,
    kind: &str,
    name: &str,
    field: &str,
    rel: &str,
    base_dir: Option<&Path>,
    is_app: bool,
) -> anyhow::Result<PathBuf> {
    if rel.contains('\0') {
        bail!("{owner}: {at} `{kind}` `{name}`: invalid `{field}`");
    }
    let rel_path = Path::new(rel);
    if !is_app
        && (rel_path.is_absolute()
            || rel_path.components().any(|c| {
                matches!(
                    c,
                    std::path::Component::ParentDir
                        | std::path::Component::RootDir
                        | std::path::Component::Prefix(_)
                )
            }))
    {
        bail!(
            "{owner}: {at} `{kind}` `{name}`: `{field}` ({rel:?}) must be a relative path inside the crate directory"
        );
    }
    let path = if rel_path.is_absolute() {
        rel_path.to_path_buf()
    } else {
        let dir = base_dir.with_context(|| {
            format!("{owner}: {at} `{kind}` `{name}`: no base directory for `{field}` {rel:?}")
        })?;
        dir.join(rel_path)
    };
    if let Some(dir) = base_dir
        && dir.exists()
    {
        if !path.is_file() {
            bail!(
                "{owner}: {at} `{kind}` `{name}`: `{field}` not found at {}",
                path.display()
            );
        }
        let resolved = path.canonicalize().unwrap_or(path);
        if !is_app {
            let root = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
            if !resolved.starts_with(&root) {
                bail!(
                    "{owner}: {at} `{kind}` `{name}`: `{field}` resolves outside {}",
                    root.display()
                );
            }
        }
        return Ok(resolved);
    }
    Ok(path.canonicalize().unwrap_or(path))
}

fn validate_resource_name(name: &str) -> anyhow::Result<()> {
    let mut chars = name.chars();
    let valid = chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_');
    if !valid {
        bail!("invalid HarmonyOS string resource name {name:?}");
    }
    Ok(())
}

fn parse_ohpm_dep(
    owner: &str,
    at: &str,
    pkg: &str,
    spec: &str,
    base_dir: Option<&Path>,
    is_app: bool,
) -> anyhow::Result<OhpmDep> {
    let local_rel = if let Some(rest) = spec.strip_prefix("file:") {
        Some(rest)
    } else if spec.starts_with("./")
        || spec.starts_with("../")
        || spec.ends_with(".har")
        || spec.ends_with(".tgz")
    {
        Some(spec)
    } else {
        None
    };
    if let Some(rel) = local_rel {
        if rel.is_empty() || rel.contains('\0') {
            bail!("{owner}: {at} `harmonyos.dependencies.{pkg}`: empty file path");
        }
        let rel_path = Path::new(rel);
        if !is_app
            && (rel_path.is_absolute()
                || rel_path.components().any(|c| {
                    matches!(
                        c,
                        std::path::Component::ParentDir
                            | std::path::Component::RootDir
                            | std::path::Component::Prefix(_)
                    )
                }))
        {
            bail!(
                "{owner}: {at} `harmonyos.dependencies.{pkg}`: local path {rel:?} must stay inside the crate directory"
            );
        }
        let path = if rel_path.is_absolute() {
            PathBuf::from(rel)
        } else {
            let dir = base_dir.with_context(|| {
                format!("{owner}: {at} `harmonyos.dependencies.{pkg}`: no base directory for local path {rel:?}")
            })?;
            dir.join(rel_path)
        };
        if let Some(dir) = base_dir
            && dir.exists()
        {
            if !path.exists() {
                bail!(
                    "{owner}: {at} `harmonyos.dependencies.{pkg}`: local package not found at {}",
                    path.display()
                );
            }
            let resolved = path.canonicalize().unwrap_or(path);
            if !is_app {
                let root = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
                if !resolved.starts_with(&root) {
                    bail!(
                        "{owner}: {at} `harmonyos.dependencies.{pkg}`: local path resolves outside {}",
                        root.display()
                    );
                }
            }
            return Ok(OhpmDep::LocalPath(resolved));
        }
        let resolved = path.canonicalize().unwrap_or(path);
        Ok(OhpmDep::LocalPath(resolved))
    } else {
        Ok(OhpmDep::Version(spec.to_owned()))
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use serde_json::json;

    use super::*;

    fn metadata(packages: &[(&str, &str, Value, &[&str])]) -> Value {
        let id = |name: &str| format!("path+file:///work/{name}#0.1.0");
        json!({
            "packages": packages.iter().map(|(name, manifest, metadata, _)| json!({
                "id": id(name),
                "name": name,
                "manifest_path": manifest,
                "metadata": metadata,
            })).collect::<Vec<_>>(),
            "resolve": { "nodes": packages.iter().map(|(name, _, _, features)| json!({
                "id": id(name),
                "features": features,
            })).collect::<Vec<_>>() },
        })
    }

    #[test]
    fn merges_only_enabled_features_and_ignores_other_platforms() {
        let extra = json!({
            "nsh": {
                "ios": { "infoPlist": { "NSCameraUsageDescription": "iOS camera" } },
                "android": { "permissions": ["CAMERA"] },
                "features": {
                    "camera": {
                        "harmonyos": {
                            "permissions": [
                                { "name": "CAMERA", "reason": "Take photos with the camera." }
                            ],
                            "querySchemes": ["Weixin"]
                        }
                    },
                    "push": {
                        "harmonyos": {
                            "metadata": [{ "name": "client_id", "value": "999" }],
                            "dependencies": { "@hms/push": "^1.0.0" }
                        }
                    }
                }
            }
        });
        let (on, warnings) = ModuleMeta::from_metadata(&metadata(&[(
            "nsh-extra",
            "/work/nsh-extra/Cargo.toml",
            extra.clone(),
            &["camera"],
        )]))
        .unwrap();
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(on.meta.permissions.len(), 1);
        assert_eq!(on.meta.permissions[0].name, "ohos.permission.CAMERA");
        assert_eq!(
            on.meta.permissions[0].reason.as_deref(),
            Some("Take photos with the camera.")
        );
        assert_eq!(
            on.meta.permissions[0].used_scene,
            Some(UsedScene {
                abilities: vec!["EntryAbility".into()],
                when: "inuse".into(),
            })
        );
        assert_eq!(on.meta.query_schemes, vec!["weixin"]);
        assert!(on.meta.metadata.is_empty());
        assert!(on.meta.dependencies.is_empty());

        let (off, _) = ModuleMeta::from_metadata(&metadata(&[(
            "nsh-extra",
            "/work/nsh-extra/Cargo.toml",
            extra,
            &[],
        )]))
        .unwrap();
        assert_eq!(off.meta, HarmonyMeta::default());
    }

    #[test]
    fn clashes_fail_until_settled_by_app_nsh_json() {
        let a = json!({
            "nsh": {
                "harmonyos": {
                    "permissions": [{ "name": "CAMERA", "reason": "Crate A reason" }],
                    "metadata": [{ "name": "client_id", "value": "aaa" }],
                    "dependencies": { "@ohos/lottie": "^2.0.0" }
                }
            }
        });
        let b = json!({
            "nsh": {
                "harmonyos": {
                    "permissions": [{ "name": "ohos.permission.CAMERA", "reason": "Crate B reason" }],
                    "metadata": [{ "name": "client_id", "value": "bbb" }],
                    "dependencies": { "@ohos/lottie": "^3.0.0" }
                }
            }
        });
        let (meta, _) = ModuleMeta::from_metadata(&metadata(&[
            ("crate-a", "/work/a/Cargo.toml", a, &[]),
            ("crate-b", "/work/b/Cargo.toml", b, &[]),
        ]))
        .unwrap();

        let err = meta
            .apply(&mut HarmonyMeta::default())
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("crate-a and crate-b") && err.contains("ohos.permission.CAMERA"),
            "{err}"
        );

        let mut app = HarmonyMeta::from_app_json(
            Path::new("/app"),
            &json!({
                "harmonyos": {
                    "permissions": [{ "name": "CAMERA", "reason": "App camera reason" }],
                    "metadata": [{ "name": "client_id", "value": "app-id" }],
                    "dependencies": { "@ohos/lottie": "^2.1.0" }
                }
            }),
        )
        .unwrap();
        meta.apply(&mut app).unwrap();
        assert_eq!(
            app.permissions[0].reason.as_deref(),
            Some("App camera reason")
        );
        assert_eq!(app.metadata[0].value.as_deref(), Some("app-id"));
        assert_eq!(
            app.dependencies.get("@ohos/lottie"),
            Some(&OhpmDep::Version("^2.1.0".into()))
        );
    }

    #[test]
    fn validates_reserved_names_and_bad_entries() {
        let run = |hm: Value| {
            ModuleMeta::from_metadata(&metadata(&[(
                "bad",
                "/work/bad/Cargo.toml",
                json!({ "nsh": { "harmonyos": hm } }),
                &[],
            )]))
        };
        assert!(
            run(json!({ "abilities": [{ "name": "EntryAbility", "srcEntry": "./a.ets" }] }))
                .is_err()
        );
        assert!(run(json!({ "dependencies": { "@nsh/harmonyos": "^1.0.0" } })).is_err());
        assert!(
            run(json!({ "dependencies": { "libnsh_platform_harmonyos.so": "^1.0.0" } })).is_err()
        );
        assert!(run(json!({ "strings": { "app_name": "hack" } })).is_err());
        assert!(run(json!({ "strings": { "nsh_microphone_reason": "hack" } })).is_err());
        assert!(run(json!({ "permissions": ["bad perm!"] })).is_err());
        assert!(run(json!({ "permissions": [{ "name": "CAMERA", "when": "never" }] })).is_err());
        assert!(run(json!({ "querySchemes": ["123bad"] })).is_err());

        let (ok, warnings) = run(json!({ "permissions": ["camera"], "futureKey": true })).unwrap();
        assert_eq!(ok.meta.permissions[0].name, "ohos.permission.camera");
        assert_eq!(warnings.len(), 2, "{warnings:?}");
    }

    #[test]
    fn validates_strict_metadata_rules_and_stages_ability_sources() {
        let dir = std::env::temp_dir().join(format!("nsh-hm-meta-strict-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let echo_ets = dir.join("EchoAbility.ets");
        fs::write(&echo_ets, "export default class EchoAbility {}").unwrap();

        let run = |hm: Value| {
            ModuleMeta::from_metadata(&metadata(&[(
                "bad",
                "/work/bad/Cargo.toml",
                json!({ "nsh": { "harmonyos": hm } }),
                &[],
            )]))
        };
        // usedScene without reason is rejected by HarmonyOS.
        assert!(run(json!({ "permissions": [{ "name": "CAMERA", "when": "inuse" }] })).is_err());
        // Crates cannot hijack generated nsh_perm_* string keys.
        assert!(run(json!({ "strings": { "nsh_perm_camera": "hijack" } })).is_err());
        // extensionAbilities require type, and dataShare requires uri.
        assert!(
            run(json!({ "extensionAbilities": [{ "name": "ExtA", "srcEntry": "./ets/a/ExtA.ets" }] }))
                .is_err()
        );
        assert!(
            run(json!({ "extensionAbilities": [{ "name": "DsA", "srcEntry": "./ets/a/DsA.ets", "type": "dataShare" }] }))
                .is_err()
        );
        // srcEntry must stay under ./ets/*.ets.
        assert!(
            run(json!({ "abilities": [{ "name": "BadAb", "srcEntry": "./ets/../escape.ets" }] }))
                .is_err()
        );
        // Crate dependencies cannot traverse outside the crate.
        assert!(run(json!({ "dependencies": { "@acme/x": "file:../outside" } })).is_err());
        // querySchemes limit (>50).
        let too_many: Vec<String> = (0..51).map(|i| format!("scheme{i}")).collect();
        assert!(run(json!({ "querySchemes": too_many })).is_err());
        // Intra-nsh.json permission clash fails.
        assert!(
            HarmonyMeta::from_app_json(
                &dir,
                &json!({
                    "harmonyos": {
                        "permissions": [
                            { "name": "CAMERA", "reason": "One" },
                            { "name": "CAMERA", "reason": "Two" }
                        ]
                    }
                })
            )
            .is_err()
        );
        // srcFile on app ability is resolved and recorded in ability_sources.
        let app = HarmonyMeta::from_app_json(
            &dir,
            &json!({
                "harmonyos": {
                    "abilities": [{
                        "name": "EchoAbility",
                        "srcEntry": "./ets/echo/EchoAbility.ets",
                        "srcFile": "EchoAbility.ets"
                    }]
                }
            }),
        )
        .unwrap();
        assert_eq!(
            app.ability_sources.get("ets/echo/EchoAbility.ets"),
            Some(&echo_ets.canonicalize().unwrap())
        );
        assert!(app.abilities[0].get("srcFile").is_none());

        fs::remove_dir_all(&dir).unwrap();
    }
}
